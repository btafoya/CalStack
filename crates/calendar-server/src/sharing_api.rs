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

#[derive(serde::Deserialize)]
struct ShareBody {
    allows_caldav: Option<bool>,
    expires_at: Option<DateTime<Utc>>,
}

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
        Json(json!({
            "id": share.id,
            "token": token, // shown exactly once; only its sha256 is stored
            "allows_caldav": share.allows_caldav,
            "expires_at": share.expires_at,
        })),
    ))
}

async fn list_shares(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    let rows = db::sharing::list_shares_for_calendar(&pool, calendar_id).await?;
    Ok(Json(json!(
        rows.iter()
            .map(|s| json!({
                "id": s.id, "allows_caldav": s.allows_caldav,
                "expires_at": s.expires_at, "created_at": s.created_at,
            }))
            .collect::<Vec<_>>()
    )))
}

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

#[derive(serde::Deserialize)]
struct SubscribeBody {
    share_token: String,
    color: Option<String>,
}

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
        Json(json!({"id": row.id, "share_id": share.id})),
    ))
}

async fn list_subscriptions(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let views = db::sharing::list_subscriptions(&pool, auth.user.id).await?;
    Ok(Json(json!(
        views
            .iter()
            .map(|v| json!({
                "id": v.subscription.id,
                "calendar_name": v.calendar.name,
                "calendar_slug": v.calendar.slug,
                "color": v.subscription.color,
                "allows_caldav": v.share.allows_caldav,
            }))
            .collect::<Vec<_>>()
    )))
}

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

/// GET /share/{token}/calendar.ics — public read-only feed. Attendee contact
/// data and PRIVATE/CONFIDENTIAL events are withheld (privacy rules).
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
    let Ok(rows) = db::sharing::list_public_events(&pool, share.calendar_id).await else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "feed failed").into_response();
    };
    let mut exports: Vec<ExportRow> = Vec::new();
    for event in rows {
        // No attendee PII in public feeds.
        let alarms = db::alarms::list_alarms(&pool, event.id)
            .await
            .unwrap_or_default();
        exports.push(ExportRow {
            attendees: vec![],
            event,
            alarms,
        });
    }
    let ics = calendar_caldav::events_to_ics(&exports);
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
