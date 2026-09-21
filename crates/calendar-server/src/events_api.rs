//! Event CRUD, recurrence occurrence expansion.

use crate::{AppError, AppState, calendars_api::require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use calendar_db::{self as db, scheduling::SubjectKind};
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct LocationBody {
    provider: Option<String>,
    provider_place_id: Option<String>,
    display_name: Option<String>,
    formatted_address: Option<String>,
    street_address: Option<String>,
    locality: Option<String>,
    administrative_area: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    website: Option<String>,
    phone: Option<String>,
    provider_metadata: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct EventBody {
    uid: Option<String>,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    starts_at: Option<chrono::DateTime<Utc>>,
    ends_at: Option<chrono::DateTime<Utc>>,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    tzid: Option<String>,
    all_day: Option<bool>,
    rrule: Option<String>,
    rdate: Option<serde_json::Value>,
    exdate: Option<serde_json::Value>,
    status: Option<String>,
    priority: Option<i16>,
    class: Option<String>,
    transp: Option<String>,
    categories: Option<Vec<String>>,
    attendees: Option<Vec<db::NewAttendee>>,
    location: Option<LocationBody>,
    // exception support
    master_event_id: Option<Uuid>,
    recurrence_id: Option<chrono::NaiveDateTime>,
    recurrence_id_date: Option<chrono::NaiveDate>,
}

/// Locations are append-only (calendar-db::create_location); a location on
/// the body always writes a new row, whose id then goes on the event.
async fn create_location_from_body(
    pool: &PgPool,
    body: LocationBody,
) -> Result<db::LocationRow, AppError> {
    let new_loc = db::NewLocation {
        provider: body.provider,
        provider_place_id: body.provider_place_id,
        display_name: body.display_name,
        formatted_address: body.formatted_address,
        street_address: body.street_address,
        locality: body.locality,
        administrative_area: body.administrative_area,
        postal_code: body.postal_code,
        country: body.country,
        latitude: body.latitude,
        longitude: body.longitude,
        website: body.website,
        phone: body.phone,
        provider_metadata: body.provider_metadata,
    };
    db::create_location(pool, &new_loc)
        .await
        .map_err(Into::into)
}

fn location_view(loc: &db::LocationRow) -> serde_json::Value {
    serde_json::json!({
        "id": loc.id,
        "provider": loc.provider,
        "provider_place_id": loc.provider_place_id,
        "display_name": loc.display_name,
        "formatted_address": loc.formatted_address,
        "street_address": loc.street_address,
        "locality": loc.locality,
        "administrative_area": loc.administrative_area,
        "postal_code": loc.postal_code,
        "country": loc.country,
        "latitude": loc.latitude,
        "longitude": loc.longitude,
        "website": loc.website,
        "phone": loc.phone,
    })
}

fn event_view(
    event: &db::EventRow,
    etag: &str,
    attendees: &[db::AttendeeRow],
    location: Option<&db::LocationRow>,
    registry: &db::categories::CategoryRegistry,
) -> serde_json::Value {
    serde_json::json!({
        "id": event.id,
        "calendar_id": event.calendar_id,
        "uid": event.uid,
        "master_event_id": event.master_event_id,
        "recurrence_id": event.recurrence_id,
        "recurrence_id_date": event.recurrence_id_date,
        "summary": event.summary,
        "description_html": event.description_html,
        "description_text": event.description_text,
        "url": event.url,
        "starts_at": event.starts_at,
        "ends_at": event.ends_at,
        "start_date": event.start_date,
        "end_date": event.end_date,
        "tzid": event.tzid,
        "all_day": event.all_day,
        "rrule": event.rrule,
        "rdate": event.rdate,
        "exdate": event.exdate,
        "status": event.status,
        "priority": event.priority,
        "class": event.class,
        "transp": event.transp,
        "categories": event.categories,
        "category_details": event.categories.iter().filter_map(|slug| {
            registry.get(slug).map(|info| serde_json::json!({
                "slug": slug, "name": info.name, "color": info.color,
            }))
        }).collect::<Vec<_>>(),
        "location_id": event.location_id,
        "location": location.map(location_view),
        "organizer_email": event.organizer_email,
        "sequence": event.sequence,
        "etag": etag,
        "created_at": event.created_at,
        "updated_at": event.updated_at,
        "attendees": attendees.iter().map(|a| serde_json::json!({
            "email": a.email, "display_name": a.display_name, "telephone": a.telephone,
            "role": a.role, "partstat": a.partstat, "rsvp": a.rsvp,
            "contact_id": a.contact_id, "user_id": a.user_id,
        })).collect::<Vec<_>>(),
    })
}

/// Mirrors the events-table CHECK constraints; the full structural model lives
/// in calendar-core and the iCalendar stage reuses it.
fn validate_event_body(body: &EventBody) -> Result<(), AppError> {
    let timed = body.starts_at.is_some();
    let all_day = body.start_date.is_some();
    if timed == all_day {
        return Err(AppError::bad_request(
            "exactly one of starts_at or start_date",
        ));
    }
    if timed && body.ends_at.is_none() {
        return Err(AppError::bad_request("timed event needs ends_at"));
    }
    if body.all_day.unwrap_or(false) != all_day && !timed && body.all_day.is_some() {
        return Err(AppError::bad_request(
            "all_day must match start_date presence",
        ));
    }
    if let Some(p) = body.priority
        && !(0..=9).contains(&p)
    {
        return Err(AppError::bad_request("priority must be 0..=9"));
    }
    if body.master_event_id.is_some()
        && body.recurrence_id.is_none()
        && body.recurrence_id_date.is_none()
    {
        return Err(AppError::bad_request("exception needs recurrence_id"));
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

async fn create_event(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(mut body): Json<EventBody>,
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
    validate_event_body(&body)?;
    let location = match body.location.take() {
        Some(loc_body) => Some(create_location_from_body(&pool, loc_body).await?),
        None => None,
    };
    let uid = body
        .uid
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let data = db::NewEventData {
        uid,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        start_date: body.start_date,
        end_date: body.end_date,
        tzid: body.tzid.clone(),
        all_day: body.all_day.unwrap_or(false),
        rrule: body.rrule.clone(),
        rdate: body.rdate.clone(),
        exdate: body.exdate.clone(),
        summary: body.summary.clone(),
        description_html: body
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: body.description_text.clone(),
        url: body.url.clone(),
        status: body.status.clone(),
        priority: body.priority,
        class: body.class.clone(),
        transp: body.transp.clone(),
        categories: body.categories.clone().unwrap_or_default(),
        location_id: location.as_ref().map(|l| l.id),
        organizer_user_id: Some(auth.user.id),
        organizer_email: auth.user.email.clone(),
        organizer_name: auth.user.display_name.clone(),
        master_id: body.master_event_id,
        recurrence_id: body.recurrence_id,
        recurrence_id_date: body.recurrence_id_date,
    };
    let attendees = body.attendees.clone().unwrap_or_default();
    let (event, etag) =
        db::create_event(&pool, calendar_id, auth.user.id, &attendees, &data).await?;
    // Scheduling dispatch (stage 8a): copies for internal attendees, REQUEST
    // intents for external ones.
    db::scheduling::dispatch(&pool, SubjectKind::Event, event.id, auth.user.id).await?;
    if let Ok(cal) = db::get_calendar(&pool, calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            cal.id,
            "event_created",
            event.id,
            serde_json::json!({"summary": event.summary, "starts_at": event.starts_at}),
            crypto.as_deref(),
        )
        .await;
        crate::webhooks_api::fire(&pool, cal.tenant_id, event.id, "event_created").await;
    }
    let rows = db::list_attendees(&pool, event.id).await?;
    let registry = db::categories::registry_for_calendar(&pool, calendar_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(event_view(
            &event,
            &etag,
            &rows,
            location.as_ref(),
            &registry,
        )),
    ))
}

#[derive(serde::Deserialize)]
struct RangeQuery {
    from: Option<chrono::DateTime<Utc>>,
    to: Option<chrono::DateTime<Utc>>,
    /// Occurrences only: comma-separated extras, e.g. `tasks,journals` —
    /// dated markers for tasks/journals appended to the event entries.
    include: Option<String>,
    // ponytail: no pagination yet; per-calendar windows stay small.
}

async fn list_events(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let from = query.from.unwrap_or(Utc::now() - Duration::days(30));
    let to = query.to.unwrap_or(Utc::now() + Duration::days(90));
    let events = db::list_events_in_range(&pool, calendar_id, from, to).await?;
    let registry = db::categories::registry_for_calendar(&pool, calendar_id).await?;
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(events.len());
    for e in &events {
        let location = db::location_for_event(&pool, e).await;
        out.push(event_view(
            e,
            &db::event_etag(e),
            &[],
            location.as_ref(),
            &registry,
        ));
    }
    Ok(Json(serde_json::json!(out)))
}

async fn get_event(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let (event, etag) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        event.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let rows = db::list_attendees(&pool, event.id).await?;
    let location = db::location_for_event(&pool, &event).await;
    let registry = db::categories::registry_for_calendar(&pool, event.calendar_id).await?;
    Ok(Json(event_view(
        &event,
        &etag,
        &rows,
        location.as_ref(),
        &registry,
    )))
}

async fn patch_event(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
    if_match: IfMatch,
    Json(mut body): Json<EventBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    // A delivered copy accepts only the attendee's own PARTSTAT; everything
    // else is discarded (the organizer's next dispatch rebuilds the copy).
    if let Some(origin_id) = db::scheduling::origin_of(&pool, SubjectKind::Event, event_id).await? {
        let email = db::scheduling::attendee_email_for_user(
            &pool,
            SubjectKind::Event,
            event_id,
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
        db::scheduling::reply(&pool, SubjectKind::Event, origin_id, &email, &partstat).await?;
        let (event, etag) = db::get_event(&pool, event_id).await?;
        let rows = db::list_attendees(&pool, event.id).await?;
        let registry = db::categories::registry_for_calendar(&pool, event.calendar_id).await?;
        return Ok(Json(event_view(
            &event,
            &etag,
            &rows,
            db::location_for_event(&pool, &event).await.as_ref(),
            &registry,
        )));
    }
    let location_id = match body.location.take() {
        Some(loc_body) => Some(create_location_from_body(&pool, loc_body).await?.id),
        None => None,
    };
    let patch = db::EventPatch {
        summary: Some(body.summary),
        description_html: body.description_html,
        description_text: body.description_text,
        url: body.url,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        start_date: body.start_date,
        end_date: body.end_date,
        all_day: body.all_day,
        tzid: body.tzid,
        status: body.status,
        priority: body.priority,
        class: body.class,
        transp: body.transp,
        location_id,
        categories: body.categories,
        attendees: body.attendees,
    };
    let (event, etag) = db::update_event(&pool, event_id, if_match.0.as_deref(), &patch).await?;
    // Scheduling dispatch: copies rebuilt, updated REQUESTs / removal CANCELs.
    db::scheduling::dispatch(&pool, SubjectKind::Event, event.id, auth.user.id).await?;
    // Fire-and-forget: rules must never break the mutation (same as create).
    if let Ok(cal) = db::get_calendar(&pool, existing.calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            cal.id,
            "event_updated",
            event.id,
            serde_json::json!({"summary": event.summary, "starts_at": event.starts_at}),
            crypto.as_deref(),
        )
        .await;
        crate::webhooks_api::fire(&pool, cal.tenant_id, event.id, "event_updated").await;
    }
    let rows = db::list_attendees(&pool, event.id).await?;
    let location = db::location_for_event(&pool, &event).await;
    let registry = db::categories::registry_for_calendar(&pool, event.calendar_id).await?;
    Ok(Json(event_view(
        &event,
        &etag,
        &rows,
        location.as_ref(),
        &registry,
    )))
}

#[derive(serde::Deserialize)]
struct SelfPartstatBody {
    partstat: String,
}

/// RSVP endpoint for a signed-in attendee (stage 8a): resolves the caller's
/// attendee identity on the origin (by attendee user_id or their account
/// email — a delivered copy carries the same rows) and records the reply so
/// it reaches the organizer's clients via sync.
async fn patch_own_partstat(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
    Json(body): Json<SelfPartstatBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (_, _) = db::get_event(&pool, event_id).await?;
    let email =
        db::scheduling::attendee_email_for_user(&pool, SubjectKind::Event, event_id, auth.user.id)
            .await?
            .ok_or(AppError::Forbidden)?;
    db::scheduling::reply(&pool, SubjectKind::Event, event_id, &email, &body.partstat).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn delete_event(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
    if_match: IfMatch,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    // Deleting a delivered copy is a decline: DECLINED on the origin, copy
    // soft-deleted. Deleting the organizer's row cancels for everyone.
    if db::scheduling::origin_of(&pool, SubjectKind::Event, event_id)
        .await?
        .is_some()
    {
        db::scheduling::decline_copy(&pool, SubjectKind::Event, event_id, auth.user.id).await?;
        return Ok(Json(serde_json::json!({"ok": true})));
    }
    db::delete_event(&pool, event_id, if_match.0.as_deref()).await?;
    // Cancelled meeting: METHOD:CANCEL to external attendees, copies to
    // STATUS:CANCELLED with an in-app notice.
    db::scheduling::dispatch_cancel(&pool, SubjectKind::Event, existing.id, auth.user.id).await?;
    // Fire-and-forget with the pre-change event as context; the row is gone
    // by now, so "existing" is all the delete rule ever sees.
    if let Ok(cal) = db::get_calendar(&pool, existing.calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            cal.id,
            "event_deleted",
            existing.id,
            serde_json::json!({"summary": existing.summary, "starts_at": existing.starts_at}),
            crypto.as_deref(),
        )
        .await;
        crate::webhooks_api::fire(&pool, cal.tenant_id, existing.id, "event_deleted").await;
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// If-Match header extractor; None when absent.
struct IfMatch(Option<String>);
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

/// Expanded occurrences with exceptions overlaid: each master occurrence
/// matching an exception's RECURRENCE-ID is replaced by that exception row.
async fn list_occurrences(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let from = query.from.unwrap_or(Utc::now() - Duration::days(30));
    let to = query.to.unwrap_or(Utc::now() + Duration::days(90));
    let rows = db::list_events_in_range(&pool, calendar_id, from, to).await?;
    let mut out = expand_occurrences_json(&pool, calendar_id, rows, from, to).await?;
    // Additive ?include=tasks,journals: dated markers for the calendar view;
    // the event entries above keep their shape untouched.
    let include: Vec<&str> = query
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if include.contains(&"tasks") {
        out.extend(include_task_markers(&pool, calendar_id, from, to).await?);
    }
    if include.contains(&"journals") {
        out.extend(include_journal_markers(&pool, calendar_id, from, to).await?);
    }
    Ok(Json(serde_json::json!(out)))
}

/// Dated task markers: masters whose start or due (timed or all-day) falls in
/// the window. Undated and subtask rows never appear (design section 8).
async fn include_task_markers(
    pool: &PgPool,
    calendar_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<serde_json::Value>, AppError> {
    let rows = db::tasks::list_tasks(pool, calendar_id, &db::tasks::TaskFilter::default()).await?;
    let uids: Vec<String> = rows.iter().map(|t| t.uid.clone()).collect();
    let counts: std::collections::HashMap<String, i64> = if uids.is_empty() {
        Default::default()
    } else {
        sqlx::query_as::<_, (Option<String>, i64)>(
            "SELECT parent_uid, COUNT(*)::bigint FROM tasks
             WHERE calendar_id = $1 AND parent_uid = ANY($2)
               AND deleted_at IS NULL AND master_task_id IS NULL
             GROUP BY parent_uid",
        )
        .bind(calendar_id)
        .bind(&uids)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .into_iter()
        .filter_map(|(uid, n)| uid.map(|u| (u, n)))
        .collect()
    };
    let mut out = Vec::new();
    for t in &rows {
        let candidates = [
            t.starts_at,
            t.due_at,
            t.start_date
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|n| n.and_utc()),
            t.due_date
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|n| n.and_utc()),
        ];
        if !candidates
            .iter()
            .flatten()
            .any(|at| *at >= from && *at < to)
        {
            continue;
        }
        let next_open = if t.rrule.is_some() {
            db::tasks::next_open(pool, t.id, 90).await.unwrap_or(None)
        } else {
            None
        };
        out.push(serde_json::json!({
            "type": "task",
            "id": t.id,
            "summary": t.summary,
            "due": t.due_at.map(|d| serde_json::json!(d))
                .or_else(|| t.due_date.map(|d| serde_json::json!(d.to_string()))),
            "status": t.status,
            "percent": t.percent_complete,
            "completed": t.completed_at,
            "next_open": next_open,
            "subtasks_count": counts.get(&t.uid).copied().unwrap_or(0),
        }));
    }
    Ok(out)
}

/// Dated journal markers: journals whose DTSTART falls in the window.
/// Undated notes never appear.
async fn include_journal_markers(
    pool: &PgPool,
    calendar_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<serde_json::Value>, AppError> {
    let rows = db::journals::list_journals(
        pool,
        calendar_id,
        &db::journals::JournalFilter {
            from: Some(from),
            to: Some(to),
            ..Default::default()
        },
    )
    .await?;
    Ok(rows
        .iter()
        .map(|j| {
            serde_json::json!({
                "type": "journal",
                "id": j.id,
                "summary": j.summary,
                "start": j.starts_at.map(|d| serde_json::json!(d))
                    .or_else(|| j.start_date.map(|d| serde_json::json!(d.to_string()))),
                "status": j.status,
            })
        })
        .collect())
}

/// GET /api/subscriptions/{id}/occurrences — same expansion as
/// `list_occurrences`, but for a calendar the caller doesn't own: access is
/// gated on holding a live subscription to it, not on calendar ACL, and only
/// PUBLIC-class events come back (matches the public feed's privacy rule).
async fn list_subscription_occurrences(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(subscription_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let calendar_id =
        db::sharing::subscribed_calendar_id(&pool, auth.user.id, subscription_id).await?;
    let from = query.from.unwrap_or(Utc::now() - Duration::days(30));
    let to = query.to.unwrap_or(Utc::now() + Duration::days(90));
    let rows = db::list_public_events_in_range(&pool, calendar_id, from, to).await?;
    let out = expand_occurrences_json(&pool, calendar_id, rows, from, to).await?;
    Ok(Json(serde_json::json!(out)))
}

/// Expanded occurrences with exceptions overlaid: each master occurrence
/// matching an exception's RECURRENCE-ID is replaced by that exception row.
async fn expand_occurrences_json(
    pool: &PgPool,
    calendar_id: Uuid,
    rows: Vec<db::EventRow>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<serde_json::Value>, AppError> {
    let registry = db::categories::registry_for_calendar(pool, calendar_id).await?;
    let master_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.master_event_id.is_none())
        .map(|r| r.id)
        .collect();
    let exceptions = db::list_exceptions(pool, &master_ids).await?;
    // Custom VTIMEZONEs stored for this calendar (ADR-012); tzdb wins.
    let resolver = db::timezones::load_for_calendar(pool, calendar_id)
        .await
        .unwrap_or_default();

    let mut out: Vec<serde_json::Value> = Vec::new();
    for event in &rows {
        // Exception rows surface only through their master's overlay.
        if event.master_event_id.is_some() {
            continue;
        }
        if event.rrule.is_none() {
            let location = db::location_for_event(pool, event).await;
            let mut view = serde_json::json!({
                "event": event_view(event, &db::event_etag(event), &[], location.as_ref(), &registry)
            });
            view["occurrence"] = match (event.starts_at, event.start_date) {
                (Some(at), _) => serde_json::json!({"kind": "timed", "at": at}),
                (None, Some(date)) => {
                    serde_json::json!({"kind": "all_day", "date": date.to_string()})
                }
                _ => serde_json::Value::Null,
            };
            view["is_exception"] = serde_json::json!(false);
            out.push(view);
            continue;
        }
        let dtstart = match (event.starts_at, event.start_date) {
            (Some(at), _) => calendar_core::DateOrDateTime::Timed(at),
            (None, Some(date)) => calendar_core::DateOrDateTime::AllDay(date),
            _ => continue,
        };
        let rdate = json_to_points(&event.rdate);
        let exdate = json_to_points(&event.exdate);
        let expanded = calendar_core::recurrence::expand_occurrences(
            dtstart,
            event.tzid.as_deref(),
            Some(&resolver),
            Some(&event.rrule.clone().unwrap_or_default()),
            &rdate,
            &exdate,
            from,
            to,
        )
        .map_err(|e| AppError::bad_request(e.to_string()))?;
        for point in expanded {
            // Exceptions are keyed on the original wall-clock occurrence.
            let zone =
                calendar_core::recurrence::resolve_tz(event.tzid.as_deref(), Some(&resolver))
                    .map_err(|e| AppError::bad_request(e.to_string()))?;
            let wall: chrono::NaiveDateTime = match point {
                calendar_core::DateOrDateTime::Timed(at) => zone.to_local(at),
                calendar_core::DateOrDateTime::AllDay(date) => date.and_hms_opt(0, 0, 0).unwrap(),
            };
            let matched = exceptions
                .iter()
                .find(|ex| ex.master_event_id == Some(event.id) && ex.recurrence_id == Some(wall));
            let (source, is_exception) = matched.map_or((event, false), |ex| (ex, true));
            let location = db::location_for_event(pool, source).await;
            let mut view = event_view(
                source,
                &db::event_etag(source),
                &[],
                location.as_ref(),
                &registry,
            );
            view["occurrence"] = match point {
                calendar_core::DateOrDateTime::Timed(at) => {
                    serde_json::json!({"kind": "timed", "at": at})
                }
                calendar_core::DateOrDateTime::AllDay(date) => {
                    serde_json::json!({"kind": "all_day", "date": date.to_string()})
                }
            };
            view["is_exception"] = serde_json::json!(is_exception);
            out.push(view);
        }
    }
    Ok(out)
}

fn json_to_points(value: &serde_json::Value) -> Vec<calendar_core::DateOrDateTime> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    if let Some(s) = v.as_str() {
                        if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                            return Some(calendar_core::DateOrDateTime::Timed(
                                at.with_timezone(&Utc),
                            ));
                        }
                        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                            return Some(calendar_core::DateOrDateTime::AllDay(d));
                        }
                    }
                    None
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/calendars/{id}/events",
            post(create_event).get(list_events),
        )
        .route("/api/calendars/{id}/occurrences", get(list_occurrences))
        .route(
            "/api/subscriptions/{id}/occurrences",
            get(list_subscription_occurrences),
        )
        .route(
            "/api/events/{id}",
            get(get_event).patch(patch_event).delete(delete_event),
        )
        .route(
            "/api/events/{id}/attendees/self",
            axum::routing::patch(patch_own_partstat),
        )
}
