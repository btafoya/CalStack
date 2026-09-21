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

#[derive(serde::Deserialize, utoipa::ToSchema)]
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
    #[schema(value_type = Object)]
    provider_metadata: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
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
    #[schema(value_type = Object)]
    rdate: Option<serde_json::Value>,
    #[schema(value_type = Object)]
    exdate: Option<serde_json::Value>,
    status: Option<String>,
    priority: Option<i16>,
    class: Option<String>,
    transp: Option<String>,
    categories: Option<Vec<String>>,
    /// Calendar-db's attendee write model; each entry needs an email or a telephone.
    #[schema(value_type = Vec<Object>)]
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

#[derive(serde::Serialize, utoipa::ToSchema)]
struct LocationView {
    id: Uuid,
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
}

fn location_view(loc: &db::LocationRow) -> LocationView {
    LocationView {
        id: loc.id,
        provider: loc.provider.clone(),
        provider_place_id: loc.provider_place_id.clone(),
        display_name: loc.display_name.clone(),
        formatted_address: loc.formatted_address.clone(),
        street_address: loc.street_address.clone(),
        locality: loc.locality.clone(),
        administrative_area: loc.administrative_area.clone(),
        postal_code: loc.postal_code.clone(),
        country: loc.country.clone(),
        latitude: loc.latitude,
        longitude: loc.longitude,
        website: loc.website.clone(),
        phone: loc.phone.clone(),
    }
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct CategoryDetailView {
    slug: String,
    name: String,
    color: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct EventView {
    id: Uuid,
    calendar_id: Uuid,
    uid: String,
    master_event_id: Option<Uuid>,
    recurrence_id: Option<chrono::NaiveDateTime>,
    recurrence_id_date: Option<chrono::NaiveDate>,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    tzid: Option<String>,
    all_day: bool,
    rrule: Option<String>,
    #[schema(value_type = Object)]
    rdate: serde_json::Value,
    #[schema(value_type = Object)]
    exdate: serde_json::Value,
    status: Option<String>,
    priority: Option<i16>,
    class: Option<String>,
    transp: Option<String>,
    categories: Vec<String>,
    category_details: Vec<CategoryDetailView>,
    location_id: Option<Uuid>,
    location: Option<LocationView>,
    organizer_email: String,
    sequence: i32,
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    attendees: Vec<crate::AttendeeView>,
}

fn event_view(
    event: &db::EventRow,
    etag: &str,
    attendees: &[db::AttendeeRow],
    location: Option<&db::LocationRow>,
    registry: &db::categories::CategoryRegistry,
) -> EventView {
    EventView {
        id: event.id,
        calendar_id: event.calendar_id,
        uid: event.uid.clone(),
        master_event_id: event.master_event_id,
        recurrence_id: event.recurrence_id,
        recurrence_id_date: event.recurrence_id_date,
        summary: event.summary.clone(),
        description_html: event.description_html.clone(),
        description_text: event.description_text.clone(),
        url: event.url.clone(),
        starts_at: event.starts_at,
        ends_at: event.ends_at,
        start_date: event.start_date,
        end_date: event.end_date,
        tzid: event.tzid.clone(),
        all_day: event.all_day,
        rrule: event.rrule.clone(),
        rdate: event.rdate.clone(),
        exdate: event.exdate.clone(),
        status: event.status.clone(),
        priority: event.priority,
        class: event.class.clone(),
        transp: event.transp.clone(),
        categories: event.categories.clone(),
        category_details: event
            .categories
            .iter()
            .filter_map(|slug| {
                registry.get(slug).map(|info| CategoryDetailView {
                    slug: slug.clone(),
                    name: info.name.clone(),
                    color: info.color.clone(),
                })
            })
            .collect(),
        location_id: event.location_id,
        location: location.map(location_view),
        organizer_email: event.organizer_email.clone(),
        sequence: event.sequence,
        etag: etag.to_string(),
        created_at: event.created_at,
        updated_at: event.updated_at,
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
    }
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

/// One expanded occurrence: the (possibly exception) event plus its slot.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct OccurrenceView {
    event: EventView,
    occurrence: Option<OccurrencePoint>,
    is_exception: bool,
}

/// `{"kind": "timed", "at": ...}` or `{"kind": "all_day", "date": "..."}`;
/// the absent half is omitted from the payload (skip, not null).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct OccurrencePoint {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/calendars/{id}/events",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body = EventBody,
    responses(
        (status = 201, description = "created (master or RECURRENCE-ID exception)", body = EventView),
        (status = 400, description = "validation error"),
        (status = 404, description = "absent"),
    )
)]
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

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct RangeQuery {
    from: Option<chrono::DateTime<Utc>>,
    to: Option<chrono::DateTime<Utc>>,
    /// Occurrences only: comma-separated extras, e.g. `tasks,journals` —
    /// dated markers for tasks/journals appended to the event entries.
    include: Option<String>,
    // ponytail: no pagination yet; per-calendar windows stay small.
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}/events",
    params(
        ("id" = Uuid, Path, description = "calendar id"),
        ("from" = Option<chrono::DateTime<Utc>>, Query, description = "window start (default: now - 30d)"),
        ("to" = Option<chrono::DateTime<Utc>>, Query, description = "window end (default: now + 90d)"),
    ),
    responses(
        (status = 200, description = "events", body = Vec<EventView>),
        (status = 404, description = "absent"),
    )
)]
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
    let mut out: Vec<EventView> = Vec::with_capacity(events.len());
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
    Ok(Json(out))
}

#[utoipa::path(
    get,
    path = "/api/events/{id}",
    params(("id" = Uuid, Path, description = "event id")),
    responses(
        (status = 200, description = "event", body = EventView),
        (status = 404, description = "absent"),
    )
)]
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

#[utoipa::path(
    patch,
    path = "/api/events/{id}",
    params(
        ("id" = Uuid, Path, description = "event id"),
        ("if-match" = Option<String>, Header, description = "ETag for optimistic concurrency (412 on mismatch)"),
    ),
    request_body = EventBody,
    responses(
        (status = 200, description = "updated", body = EventView),
        (status = 400, description = "validation error or ETag mismatch"),
        (status = 404, description = "absent"),
    )
)]
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

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct SelfPartstatBody {
    /// NEEDS-ACTION, ACCEPTED, DECLINED, TENTATIVE.
    partstat: String,
}

/// RSVP endpoint for a signed-in attendee (stage 8a): resolves the caller's
/// attendee identity on the origin (by attendee user_id or their account
/// email — a delivered copy carries the same rows) and records the reply so
/// it reaches the organizer's clients via sync.
#[utoipa::path(
    patch,
    path = "/api/events/{id}/attendees/self",
    params(("id" = Uuid, Path, description = "event id")),
    request_body = SelfPartstatBody,
    responses((status = 200, description = "reply recorded", body = crate::OkView))
)]
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

#[utoipa::path(
    delete,
    path = "/api/events/{id}",
    params(
        ("id" = Uuid, Path, description = "event id"),
        ("if-match" = Option<String>, Header, description = "ETag for optimistic concurrency (412 on mismatch)"),
    ),
    responses((status = 200, description = "deleted (soft, sync-visible tombstone)", body = crate::OkView))
)]
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
#[utoipa::path(
    get,
    path = "/api/calendars/{id}/occurrences",
    params(
        ("id" = Uuid, Path, description = "calendar id"),
        ("from" = Option<chrono::DateTime<Utc>>, Query, description = "window start (default: now - 30d)"),
        ("to" = Option<chrono::DateTime<Utc>>, Query, description = "window end (default: now + 90d)"),
        ("include" = Option<String>, Query, description = "comma-separated extras: tasks,journals append dated markers (type: task/journal) to the event entries"),
    ),
    responses((status = 200, description = "occurrences", body = Vec<OccurrenceView>))
)]
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
    let mut out: Vec<serde_json::Value> =
        expand_occurrences_json(&pool, calendar_id, rows, from, to)
            .await?
            .into_iter()
            .map(|o| serde_json::to_value(o).expect("occurrence view serializes"))
            .collect();
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
        out.extend(
            include_task_markers(&pool, calendar_id, from, to)
                .await?
                .into_iter()
                .map(|m| serde_json::to_value(m).expect("task marker serializes")),
        );
    }
    if include.contains(&"journals") {
        out.extend(
            include_journal_markers(&pool, calendar_id, from, to)
                .await?
                .into_iter()
                .map(|m| serde_json::to_value(m).expect("journal marker serializes")),
        );
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
) -> Result<Vec<TaskMarkerView>, AppError> {
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
        out.push(TaskMarkerView {
            kind: "task",
            id: t.id,
            summary: t.summary.clone(),
            due: t
                .due_at
                .map(|d| MarkerPoint {
                    timed: Some(d),
                    all_day: None,
                })
                .or_else(|| {
                    t.due_date.map(|d| MarkerPoint {
                        timed: None,
                        all_day: Some(d.to_string()),
                    })
                }),
            status: t.status.clone(),
            percent: t.percent_complete,
            completed: t.completed_at,
            next_open,
            subtasks_count: counts.get(&t.uid).copied().unwrap_or(0),
        });
    }
    Ok(out)
}

/// One `{"timed": ...}` or `{"all_day": "..."}` slot; the absent half is
/// omitted from the payload (skip, not null).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct MarkerPoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    timed: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    all_day: Option<String>,
}

/// Calendar-view marker for a dated task (`?include=tasks`).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct TaskMarkerView {
    #[serde(rename = "type")]
    kind: &'static str,
    id: Uuid,
    summary: String,
    due: Option<MarkerPoint>,
    status: Option<String>,
    percent: Option<i16>,
    completed: Option<DateTime<Utc>>,
    next_open: Option<DateTime<Utc>>,
    subtasks_count: i64,
}

/// Calendar-view marker for a dated journal (`?include=journals`).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct JournalMarkerView {
    #[serde(rename = "type")]
    kind: &'static str,
    id: Uuid,
    summary: String,
    start: Option<MarkerPoint>,
    status: Option<String>,
}

/// Dated journal markers: journals whose DTSTART falls in the window.
/// Undated notes never appear.
async fn include_journal_markers(
    pool: &PgPool,
    calendar_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<JournalMarkerView>, AppError> {
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
        .map(|j| JournalMarkerView {
            kind: "journal",
            id: j.id,
            summary: j.summary.clone(),
            start: j
                .starts_at
                .map(|d| MarkerPoint {
                    timed: Some(d),
                    all_day: None,
                })
                .or_else(|| {
                    j.start_date.map(|d| MarkerPoint {
                        timed: None,
                        all_day: Some(d.to_string()),
                    })
                }),
            status: j.status.clone(),
        })
        .collect())
}

#[utoipa::path(
    get,
    path = "/api/subscriptions/{id}/occurrences",
    params(
        ("id" = Uuid, Path, description = "subscription id"),
        ("from" = Option<chrono::DateTime<Utc>>, Query, description = "window start (default: now - 30d)"),
        ("to" = Option<chrono::DateTime<Utc>>, Query, description = "window end (default: now + 90d)"),
    ),
    responses((status = 200, description = "PUBLIC-class occurrences of the subscribed calendar", body = Vec<OccurrenceView>))
)]
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
    Ok(Json(out))
}

/// Expanded occurrences with exceptions overlaid: each master occurrence
/// matching an exception's RECURRENCE-ID is replaced by that exception row.
async fn expand_occurrences_json(
    pool: &PgPool,
    calendar_id: Uuid,
    rows: Vec<db::EventRow>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<OccurrenceView>, AppError> {
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

    let mut out: Vec<OccurrenceView> = Vec::new();
    for event in &rows {
        // Exception rows surface only through their master's overlay.
        if event.master_event_id.is_some() {
            continue;
        }
        if event.rrule.is_none() {
            let location = db::location_for_event(pool, event).await;
            let view = event_view(
                event,
                &db::event_etag(event),
                &[],
                location.as_ref(),
                &registry,
            );
            let occurrence = match (event.starts_at, event.start_date) {
                (Some(at), _) => Some(OccurrencePoint {
                    kind: "timed",
                    at: Some(at),
                    date: None,
                }),
                (None, Some(date)) => Some(OccurrencePoint {
                    kind: "all_day",
                    at: None,
                    date: Some(date.to_string()),
                }),
                _ => None,
            };
            out.push(OccurrenceView {
                event: view,
                occurrence,
                is_exception: false,
            });
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
            let event_view = event_view(
                source,
                &db::event_etag(source),
                &[],
                location.as_ref(),
                &registry,
            );
            let occurrence = match point {
                calendar_core::DateOrDateTime::Timed(at) => OccurrencePoint {
                    kind: "timed",
                    at: Some(at),
                    date: None,
                },
                calendar_core::DateOrDateTime::AllDay(date) => OccurrencePoint {
                    kind: "all_day",
                    at: None,
                    date: Some(date.to_string()),
                },
            };
            out.push(OccurrenceView {
                event: event_view,
                occurrence: Some(occurrence),
                is_exception,
            });
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

/// OpenAPI for the events module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_event,
        list_events,
        get_event,
        patch_event,
        delete_event,
        patch_own_partstat,
        list_occurrences,
        list_subscription_occurrences,
    ),
    components(schemas(
        EventBody,
        LocationBody,
        SelfPartstatBody,
        EventView,
        LocationView,
        CategoryDetailView,
        OccurrenceView,
        OccurrencePoint,
        TaskMarkerView,
        JournalMarkerView,
        MarkerPoint,
        crate::OkView,
    ))
)]
pub(crate) struct EventsApi;
