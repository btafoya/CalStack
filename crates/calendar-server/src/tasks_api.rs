//! Task (VTODO) JSON API — docs/TASKS_JOURNALS_DESIGN.md section 7.
//! Views never expose `extra_props` (it can carry private client data).

use crate::{AppError, AppState, calendars_api::require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use calendar_db::{self as db, scheduling::SubjectKind};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct TaskBody {
    uid: Option<String>,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    location: Option<String>,
    starts_at: Option<DateTime<Utc>>,
    start_date: Option<NaiveDate>,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    duration_secs: Option<i64>,
    tzid: Option<String>,
    rrule: Option<String>,
    #[schema(value_type = Object)]
    rdate: Option<serde_json::Value>,
    #[schema(value_type = Object)]
    exdate: Option<serde_json::Value>,
    status: Option<String>,
    percent_complete: Option<i16>,
    priority: Option<i16>,
    class: Option<String>,
    categories: Option<Vec<String>>,
    parent_uid: Option<String>,
    sort_order: Option<i64>,
    #[schema(value_type = Vec<Object>)]
    attendees: Option<Vec<db::NewAttendee>>,
    alarms: Option<Vec<AlarmBody>>,
}

#[derive(serde::Deserialize, Clone, utoipa::ToSchema)]
struct AlarmBody {
    action: String,
    related: Option<String>,
    offset_secs: Option<i64>,
    trigger_at: Option<DateTime<Utc>>,
    description: Option<String>,
    summary: Option<String>,
    recipient_emails: Option<Vec<String>>,
    notify_channels: Option<Vec<String>>,
}

impl AlarmBody {
    fn into_db(self) -> Result<db::alarms::NewAlarm, AppError> {
        if !matches!(self.action.as_str(), "DISPLAY" | "EMAIL") {
            return Err(AppError::bad_request(
                "alarm action must be DISPLAY or EMAIL",
            ));
        }
        if let Some(related) = &self.related
            && !matches!(related.as_str(), "START" | "END")
        {
            return Err(AppError::bad_request("alarm related must be START or END"));
        }
        Ok(db::alarms::NewAlarm {
            action: self.action,
            related: self.related,
            offset_secs: self.offset_secs,
            trigger_at: self.trigger_at,
            description: self.description,
            summary: self.summary,
            recipient_emails: self.recipient_emails.unwrap_or_default(),
            notify_channels: self.notify_channels.unwrap_or_default(),
        })
    }
}

fn validate_task_body(body: &TaskBody) -> Result<(), AppError> {
    if body.starts_at.is_some() && body.start_date.is_some() {
        return Err(AppError::bad_request(
            "starts_at and start_date are mutually exclusive",
        ));
    }
    if body.due_at.is_some() && body.due_date.is_some() {
        return Err(AppError::bad_request(
            "due_at and due_date are mutually exclusive",
        ));
    }
    if let Some(p) = body.priority
        && !(0..=9).contains(&p)
    {
        return Err(AppError::bad_request("priority must be 0..=9"));
    }
    if let Some(p) = body.percent_complete
        && !(0..=100).contains(&p)
    {
        return Err(AppError::bad_request("percent_complete must be 0..=100"));
    }
    if let Some(s) = &body.status
        && !matches!(
            s.as_str(),
            "NEEDS-ACTION" | "IN-PROCESS" | "COMPLETED" | "CANCELLED"
        )
    {
        return Err(AppError::bad_request(
            "status must be NEEDS-ACTION, IN-PROCESS, COMPLETED or CANCELLED",
        ));
    }
    if let Some(c) = &body.class
        && !matches!(c.as_str(), "PUBLIC" | "PRIVATE" | "CONFIDENTIAL")
    {
        return Err(AppError::bad_request(
            "class must be PUBLIC, PRIVATE or CONFIDENTIAL",
        ));
    }
    for a in body.attendees.iter().flatten() {
        if a.email.is_none() && a.telephone.is_none() {
            return Err(AppError::bad_request(
                "attendee needs an email or a telephone",
            ));
        }
        if let Some(email) = &a.email {
            calendar_core::validate_email(email)
                .map_err(|e| AppError::bad_request(e.to_string()))?;
        }
    }
    Ok(())
}

fn validate_task_patch(body: &TaskBody) -> Result<(), AppError> {
    if body.duration_secs.is_some() && (body.due_at.is_some() || body.due_date.is_some()) {
        return Err(AppError::bad_request(
            "duration_secs and due are mutually exclusive",
        ));
    }
    validate_task_body(body)
}

/// The task's due instant; floating/all-day dues compare as UTC wall clock
/// (the design's documented floating limitation).
pub(crate) fn due_instant(task: &db::tasks::TaskRow) -> Option<DateTime<Utc>> {
    task.due_at.or_else(|| {
        task.due_date
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|n| n.and_utc())
    })
}

pub(crate) fn is_overdue(task: &db::tasks::TaskRow) -> bool {
    if task.completed_at.is_some()
        || matches!(
            task.status.as_deref(),
            Some("COMPLETED") | Some("CANCELLED")
        )
    {
        return false;
    }
    due_instant(task)
        .map(|due| due < Utc::now())
        .unwrap_or(false)
}

/// Live subtask counts keyed by parent UID — one aggregate query for a list.
async fn subtask_counts(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    uids: &[String],
) -> Result<HashMap<String, i64>, AppError> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT parent_uid, COUNT(*)::bigint FROM tasks
         WHERE calendar_id = $1 AND parent_uid = ANY($2)
           AND deleted_at IS NULL AND master_task_id IS NULL
         GROUP BY parent_uid",
    )
    .bind(calendar_id)
    .bind(uids)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(rows
        .into_iter()
        .filter_map(|(uid, n)| uid.map(|u| (u, n)))
        .collect())
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct AlarmView {
    id: Uuid,
    action: String,
    related: Option<String>,
    offset_secs: Option<i64>,
    trigger_at: Option<DateTime<Utc>>,
    description: Option<String>,
    summary: Option<String>,
    recipient_emails: Vec<String>,
    notify_channels: Vec<String>,
}

fn alarms_view(alarms: &[db::tasks::TaskAlarmRow]) -> Vec<AlarmView> {
    alarms
        .iter()
        .map(|a| AlarmView {
            id: a.id,
            action: a.action.clone(),
            related: a.related.clone(),
            offset_secs: a.offset_secs(),
            trigger_at: a.trigger_at,
            description: a.description.clone(),
            summary: a.summary.clone(),
            recipient_emails: a.recipient_emails.clone(),
            notify_channels: a.notify_channels.clone(),
        })
        .collect()
}

/// Task view (design section 7): the modelled row plus subtasks_count,
/// next_open and is_overdue. Never includes extra_props.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct TaskView {
    id: Uuid,
    calendar_id: Uuid,
    uid: String,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    location: Option<String>,
    starts_at: Option<DateTime<Utc>>,
    start_date: Option<NaiveDate>,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    duration_secs: Option<i64>,
    tzid: Option<String>,
    floating: bool,
    completed_at: Option<DateTime<Utc>>,
    rrule: Option<String>,
    #[schema(value_type = Object)]
    rdate: serde_json::Value,
    #[schema(value_type = Object)]
    exdate: serde_json::Value,
    status: Option<String>,
    percent_complete: Option<i16>,
    priority: Option<i16>,
    class: Option<String>,
    categories: Vec<String>,
    parent_uid: Option<String>,
    sort_order: Option<i64>,
    organizer_email: Option<String>,
    organizer_name: Option<String>,
    sequence: i32,
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    attendees: Vec<crate::AttendeeView>,
    alarms: Vec<AlarmView>,
    subtasks_count: i64,
    next_open: Option<DateTime<Utc>>,
    is_overdue: bool,
}

pub(crate) fn task_view(
    task: &db::tasks::TaskRow,
    etag: &str,
    attendees: &[db::tasks::TaskAttendeeRow],
    alarms: &[db::tasks::TaskAlarmRow],
    next_open: Option<DateTime<Utc>>,
    subtasks_count: i64,
) -> TaskView {
    TaskView {
        id: task.id,
        calendar_id: task.calendar_id,
        uid: task.uid.clone(),
        summary: task.summary.clone(),
        description_html: task.description_html.clone(),
        description_text: task.description_text.clone(),
        url: task.url.clone(),
        location: task.location.clone(),
        starts_at: task.starts_at,
        start_date: task.start_date,
        due_at: task.due_at,
        due_date: task.due_date,
        duration_secs: task.duration.as_ref().map(|i| i.microseconds / 1_000_000),
        tzid: task.tzid.clone(),
        floating: task.floating,
        completed_at: task.completed_at,
        rrule: task.rrule.clone(),
        rdate: task.rdate.clone(),
        exdate: task.exdate.clone(),
        status: task.status.clone(),
        percent_complete: task.percent_complete,
        priority: task.priority,
        class: task.class.clone(),
        categories: task.categories.clone(),
        parent_uid: task.parent_uid.clone(),
        sort_order: task.sort_order,
        organizer_email: task.organizer_email.clone(),
        organizer_name: task.organizer_name.clone(),
        sequence: task.sequence,
        etag: etag.to_string(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        attendees: attendees
            .iter()
            .map(|a| crate::AttendeeView {
                email: a.email.clone(),
                display_name: a.display_name.clone(),
                telephone: a.telephone.clone(),
                role: a.role.clone(),
                partstat: a.partstat.clone(),
                rsvp: a.rsvp,
                contact_id: a.contact_id,
                user_id: a.user_id,
            })
            .collect(),
        alarms: alarms_view(alarms),
        subtasks_count,
        next_open,
        is_overdue: is_overdue(task),
    }
}

#[utoipa::path(
    post,
    path = "/api/calendars/{id}/tasks",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body = TaskBody,
    responses(
        (status = 201, description = "created (VTODO master)", body = TaskView),
        (status = 400, description = "validation error"),
        (status = 404, description = "absent"),
    )
)]
async fn create_task(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<TaskBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    validate_task_body(&body)?;
    let attendees = body.attendees.clone().unwrap_or_default();
    let alarms = body
        .alarms
        .iter()
        .flatten()
        .map(|a| a.clone().into_db())
        .collect::<Result<Vec<_>, _>>()?;
    let uid = body
        .uid
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let data = db::tasks::NewTaskData {
        uid,
        summary: body.summary.clone(),
        description_html: body
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: body.description_text.clone(),
        url: body.url.clone(),
        location: body.location.clone(),
        starts_at: body.starts_at,
        start_date: body.start_date,
        due_at: body.due_at,
        due_date: body.due_date,
        duration_secs: body.duration_secs,
        tzid: body.tzid.clone(),
        rrule: body.rrule.clone(),
        rdate: body.rdate.clone(),
        exdate: body.exdate.clone(),
        status: body.status.clone(),
        percent_complete: body.percent_complete,
        priority: body.priority,
        class: body.class.clone(),
        categories: body.categories.clone().unwrap_or_default(),
        parent_uid: body.parent_uid.clone(),
        sort_order: body.sort_order,
        organizer_user_id: Some(auth.user.id),
        organizer_email: Some(auth.user.email.clone()),
        organizer_name: auth.user.display_name.clone(),
        ..Default::default()
    };
    let (task, etag) =
        db::tasks::create_task(&pool, calendar_id, auth.user.id, &attendees, &alarms, &data)
            .await?;
    // Scheduling dispatch (stage 8c): copies for internal assignees, REQUEST
    // intents for external ones.
    db::scheduling::dispatch(&pool, SubjectKind::Task, task.id, auth.user.id).await?;
    let next_open = if task.rrule.is_some() {
        db::tasks::next_open(&pool, task.id, 90).await?
    } else {
        None
    };
    fire_task_hooks(
        &pool,
        calendar_id,
        task.id,
        "task_created",
        crypto.as_deref(),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(task_view(
            &task,
            &etag,
            &db::tasks::list_task_attendees(&pool, task.id).await?,
            &db::tasks::list_task_alarms(&pool, task.id).await?,
            next_open,
            0,
        )),
    ))
}

/// Rules + webhook triggers for task mutations. Fire-and-forget like the
/// event paths: rules must never break the mutation.
async fn fire_task_hooks(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    task_id: Uuid,
    trigger: &str,
    crypto: Option<&calendar_auth::Crypto>,
) {
    let Ok(cal) = db::get_calendar(pool, calendar_id).await else {
        return;
    };
    let summary: Option<String> = db::tasks::get_task(pool, task_id)
        .await
        .ok()
        .map(|(t, _)| t.summary);
    crate::rules_api::run_rules(
        pool,
        cal.tenant_id,
        cal.id,
        trigger,
        task_id,
        serde_json::json!({"summary": summary, "kind": "task"}),
        crypto,
    )
    .await;
    crate::webhooks_api::fire(pool, cal.tenant_id, task_id, trigger).await;
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct TaskListQuery {
    status: Option<String>,
    due_before: Option<DateTime<Utc>>,
    due_after: Option<DateTime<Utc>>,
    category: Option<String>,
    /// The parent task's uid (subtasks chain by parent_uid).
    parent_id: Option<String>,
    /// Case-insensitive substring over the summary.
    q: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}/tasks",
    params(
        ("id" = Uuid, Path, description = "calendar id"),
        ("status" = Option<String>, Query, description = "filter by status"),
        ("due_before" = Option<DateTime<Utc>>, Query),
        ("due_after" = Option<DateTime<Utc>>, Query),
        ("category" = Option<String>, Query, description = "category slug"),
        ("parent_id" = Option<String>, Query, description = "the parent task's uid (subtasks chain by parent_uid)"),
        ("q" = Option<String>, Query, description = "case-insensitive substring over the summary"),
    ),
    responses(
        (status = 200, description = "task masters (undated included)", body = Vec<TaskView>),
        (status = 404, description = "absent"),
    )
)]
async fn list_tasks(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<TaskListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let filter = db::tasks::TaskFilter {
        status: query.status,
        due_before: query.due_before,
        due_after: query.due_after,
        category: query.category,
        parent: query.parent_id,
    };
    let rows = db::tasks::list_tasks(&pool, calendar_id, &filter).await?;
    let rows = match query.q.as_deref() {
        // TaskFilter has no summary search; filter the (small) list here.
        Some(q) => {
            let needle = q.to_lowercase();
            rows.into_iter()
                .filter(|t| t.summary.to_lowercase().contains(&needle))
                .collect::<Vec<_>>()
        }
        None => rows,
    };
    let counts = subtask_counts(
        &pool,
        calendar_id,
        &rows.iter().map(|t| t.uid.clone()).collect::<Vec<_>>(),
    )
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for t in &rows {
        // List views skip per-row attendees/alarms (same as events).
        let next_open = if t.rrule.is_some() {
            db::tasks::next_open(&pool, t.id, 90).await.unwrap_or(None)
        } else {
            None
        };
        out.push(task_view(
            t,
            &t.etag,
            &[],
            &[],
            next_open,
            counts.get(&t.uid).copied().unwrap_or(0),
        ));
    }
    Ok(Json(out))
}

/// If-Match header extractor; None when absent. (Same shape as events'.)
pub(crate) struct IfMatch(pub(crate) Option<String>);

impl axum::extract::FromRequestParts<AppState> for IfMatch {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get("if-match")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string()),
        ))
    }
}

#[utoipa::path(
    get,
    path = "/api/tasks/{id}",
    params(("id" = Uuid, Path, description = "task id")),
    responses(
        (status = 200, description = "task with its ETag", body = TaskView),
        (status = 404, description = "absent"),
    )
)]
async fn get_task(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let (task, etag) = db::tasks::get_task(&pool, task_id).await?;
    require_capability(
        &pool,
        task.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let (next_open, subtasks_count) = task_extras(&pool, &task).await?;
    Ok(Json(task_view(
        &task,
        &etag,
        &db::tasks::list_task_attendees(&pool, task.id).await?,
        &db::tasks::list_task_alarms(&pool, task.id).await?,
        next_open,
        subtasks_count,
    )))
}

/// next_open and subtask count for a single task.
async fn task_extras(
    pool: &sqlx::PgPool,
    task: &db::tasks::TaskRow,
) -> Result<(Option<DateTime<Utc>>, i64), AppError> {
    let next_open = if task.rrule.is_some() {
        db::tasks::next_open(pool, task.id, 90)
            .await
            .unwrap_or(None)
    } else {
        None
    };
    let subtasks_count = if task.master_task_id.is_none() {
        db::tasks::list_subtasks(pool, task.calendar_id, &task.uid)
            .await?
            .len() as i64
    } else {
        0
    };
    Ok((next_open, subtasks_count))
}

#[utoipa::path(
    patch,
    path = "/api/tasks/{id}",
    params(
        ("id" = Uuid, Path, description = "task id"),
        ("if-match" = Option<String>, Header, description = "ETag for optimistic concurrency (412 on mismatch)"),
    ),
    request_body = TaskBody,
    responses(
        (status = 200, description = "updated", body = TaskView),
        (status = 400, description = "validation error or ETag mismatch"),
        (status = 404, description = "absent"),
    )
)]
async fn patch_task(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    if_match: IfMatch,
    Json(body): Json<TaskBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::tasks::get_task(&pool, task_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    validate_task_patch(&body)?;
    // A delivered copy accepts only the assignee's own PARTSTAT; everything
    // else is discarded (the organizer's next dispatch rebuilds the copy).
    if let Some(origin_id) = db::scheduling::origin_of(&pool, SubjectKind::Task, task_id).await? {
        let email = db::scheduling::attendee_email_for_user(
            &pool,
            SubjectKind::Task,
            task_id,
            auth.user.id,
        )
        .await?
        .ok_or(AppError::Forbidden)?;
        let partstat = body
            .attendees
            .unwrap_or_default()
            .into_iter()
            .find(|a| {
                a.user_id == Some(auth.user.id)
                    || a.email
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case(&auth.user.email))
            })
            .and_then(|a| a.partstat)
            .ok_or_else(|| {
                AppError::BadRequest("a delivered copy accepts only your own PARTSTAT".into())
            })?;
        db::scheduling::reply(&pool, SubjectKind::Task, origin_id, &email, &partstat).await?;
        let (task, etag) = db::tasks::get_task(&pool, task_id).await?;
        let (next_open, subtasks_count) = task_extras(&pool, &task).await?;
        return Ok(Json(task_view(
            &task,
            &etag,
            &db::tasks::list_task_attendees(&pool, task.id).await?,
            &db::tasks::list_task_alarms(&pool, task.id).await?,
            next_open,
            subtasks_count,
        )));
    }
    let patch = db::tasks::TaskPatch {
        summary: Some(body.summary),
        description_html: body
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: body.description_text,
        url: body.url,
        location: body.location,
        starts_at: body.starts_at,
        start_date: body.start_date,
        due_at: body.due_at,
        due_date: body.due_date,
        duration_secs: body.duration_secs,
        tzid: body.tzid,
        status: body.status,
        percent_complete: body.percent_complete,
        priority: body.priority,
        class: body.class,
        categories: body.categories,
        parent_uid: body.parent_uid.map(Some),
        sort_order: body.sort_order,
        attendees: body.attendees,
        alarms: body
            .alarms
            .map(|alarms| {
                alarms
                    .into_iter()
                    .map(AlarmBody::into_db)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?,
        ..Default::default()
    };
    let (task, etag) =
        db::tasks::update_task(&pool, task_id, if_match.0.as_deref(), &patch).await?;
    // Scheduling dispatch: copies rebuilt, updated REQUESTs / removal CANCELs.
    db::scheduling::dispatch(&pool, SubjectKind::Task, task.id, auth.user.id).await?;
    let (next_open, subtasks_count) = task_extras(&pool, &task).await?;
    fire_task_hooks(
        &pool,
        existing.calendar_id,
        task.id,
        "task_updated",
        crypto.as_deref(),
    )
    .await;
    Ok(Json(task_view(
        &task,
        &etag,
        &db::tasks::list_task_attendees(&pool, task.id).await?,
        &db::tasks::list_task_alarms(&pool, task.id).await?,
        next_open,
        subtasks_count,
    )))
}

#[utoipa::path(
    delete,
    path = "/api/tasks/{id}",
    params(
        ("id" = Uuid, Path, description = "task id"),
        ("if-match" = Option<String>, Header, description = "ETag for optimistic concurrency (412 on mismatch)"),
    ),
    responses((status = 200, description = "deleted; subtasks cascade (deleted count returned)", body = TaskDeleteView))
)]
async fn delete_task(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    if_match: IfMatch,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::tasks::get_task(&pool, task_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    // Deleting a delivered copy is a decline: DECLINED on the origin, copy
    // soft-deleted. Deleting the organizer's row cancels for everyone.
    if db::scheduling::origin_of(&pool, SubjectKind::Task, task_id)
        .await?
        .is_some()
    {
        db::scheduling::decline_copy(&pool, SubjectKind::Task, task_id, auth.user.id).await?;
        return Ok(Json(serde_json::json!({"ok": true, "deleted": 1})));
    }
    let n = db::tasks::delete_task(&pool, task_id, if_match.0.as_deref()).await?;
    db::scheduling::dispatch_cancel(&pool, SubjectKind::Task, existing.id, auth.user.id).await?;
    if let Ok(cal) = db::get_calendar(&pool, existing.calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            cal.id,
            "task_deleted",
            existing.id,
            serde_json::json!({"summary": existing.summary, "kind": "task"}),
            crypto.as_deref(),
        )
        .await;
        crate::webhooks_api::fire(&pool, cal.tenant_id, existing.id, "task_deleted").await;
    }
    Ok(Json(serde_json::json!({"ok": true, "deleted": n})))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct CompleteBody {
    /// For recurring tasks: the occurrence being completed (writes a
    /// RECURRENCE-ID override). One of "timed" or "all_day".
    occurrence: Option<OccurrenceBody>,
}

#[derive(serde::Deserialize, Clone, Copy, utoipa::ToSchema)]
#[serde(untagged)]
enum OccurrenceBody {
    /// Naive local wall clock, e.g. "2026-09-20T08:30:00".
    Timed { timed: chrono::NaiveDateTime },
    /// All-day date, e.g. "2026-09-20".
    AllDay { all_day: NaiveDate },
}

impl OccurrenceBody {
    fn into_db(self) -> db::tasks::Occurrence {
        match self {
            OccurrenceBody::Timed { timed } => db::tasks::Occurrence::Timed(timed),
            OccurrenceBody::AllDay { all_day } => db::tasks::Occurrence::AllDay(all_day),
        }
    }
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct TaskDeleteView {
    ok: bool,
    /// Live subtasks deleted along with the master (cascade count).
    deleted: i64,
}

#[utoipa::path(
    post,
    path = "/api/tasks/{id}/complete",
    params(("id" = Uuid, Path, description = "task id")),
    request_body = CompleteBody,
    responses(
        (status = 200, description = "completed", body = TaskView),
        (status = 404, description = "absent"),
    )
)]
async fn complete_task(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    // An empty body (plain POST) is valid; only {"occurrence": ...} carries one.
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::tasks::get_task(&pool, task_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    let occurrence = serde_json::from_slice::<CompleteBody>(&body)
        .ok()
        .and_then(|b| b.occurrence.map(OccurrenceBody::into_db));
    let (task, etag) = db::tasks::complete_task(&pool, task_id, occurrence, auth.user.id).await?;
    fire_task_hooks(
        &pool,
        existing.calendar_id,
        task.id,
        "task_completed",
        crypto.as_deref(),
    )
    .await;
    let (next_open, subtasks_count) = task_extras(&pool, &task).await?;
    Ok(Json(task_view(
        &task,
        &etag,
        &db::tasks::list_task_attendees(&pool, task.id).await?,
        &db::tasks::list_task_alarms(&pool, task.id).await?,
        next_open,
        subtasks_count,
    )))
}

#[utoipa::path(
    post,
    path = "/api/tasks/{id}/reopen",
    params(("id" = Uuid, Path, description = "task id")),
    request_body = CompleteBody,
    responses(
        (status = 200, description = "reopened", body = TaskView),
        (status = 404, description = "absent"),
    )
)]
async fn reopen_task(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<Uuid>,
    // An empty body (plain POST) is valid; only {"occurrence": ...} carries one.
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::tasks::get_task(&pool, task_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    let occurrence = serde_json::from_slice::<CompleteBody>(&body)
        .ok()
        .and_then(|b| b.occurrence.map(OccurrenceBody::into_db));
    let (task, etag) = db::tasks::reopen_task(&pool, task_id, occurrence).await?;
    let (next_open, subtasks_count) = task_extras(&pool, &task).await?;
    Ok(Json(task_view(
        &task,
        &etag,
        &db::tasks::list_task_attendees(&pool, task.id).await?,
        &db::tasks::list_task_alarms(&pool, task.id).await?,
        next_open,
        subtasks_count,
    )))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/calendars/{id}/tasks",
            post(create_task).get(list_tasks),
        )
        .route(
            "/api/tasks/{id}",
            get(get_task).patch(patch_task).delete(delete_task),
        )
        .route("/api/tasks/{id}/complete", post(complete_task))
        .route("/api/tasks/{id}/reopen", post(reopen_task))
}

/// OpenAPI for the tasks module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_task,
        list_tasks,
        get_task,
        patch_task,
        delete_task,
        complete_task,
        reopen_task,
    ),
    components(schemas(
        TaskBody,
        AlarmBody,
        TaskListQuery,
        CompleteBody,
        OccurrenceBody,
        TaskView,
        AlarmView,
        TaskDeleteView,
    ))
)]
pub(crate) struct TasksApi;

#[cfg(test)]
mod tests {
    use super::*;

    fn row(overrides: impl FnOnce(&mut db::tasks::TaskRow)) -> db::tasks::TaskRow {
        let mut t = db::tasks::TaskRow {
            id: Uuid::new_v4(),
            calendar_id: Uuid::new_v4(),
            uid: "uid-1".into(),
            href: None,
            master_task_id: None,
            recurrence_id: None,
            recurrence_id_date: None,
            starts_at: None,
            start_date: None,
            due_at: None,
            due_date: None,
            duration: None,
            tzid: None,
            floating: false,
            completed_at: None,
            rrule: None,
            rdate: serde_json::json!([]),
            exdate: serde_json::json!([]),
            summary: "buy milk".into(),
            description_html: Some("<b>x</b>".into()),
            description_text: None,
            url: None,
            location: None,
            status: Some("NEEDS-ACTION".into()),
            percent_complete: None,
            priority: Some(5),
            class: Some("PRIVATE".into()),
            categories: vec!["errands".into()],
            parent_uid: None,
            sort_order: Some(3),
            // extra_props must never leak through the view
            extra_props: serde_json::json!([{"name": "X-SECRET", "params": [], "value": "shh"}]),
            organizer_user_id: None,
            organizer_email: Some("o@x.test".into()),
            organizer_name: None,
            origin_id: None,
            sequence: 2,
            etag: "stored-etag".into(),
            created_by: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        overrides(&mut t);
        t
    }

    #[test]
    fn task_view_shape_has_no_extra_props() {
        let t = row(|_| {});
        let v = serde_json::to_value(task_view(&t, "etag-1", &[], &[], None, 4)).unwrap();
        assert_eq!(v["summary"], "buy milk");
        assert_eq!(v["status"], "NEEDS-ACTION");
        assert_eq!(v["class"], "PRIVATE");
        assert_eq!(v["categories"], serde_json::json!(["errands"]));
        assert_eq!(v["sort_order"], 3);
        assert_eq!(v["sequence"], 2);
        assert_eq!(v["etag"], "etag-1");
        assert_eq!(v["subtasks_count"], 4);
        assert!(v["next_open"].is_null());
        assert!(v["extra_props"].is_null(), "extra_props must not leak");
        assert!(v["attendees"].as_array().unwrap().is_empty());
        assert!(v["alarms"].as_array().unwrap().is_empty());
    }

    #[test]
    fn is_overdue_follows_due_and_status() {
        let open_past = row(|t| t.due_at = Some(Utc::now() - chrono::Duration::days(1)));
        assert!(is_overdue(&open_past));
        let done = row(|t| {
            t.due_at = Some(Utc::now() - chrono::Duration::days(1));
            t.status = Some("COMPLETED".into());
        });
        assert!(!is_overdue(&done));
        let cancelled = row(|t| {
            t.due_at = Some(Utc::now() - chrono::Duration::days(1));
            t.completed_at = Some(Utc::now());
        });
        assert!(!is_overdue(&cancelled));
        let future = row(|t| t.due_at = Some(Utc::now() + chrono::Duration::days(1)));
        assert!(!is_overdue(&future));
        let all_day_past =
            row(|t| t.due_date = Some((Utc::now() - chrono::Duration::days(1)).date_naive()));
        assert!(is_overdue(&all_day_past));
        let undated = row(|_| {});
        assert!(!is_overdue(&undated));
    }

    #[test]
    fn occurrence_body_parses_timed_and_all_day() {
        let v: OccurrenceBody =
            serde_json::from_value(serde_json::json!({"timed": "2026-09-20T08:30:00"})).unwrap();
        assert_eq!(
            v.into_db(),
            db::tasks::Occurrence::Timed(
                chrono::NaiveDate::from_ymd_opt(2026, 9, 20)
                    .unwrap()
                    .and_hms_opt(8, 30, 0)
                    .unwrap()
            )
        );
        let v: OccurrenceBody =
            serde_json::from_value(serde_json::json!({"all_day": "2026-09-20"})).unwrap();
        assert_eq!(
            v.into_db(),
            db::tasks::Occurrence::AllDay(chrono::NaiveDate::from_ymd_opt(2026, 9, 20).unwrap())
        );
    }
}
