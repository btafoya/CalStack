//! iTIP/iMIP scheduling (docs/PRD.md section 9; ADR-009): outbound REQUEST
//! recording + a Postmark inbound webhook for external attendee replies.
//! Generic SMTP stays outbound-only.

use crate::{AppError, AppState};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use base64::Engine;
use calendar_auth::Crypto;
use calendar_db::{self as db};
use serde_json::{Value, json};
use uuid::Uuid;

/// Renders the iTIP REQUEST body for an event (VCALENDAR with METHOD).
pub(crate) fn request_body(
    event: &db::EventRow,
    attendees: &[db::AttendeeRow],
    location: Option<db::LocationRow>,
) -> String {
    calendar_caldav::events_to_ics(&[calendar_caldav::ExportRow {
        event: event.clone(),
        attendees: attendees.to_vec(),
        alarms: vec![],
        location,
    }])
    .replacen("BEGIN:VCALENDAR", "BEGIN:VCALENDAR\nMETHOD:REQUEST", 1)
}

/// Inbound Postmark webhook (ADR-009): finds the text/calendar attachment,
/// matches the reply to its attendee, and records RSVP changes.
pub(crate) async fn postmark_inbound(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    // Shared secret protects the webhook (env POSTMARK_INBOUND_SECRET).
    let secret = std::env::var("POSTMARK_INBOUND_SECRET").unwrap_or_default();
    if !secret.is_empty() {
        let supplied = headers
            .get("x-postmark-inbound-secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if supplied != secret {
            return Err(AppError::unauthorized());
        }
    }
    let from = body
        .get("From")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let message_id = body
        .get("MessageID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Postmark delivers non-text parts as base64 attachments; calendar data
    // arrives with a text/calendar content type (or as an .ics attachment).
    let attachments = body
        .get("Attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for attachment in &attachments {
        let content_type = attachment
            .get("ContentType")
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = attachment.get("Name").and_then(Value::as_str).unwrap_or("");
        let is_calendar = content_type.to_ascii_lowercase().contains("calendar")
            || name.to_ascii_lowercase().ends_with(".ics");
        if !is_calendar {
            continue;
        }
        let raw = attachment
            .get("Content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&data).to_string();
        let Ok(events) = calendar_caldav::parse_ics(&text) else {
            continue;
        };
        for parsed in &events {
            let Some(event) = find_event_by_uid(&pool, &parsed.uid).await else {
                continue;
            };
            // Idempotent on (event, attendee, method, Message-ID).
            let recorded =
                db::scheduling::record_inbound(&pool, event.id, &from, "REPLY", &message_id)
                    .await?;
            if recorded.is_none() {
                continue; // duplicate delivery
            }
            if let Some(attendee) = &parsed.attendees.iter().find(|a| {
                a.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(&from))
            }) && let Some(partstat) = &attendee.partstat
            {
                db::scheduling::update_partstat(&pool, event.id, &from, partstat).await?;
            }
        }
    }
    Ok((StatusCode::OK, Json(json!({"ok": true}))).into_response())
}

async fn find_event_by_uid(pool: &sqlx::PgPool, uid: &str) -> Option<db::EventRow> {
    sqlx::query_as::<_, db::EventRow>(
        "SELECT * FROM events WHERE uid = $1 AND deleted_at IS NULL LIMIT 1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Sends every pending outbound iTIP message through the event tenant's
/// email provider (Postmark preferred, then SMTP). Idempotent: messages move
/// to `sent` exactly once.
pub(crate) async fn send_pending(pool: &sqlx::PgPool, crypto: Option<&Crypto>) {
    let Ok(pending) = db::scheduling::pending_outbound(pool).await else {
        return;
    };
    for message in pending {
        let Ok((event, _)) = db::get_event(pool, message.event_id).await else {
            db::scheduling::mark_message_status(pool, message.id, "failed", Some("event gone"))
                .await
                .ok();
            continue;
        };
        let tenant_id = match db::get_calendar(pool, event.calendar_id).await {
            Ok(cal) => Some(cal.tenant_id),
            Err(_) => None,
        };
        let Some(provider) = load_email_provider(pool, tenant_id, crypto).await else {
            break; // no provider configured: leave messages pending
        };
        let attendees = db::list_attendees(pool, event.id).await.unwrap_or_default();
        let location = db::location_for_event(pool, &event).await;
        let body = request_body(&event, &attendees, location);
        match provider
            .send(&message.attendee_email, &event.summary, &body)
            .await
        {
            Ok(()) => {
                db::scheduling::mark_message_status(pool, message.id, "sent", None)
                    .await
                    .ok();
            }
            Err(e) => {
                // Leave pending for retry; the durable job's backoff re-runs.
                db::scheduling::mark_message_status(
                    pool,
                    message.id,
                    "pending",
                    Some(&e.to_string()),
                )
                .await
                .ok();
                tracing::warn!(error = %e, "iMIP send failed; will retry");
                break;
            }
        }
    }
}

/// The tenant's first enabled Postmark/SMTP provider.
pub(crate) async fn load_email_provider(
    pool: &sqlx::PgPool,
    tenant_id: Option<Uuid>,
    crypto: Option<&Crypto>,
) -> Option<calendar_notify::EmailProvider> {
    let (tenant_id, crypto) = (tenant_id?, crypto?);
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        config_encrypted: Vec<u8>,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT kind, config_encrypted FROM notification_providers
         WHERE tenant_id = $1 AND enabled AND kind IN ('postmark', 'smtp')
         ORDER BY kind = 'postmark' DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let config: Value =
        serde_json::from_slice(&crypto.decrypt(&row.config_encrypted).ok()?).ok()?;
    let provider = calendar_notify::Provider::from_db_str(&row.kind)?;
    calendar_notify::EmailProvider::from_config(provider, &config).ok()
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new().route("/webhooks/postmark/inbound", post(postmark_inbound))
}
