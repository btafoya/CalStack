//! Public shares (ADR-004), anonymous feeds and inbound subscriptions
//! (docs/PRD.md section 5; checklist stage 14). Share tokens are shown once
//! and stored hashed; feeds withhold PRIVATE/CONFIDENTIAL events and
//! attendee contact data.

use crate::{AppError, AppState, require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{delete, get, post},
};
use calendar_caldav::ExportRow;
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

// ============ share management ============

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct ShareBody {
    allows_caldav: Option<bool>,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ShareCreatedView {
    id: Uuid,
    /// Shown exactly once; only its sha256 is stored.
    token: String,
    allows_caldav: bool,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ShareView {
    id: Uuid,
    allows_caldav: bool,
    expires_at: Option<DateTime<Utc>>,
    created_at: chrono::DateTime<Utc>,
}

#[utoipa::path(
    post,
    path = "/api/calendars/{id}/shares",
    params(("id" = Uuid, Path, description = "calendar id")),
    request_body = ShareBody,
    responses(
        (status = 201, description = "created; token shown once", body = ShareCreatedView),
        (status = 403, description = "not owner"),
        (status = 404, description = "absent"),
    )
)]
async fn create_share(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<ShareBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    let token = calendar_auth::generate_secret();
    let share = db::sharing::create_share(
        &pool,
        calendar_id,
        None, // ponytail: single-event shares land with the event view page
        &calendar_auth::sha256(token.as_bytes()),
        body.allows_caldav.unwrap_or(false),
        auth.user.id,
        body.expires_at,
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(ShareCreatedView {
            id: share.id,
            token, // shown exactly once; only its sha256 is stored
            allows_caldav: share.allows_caldav,
            expires_at: share.expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/calendars/{id}/shares",
    params(("id" = Uuid, Path, description = "calendar id")),
    responses(
        (status = 200, description = "shares (no tokens)", body = Vec<ShareView>),
        (status = 403, description = "not owner"),
    )
)]
async fn list_shares(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    let rows = db::sharing::list_shares_for_calendar(&pool, calendar_id).await?;
    Ok(Json(
        rows.iter()
            .map(|s| ShareView {
                id: s.id,
                allows_caldav: s.allows_caldav,
                expires_at: s.expires_at,
                created_at: s.created_at,
            })
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/calendars/{id}/shares/{share_id}",
    params(
        ("id" = Uuid, Path, description = "calendar id"),
        ("share_id" = Uuid, Path, description = "share id"),
    ),
    responses(
        (status = 200, description = "revoked", body = crate::OkView),
        (status = 403, description = "not owner"),
    )
)]
async fn revoke_share(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path((calendar_id, share_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    db::sharing::revoke_share(&pool, calendar_id, share_id).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ subscriptions ============

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct SubscribeBody {
    share_token: String,
    color: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct SubscribedView {
    id: Uuid,
    share_id: Uuid,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct SubscriptionView {
    id: Uuid,
    calendar_name: String,
    calendar_slug: String,
    color: Option<String>,
    allows_caldav: bool,
    /// false once the share or calendar is revoked/expired.
    live: bool,
}

#[utoipa::path(
    post,
    path = "/api/subscriptions",
    request_body = SubscribeBody,
    responses(
        (status = 201, description = "subscribed", body = SubscribedView),
        (status = 404, description = "unknown or dead share token"),
    )
)]
async fn subscribe(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let share =
        db::sharing::find_live_share(&pool, &calendar_auth::sha256(body.share_token.as_bytes()))
            .await
            .map_err(|_| AppError::NotFound)?;
    let row = db::sharing::create_subscription(&pool, auth.user.id, share.id, body.color).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(SubscribedView {
            id: row.id,
            share_id: share.id,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/subscriptions",
    responses(
        (status = 200, description = "the caller's subscriptions", body = Vec<SubscriptionView>),
    )
)]
async fn list_subscriptions(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let views = db::sharing::list_subscriptions(&pool, auth.user.id).await?;
    Ok(Json(
        views
            .iter()
            .map(|v| SubscriptionView {
                id: v.id,
                calendar_name: v.calendar_name.clone(),
                calendar_slug: v.calendar_slug.clone(),
                color: v.color.clone(),
                allows_caldav: v.allows_caldav,
                live: v.live,
            })
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/subscriptions/{id}",
    params(("id" = Uuid, Path, description = "subscription id")),
    responses((status = 200, description = "removed", body = crate::OkView))
)]
async fn unsubscribe(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(subscription_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    db::sharing::delete_subscription(&pool, auth.user.id, subscription_id).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ anonymous feed (no auth; share token is the capability) ============

/// RFC 5545 3.1 line folding: max 75 octets per line, continuations after a
/// single space. Char-boundary safe (never splits a UTF-8 sequence).
fn fold_ics_line(line: &str) -> String {
    if line.len() <= 75 {
        return format!("{line}\r\n");
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut width = 0usize;
    for ch in line.chars() {
        if width + ch.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(ch);
        width += ch.len_utf8();
    }
    out.push_str("\r\n");
    out
}

/// RFC 5545 3.3.11 TEXT escaping.
fn escape_ics_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn ics_prop(name: &str, value: &str) -> String {
    fold_ics_line(&format!("{name}:{}", escape_ics_text(value)))
}

/// DTSTART/DUE as a UTC instant (or bare wall clock when floating — the wall
/// time is stored as if UTC), or a VALUE=DATE form. The stored TZID is not
/// emitted: a UTC rendering is a correct instant for any zone.
fn moment_prop(
    name: &str,
    at: Option<DateTime<Utc>>,
    date: Option<chrono::NaiveDate>,
    floating: bool,
) -> String {
    if let Some(at) = at {
        let value = if floating {
            at.format("%Y%m%dT%H%M%S")
        } else {
            at.format("%Y%m%dT%H%M%SZ")
        };
        fold_ics_line(&format!("{name}:{value}"))
    } else if let Some(date) = date {
        fold_ics_line(&format!("{name};VALUE=DATE:{}", date.format("%Y%m%d")))
    } else {
        String::new()
    }
}

/// Minimal public-feed VTODO (docs/TASKS_JOURNALS_DESIGN.md section 6):
/// no extra_props, attendees or alarms — extras can carry private data.
/// TODO: swap to the shared calendar_caldav renderer (todos_to_ics) when it
/// lands; this inline version only covers the public-feed property set.
fn vtodo_text(task: &db::tasks::TaskRow) -> String {
    let mut out = String::from("BEGIN:VTODO\r\n");
    out.push_str(&ics_prop("UID", &task.uid));
    out.push_str(&format!(
        "DTSTAMP:{}\r\n",
        task.updated_at.format("%Y%m%dT%H%M%SZ")
    ));
    out.push_str(&ics_prop("SUMMARY", &task.summary));
    if let Some(status) = &task.status {
        out.push_str(&ics_prop("STATUS", status));
    }
    out.push_str(&moment_prop(
        "DTSTART",
        task.starts_at,
        task.start_date,
        task.floating,
    ));
    out.push_str(&moment_prop(
        "DUE",
        task.due_at,
        task.due_date,
        task.floating,
    ));
    out.push_str("END:VTODO\r\n");
    out
}

/// Minimal public-feed VJOURNAL: same privacy rule, undated journals ride
/// along with no DTSTART at all.
fn vjournal_text(journal: &db::journals::JournalRow) -> String {
    let mut out = String::from("BEGIN:VJOURNAL\r\n");
    out.push_str(&ics_prop("UID", &journal.uid));
    out.push_str(&format!(
        "DTSTAMP:{}\r\n",
        journal.updated_at.format("%Y%m%dT%H%M%SZ")
    ));
    out.push_str(&ics_prop("SUMMARY", &journal.summary));
    if let Some(status) = &journal.status {
        out.push_str(&ics_prop("STATUS", status));
    }
    out.push_str(&moment_prop(
        "DTSTART",
        journal.starts_at,
        journal.start_date,
        journal.floating,
    ));
    out.push_str("END:VJOURNAL\r\n");
    out
}

/// GET /share/{token}/calendar.ics — public read-only feed. Attendee contact
/// data and PRIVATE/CONFIDENTIAL events are withheld (privacy rules); PUBLIC
/// (or unclassified) tasks and journals are appended as minimal VTODO /
/// VJOURNAL components without extra_props, attendees or alarms.
pub(crate) async fn public_feed(
    State(AppState { pool, .. }): State<AppState>,
    Path(token): Path<String>,
) -> axum::response::Response {
    let Some(share) = db::sharing::find_live_share(&pool, &calendar_auth::sha256(token.as_bytes()))
        .await
        .ok()
    else {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    };
    let (rows, tasks, journals) = (
        db::sharing::list_public_events(&pool, share.calendar_id).await,
        db::sharing::list_public_tasks(&pool, share.calendar_id).await,
        db::sharing::list_public_journals(&pool, share.calendar_id).await,
    );
    let (rows, tasks, journals) = match (rows, tasks, journals) {
        (Ok(r), Ok(t), Ok(j)) => (r, t, j),
        _ => return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "feed failed").into_response(),
    };
    // Client-supplied VTIMEZONEs ride along so TZID-qualified events stay
    // interpretable in the public feed (ADR-012).
    let zones = db::timezones::list_for_calendar(&pool, share.calendar_id)
        .await
        .unwrap_or_default();
    let mut exports: Vec<ExportRow> = Vec::new();
    for event in rows {
        // No attendee PII in public feeds.
        let alarms = db::alarms::list_alarms(&pool, event.id)
            .await
            .unwrap_or_default();
        let location = db::location_for_event(&pool, &event).await;
        exports.push(ExportRow {
            attendees: vec![],
            event,
            alarms,
            location,
            vtimezones: zones.clone(),
        });
    }
    let mut ics = calendar_caldav::events_to_ics(&exports);
    let extras: String = tasks
        .iter()
        .map(vtodo_text)
        .chain(journals.iter().map(vjournal_text))
        .collect();
    if !extras.is_empty()
        && let Some(pos) = ics.rfind("END:VCALENDAR")
    {
        ics.insert_str(pos, &extras);
    }
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/calendar; charset=utf-8",
        )],
        ics,
    )
        .into_response()
}

// ============ router ============

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/calendars/{id}/shares",
            post(create_share).get(list_shares),
        )
        .route(
            "/api/calendars/{id}/shares/{share_id}",
            delete(revoke_share),
        )
        .route(
            "/api/subscriptions",
            post(subscribe).get(list_subscriptions),
        )
        .route("/api/subscriptions/{id}", delete(unsubscribe))
        .route("/share/{token}/calendar.ics", get(public_feed))
}

/// OpenAPI for the sharing module; merged into the served document in `main.rs`.
/// The anonymous `/share/{token}/calendar.ics` feed is a CalDAV-adjacent
/// protocol route, not part of the JSON API document.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_share,
        list_shares,
        revoke_share,
        subscribe,
        list_subscriptions,
        unsubscribe
    ),
    components(schemas(
        ShareBody,
        ShareCreatedView,
        ShareView,
        SubscribeBody,
        SubscribedView,
        SubscriptionView,
        crate::OkView,
    ))
)]
pub(crate) struct SharingApi;

#[cfg(test)]
mod feed_tests {
    use super::*;

    #[test]
    fn text_values_are_escaped() {
        // ICS injection: user text must not introduce properties or newlines.
        assert_eq!(escape_ics_text("a;b,c\nd\\e"), "a\\;b\\,c\\nd\\\\e");
    }

    #[test]
    fn long_lines_fold_at_75_octets() {
        let folded = fold_ics_line(&format!("SUMMARY:{}", "x".repeat(120)));
        for line in folded.split("\r\n") {
            assert!(line.len() <= 75, "unfolded line too long: {}", line.len());
        }
        assert!(folded.contains("\r\n "));
        // Multibyte text never splits mid-character.
        let wide = fold_ics_line(&format!("SUMMARY:{}", "é".repeat(60)));
        assert!(wide.contains("éé"));
    }

    #[test]
    fn moments_render_utc_date_or_floating() {
        let at = chrono::DateTime::from_timestamp(1_789_000_000, 0).unwrap();
        assert_eq!(
            moment_prop("DUE", Some(at), None, false),
            format!("DUE:{}\r\n", at.format("%Y%m%dT%H%M%SZ"))
        );
        assert_eq!(
            moment_prop("DUE", Some(at), None, true),
            format!("DUE:{}\r\n", at.format("%Y%m%dT%H%M%S"))
        );
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        assert_eq!(
            moment_prop("DUE", None, Some(day), false),
            "DUE;VALUE=DATE:20260920\r\n"
        );
        assert_eq!(moment_prop("DUE", None, None, false), "");
    }
}
