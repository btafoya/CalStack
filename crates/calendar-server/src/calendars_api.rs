//! Calendar CRUD and ACL management.

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use calendar_db::{self as db};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct CalendarBody {
    slug: String,
    name: String,
    description: Option<String>,
    color: Option<String>,
    timezone: Option<String>,
    /// Component set for this calendar (ADR-015); omit for VEVENT-only default.
    components: Option<Vec<String>>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct CalendarPatchBody {
    name: Option<String>,
    description: Option<String>,
    color: Option<String>,
    timezone: Option<String>,
    order_index: Option<i32>,
    /// 1-3 distinct kinds; removal is refused with 409 plus per-type counts
    /// while live items of a removed kind exist.
    components: Option<Vec<String>>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct AclEntryBody {
    user_id: Uuid,
    capability: String,
    can_manage_acl: bool,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct AclBody {
    entries: Vec<AclEntryBody>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct CalendarView {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    color: Option<String>,
    timezone: Option<String>,
    order_index: i32,
    components: Vec<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    my_capability: &'static str,
}

fn calendar_view(
    calendar: &db::CalendarRow,
    capability: calendar_core::CalendarCapability,
) -> CalendarView {
    CalendarView {
        id: calendar.id,
        slug: calendar.slug.clone(),
        name: calendar.name.clone(),
        description: calendar.description.clone(),
        color: calendar.color.clone(),
        timezone: calendar.timezone.clone(),
        order_index: calendar.order_index,
        components: calendar.components.clone(),
        created_at: calendar.created_at,
        updated_at: calendar.updated_at,
        my_capability: capability.as_db_str(),
    }
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct AclEntryView {
    user_id: Uuid,
    capability: &'static str,
    can_manage_acl: bool,
}

fn parse_acl(
    body: &AclBody,
) -> Result<Vec<(Uuid, calendar_core::CalendarCapability, bool)>, AppError> {
    let entries: Vec<calendar_core::AclEntry> = body
        .entries
        .iter()
        .map(|e| {
            calendar_core::CalendarCapability::from_db_str(&e.capability)
                .map(|cap| calendar_core::AclEntry {
                    principal_user_id: e.user_id,
                    capability: cap,
                    can_manage_acl: e.can_manage_acl,
                })
                .ok_or_else(|| {
                    AppError::bad_request(format!("unknown capability: {}", e.capability))
                })
        })
        .collect::<Result<_, _>>()?;
    let set = calendar_core::AclSet::validate(&entries)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    Ok(set
        .entries
        .iter()
        .map(|e| (e.principal_user_id, e.capability, e.can_manage_acl))
        .collect())
}

/// ACL guard: fetch capability, require it, 404 when absent (no existence leak).
pub(crate) async fn require_capability(
    pool: &PgPool,
    calendar_id: Uuid,
    user_id: Uuid,
    required: calendar_core::CalendarCapability,
) -> Result<db::CalendarRow, AppError> {
    let cap = db::calendar_capability(pool, calendar_id, user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !cap.satisfies(required) {
        return Err(AppError::Forbidden);
    }
    Ok(db::get_calendar(pool, calendar_id).await?)
}

#[utoipa::path(
    post,
    path = "/api/calendars",
    request_body = CalendarBody,
    responses(
        (status = 201, description = "created", body = CalendarView),
        (status = 400, description = "validation error or slug conflict"),
    )
)]
async fn create_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CalendarBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    calendar_core::validate_slug(&body.slug).map_err(|e| AppError::bad_request(e.to_string()))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let mut components = None;
    if let Some(want) = &body.components {
        const ALL: [&str; 3] = ["VEVENT", "VTODO", "VJOURNAL"];
        let unique = want
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>()
            .len()
            == want.len();
        if !unique
            || want.is_empty()
            || want.len() > 3
            || want.iter().any(|c| !ALL.contains(&c.as_str()))
        {
            return Err(AppError::bad_request(
                "components must be 1-3 distinct values from VEVENT, VTODO, VJOURNAL",
            ));
        }
        components = Some(want.clone());
    }
    let new_calendar = db::NewCalendar {
        slug: body.slug,
        name: body.name,
        description: body.description,
        color: body.color,
        timezone: body.timezone,
        components,
    };
    let calendar = db::create_calendar(
        &pool,
        tenant_id,
        &new_calendar,
        auth.user.id,
        &[(auth.user.id, calendar_core::CalendarCapability::Owner, true)],
    )
    .await
    .map_err(|e| match e {
        db::DbError::Conflict(msg) => AppError::bad_request(msg),
        other => other.into(),
    })?;
    Ok((
        StatusCode::CREATED,
        Json(calendar_view(
            &calendar,
            calendar_core::CalendarCapability::Owner,
        )),
    ))
}

#[utoipa::path(
    get,
    path = "/api/calendars",
    responses(
        (status = 200, description = "list", body = Vec<CalendarView>),
    )
)]
async fn list_calendars(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows = db::list_calendars_for_user(&pool, auth.user.id).await?;
    Ok(Json(
        rows.iter()
            .map(|(cal, cap)| calendar_view(cal, *cap))
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}",
    params(("id" = Uuid, Path, description = "calendar id")),
    responses(
        (status = 200, description = "calendar", body = CalendarView),
        (status = 404, description = "absent"),
    )
)]
async fn get_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let calendar = require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let cap = db::calendar_capability(&pool, calendar_id, auth.user.id)
        .await?
        .unwrap_or(calendar_core::CalendarCapability::FreeBusy);
    Ok(Json(calendar_view(&calendar, cap)))
}

#[utoipa::path(
    patch,
    path = "/api/calendars/{id}",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body = CalendarPatchBody,
    responses(
        (status = 200, description = "updated", body = CalendarView),
        (status = 400, description = "validation error"),
        (status = 404, description = "absent"),
        (status = 409, description = "component removal blocked by live items"),
    )
)]
async fn patch_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<CalendarPatchBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let calendar = require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    // Component removal is refused while live items of a removed kind exist
    // (design section 3, decision 9): 409 with per-type counts.
    let mut components = None;
    if let Some(want) = &body.components {
        const ALL: [&str; 3] = ["VEVENT", "VTODO", "VJOURNAL"];
        let unique = want
            .iter()
            .map(|c| c.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
            == want.len();
        if !unique
            || want.is_empty()
            || want.len() > 3
            || want.iter().any(|c| !ALL.contains(&c.as_str()))
        {
            return Err(AppError::bad_request(
                "components must be 1-3 distinct values from VEVENT, VTODO, VJOURNAL",
            ));
        }
        let removed: Vec<&str> = calendar
            .components
            .iter()
            .map(String::as_str)
            .filter(|c| !want.iter().any(|w| w == c))
            .collect();
        if !removed.is_empty() {
            let counts = db::tasks::count_live_components(&pool, calendar_id).await?;
            let blocked: Vec<String> = removed
                .iter()
                .filter_map(|kind| {
                    counts
                        .iter()
                        .find(|c| c.kind == *kind)
                        .filter(|c| c.live > 0)
                        .map(|c| format!("{}={}", c.kind, c.live))
                })
                .collect();
            if !blocked.is_empty() {
                return Err(AppError::Conflict(format!(
                    "cannot remove components with live items: {}",
                    blocked.join(", ")
                )));
            }
        }
        components = Some(want.clone());
    }
    let changes = db::CalendarUpdate {
        name: body.name,
        description: body.description,
        color: body.color,
        timezone: body.timezone,
        order_index: body.order_index,
        components,
    };
    let calendar = db::update_calendar(&pool, calendar_id, &changes).await?;
    let cap = db::calendar_capability(&pool, calendar_id, auth.user.id)
        .await?
        .unwrap_or(calendar_core::CalendarCapability::FreeBusy);
    Ok(Json(calendar_view(&calendar, cap)))
}

#[utoipa::path(
    delete,
    path = "/api/calendars/{id}",
    params(("id" = Uuid, Path, description = "calendar id")),
    responses((status = 200, description = "deleted (soft, retention before purge)", body = crate::OkView))
)]
async fn delete_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    db::soft_delete_calendar(&pool, calendar_id).await?;
    Ok(Json(crate::OkView { ok: true }))
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}/acl",
    params(("id" = Uuid, Path, description = "calendar id")),
    responses(
        (status = 200, description = "entries", body = Vec<AclEntryView>),
    )
)]
async fn get_calendar_acl(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    let acl = db::list_calendar_acl(&pool, calendar_id).await?;
    Ok(Json(
        acl.iter()
            .map(|(user, cap, manage)| AclEntryView {
                user_id: *user,
                capability: cap.as_db_str(),
                can_manage_acl: *manage,
            })
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    put,
    path = "/api/calendars/{id}/acl",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body = AclBody,
    responses((status = 200, description = "replaced", body = crate::OkView))
)]
async fn put_calendar_acl(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<AclBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    let acl = parse_acl(&body)?;
    db::replace_calendar_acl(&pool, calendar_id, &acl).await?;
    Ok(Json(crate::OkView { ok: true }))
}

/// OpenAPI for the calendar module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_calendar,
        list_calendars,
        get_calendar,
        patch_calendar,
        delete_calendar,
        get_calendar_acl,
        put_calendar_acl,
    ),
    components(schemas(
        CalendarBody,
        CalendarPatchBody,
        AclBody,
        AclEntryBody,
        CalendarView,
        AclEntryView,
        crate::OkView,
    ))
)]
pub(crate) struct CalendarsApi;

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/calendars", get(list_calendars).post(create_calendar))
        .route(
            "/api/calendars/{id}",
            get(get_calendar)
                .patch(patch_calendar)
                .delete(delete_calendar),
        )
        .route(
            "/api/calendars/{id}/acl",
            get(get_calendar_acl).put(put_calendar_acl),
        )
}
