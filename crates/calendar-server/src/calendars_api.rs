//! Calendar CRUD and ACL management.

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
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
    /// Remote ICS URL: creates a subscribed read-only calendar fed by the
    /// ics_sync job. Empty/absent = a normal calendar.
    source_url: Option<String>,
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
    /// Owner-only subscribe/unsubscribe: a URL to subscribe, empty string to
    /// clear. Changing it resets the sync state.
    source_url: Option<String>,
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
    /// True when fed from a remote ICS source (read-only, ics_sync job).
    read_only: bool,
    source_url: Option<String>,
    source_synced_at: Option<chrono::DateTime<chrono::Utc>>,
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
        read_only: calendar.source_url.is_some(),
        source_url: calendar.source_url.clone(),
        source_synced_at: calendar.source_synced_at,
    }
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct AclEntryView {
    user_id: Uuid,
    capability: &'static str,
    can_manage_acl: bool,
}

/// Validates an optional remote source URL: http(s) only. None = keep
/// normal-calendar semantics; the empty string normalizes to None too.
fn normalize_source(source_url: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(url) = source_url.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let parsed =
        reqwest::Url::parse(url).map_err(|_| AppError::bad_request("invalid source URL"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(AppError::bad_request("source URL must be http or https"));
    }
    Ok(Some(url.to_string()))
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
/// ReadWrite requests to source-fed (subscribed) calendars are refused:
/// their content comes from the ics_sync job.
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
    let calendar = db::get_calendar(pool, calendar_id).await?;
    if required == calendar_core::CalendarCapability::ReadWrite && calendar.source_url.is_some() {
        return Err(AppError::Forbidden);
    }
    Ok(calendar)
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
        source_url: normalize_source(body.source_url.as_deref())?,
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
    // Metadata edits stay at ReadWrite, but the write-content guard in
    // require_capability would block subscribed calendars — they may be
    // renamed/recolored. Read the capability read-only here.
    let calendar = require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    // Source (un)subscribing is owner-only.
    if body.source_url.is_some() {
        require_capability(
            &pool,
            calendar_id,
            auth.user.id,
            calendar_core::CalendarCapability::Owner,
        )
        .await?;
        db::set_calendar_source(
            &pool,
            calendar_id,
            normalize_source(body.source_url.as_deref())?.as_deref(),
        )
        .await?;
    }
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
        export_ics,
        import_ics,
    ),
    components(schemas(
        CalendarBody,
        CalendarPatchBody,
        AclBody,
        AclEntryBody,
        CalendarView,
        AclEntryView,
        ImportResult,
        ImportRejected,
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
        .route("/api/calendars/{id}/export.ics", get(export_ics))
        .route("/api/calendars/{id}/import", post(import_ics))
}

// ============ ICS export and import ============

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ImportRejected {
    uid: String,
    reason: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ImportResult {
    imported: i64,
    /// UIDs that already exist live in the target calendar (skipped, never
    /// overwritten).
    skipped: i64,
    rejected: Vec<ImportRejected>,
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}/export.ics",
    params(("id" = Uuid, Path, description = "calendar id")),
    responses(
        (status = 200, description = "the whole calendar as text/calendar", body = String, content_type = "text/calendar"),
        (status = 404, description = "absent"),
    )
)]
async fn export_ics(
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
    let ics = crate::sharing_api::build_calendar_ics(&pool, calendar_id, false).await?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/calendar; charset=utf-8",
        )],
        [(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.ics\"", calendar.slug),
        )],
        ics,
    ))
}

#[utoipa::path(
    post,
    path = "/api/calendars/{id}/import",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body(content = String, content_type = "text/calendar", description = "the ICS file to import"),
    responses(
        (status = 200, description = "per-item outcome; duplicates are skipped, never overwritten", body = ImportResult),
        (status = 400, description = "not valid iCalendar, or no importable components"),
        (status = 413, description = "body over IMPORT_MAX_BYTES"),
    )
)]
async fn import_ics(
    State(AppState { pool, config, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    body: axum::body::Bytes,
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
    if !calendar.components.iter().any(|c| c == "VEVENT") {
        return Err(AppError::bad_request(
            "calendar does not include VEVENT components",
        ));
    }
    if body.len() as i64 > config.import_max_bytes {
        return Err(AppError::Conflict(format!(
            "import body over the {} byte limit",
            config.import_max_bytes
        )));
    }
    let text = String::from_utf8_lossy(&body);
    let parsed = calendar_caldav::parse_calendar(&text)
        .map_err(|e| AppError::bad_request(format!("not valid iCalendar: {e}")))?;
    // VTODO/VJOURNAL import is not supported: their storage path lives in
    // the CalDAV adapter (series + assignee semantics). Silent-loss guard:
    // a file holding only those is refused outright.
    if parsed.events.is_empty() && (text.contains("BEGIN:VTODO") || text.contains("BEGIN:VJOURNAL"))
    {
        return Err(AppError::bad_request(
            "import supports VEVENT only; tasks and journals go through CalDAV",
        ));
    }
    // Group by UID: one master plus its RECURRENCE-ID overrides per series.
    struct Group<'a> {
        master: Option<&'a calendar_caldav::ParsedEvent>,
        overrides: Vec<&'a calendar_caldav::ParsedEvent>,
    }
    let mut groups: std::collections::HashMap<String, Group<'_>> = std::collections::HashMap::new();
    for event in &parsed.events {
        let group = groups.entry(event.uid.clone()).or_insert(Group {
            master: None,
            overrides: Vec::new(),
        });
        if event.recurrence_id.is_none() && event.recurrence_id_date.is_none() {
            group.master = Some(event);
        } else {
            group.overrides.push(event);
        }
    }
    let zones: Vec<db::timezones::NewTimezone> = parsed
        .timezones
        .iter()
        .map(|tz| db::timezones::NewTimezone {
            tzid: tz.tzid.clone(),
            definition: tz.definition.clone(),
            rules: tz.rules.clone(),
        })
        .collect();
    let mut result = ImportResult {
        imported: 0,
        skipped: 0,
        rejected: Vec::new(),
    };
    for (uid, group) in groups {
        let Some(master) = group.master else {
            result.rejected.push(ImportRejected {
                uid,
                reason: "override without its master; import whole series".into(),
            });
            continue;
        };
        // Skip duplicates: an existing live UID is never overwritten. A
        // soft-deleted UID resurrects through put_series's own match.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM events
                WHERE calendar_id = $1 AND uid = $2 AND deleted_at IS NULL
                  AND master_event_id IS NULL AND recurrence_id IS NULL AND recurrence_id_date IS NULL
            )",
        )
        .bind(calendar_id)
        .bind(&uid)
        .fetch_one(&pool)
        .await
        .map_err(db::DbError::from)
        .map_err(AppError::from)?;
        if exists {
            result.skipped += 1;
            continue;
        }
        // Fresh canonical href per series (put_series derives the row id
        // from a "{uuid}.ics" href; updates later match by UID anyway).
        let href = format!("{}.ics", Uuid::new_v4());
        let overrides: Vec<db::ics_upsert::IcsEventUpsert> = group
            .overrides
            .iter()
            .map(|e| calendar_caldav::upsert_for(&auth.user, e))
            .collect();
        match db::ics_upsert::put_series(
            &pool,
            calendar_id,
            auth.user.id,
            &href,
            &calendar_caldav::upsert_for(&auth.user, master),
            &overrides,
            &zones,
            &db::ics_upsert::PutPrecondition::None,
        )
        .await
        {
            Ok(_) => result.imported += 1,
            Err(e) => result.rejected.push(ImportRejected {
                uid,
                reason: e.to_string(),
            }),
        }
    }
    Ok(Json(result))
}
