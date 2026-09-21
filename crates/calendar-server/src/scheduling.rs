//! iTIP/iMIP scheduling (docs/PRD.md section 9; ADR-009): outbound REQUEST/
//! CANCEL recording + a Postmark inbound webhook for external attendee
//! messages. Generic SMTP stays outbound-only.
//!
//! Trust boundary: the From address on an inbound reply comes from
//! Postmark's inbound pipeline, which performs SPF/DKIM/DMARC handling at
//! receipt. The server's check (From must match an attendee) is
//! authorization, not mail authentication — RSVP trust equals mail-pipeline
//! trust. Deployments that do not trust their inbound pipeline must not
//! configure the webhook.

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
use calendar_db::{self as db, scheduling::SubjectKind};
use serde_json::{Value, json};
use uuid::Uuid;

/// Renders an iTIP body for an event (VCALENDAR with the given METHOD:
/// REQUEST, CANCEL, ...).
pub(crate) fn itip_body(
    event: &db::EventRow,
    attendees: &[db::AttendeeRow],
    location: Option<db::LocationRow>,
    method: &str,
) -> String {
    calendar_caldav::events_to_ics(&[calendar_caldav::ExportRow {
        event: event.clone(),
        attendees: attendees.to_vec(),
        alarms: vec![],
        location,
        // ponytail: iTIP bodies for custom-tzid events stay zone-less; the
        // attendee's client falls back to the tzdb name. Thread stored
        // zones through here when zone-accurate iMIP bodies matter.
        vtimezones: vec![],
    }])
    .replacen(
        "BEGIN:VCALENDAR",
        &format!("BEGIN:VCALENDAR\r\nMETHOD:{method}"),
        1,
    )
}

/// The task-side twin of `itip_body`: METHOD:REQUEST/CANCEL VTODO bodies.
pub(crate) fn itip_todo_body(
    task: &db::tasks::TaskRow,
    attendees: &[db::tasks::TaskAttendeeRow],
    method: &str,
) -> String {
    calendar_caldav::todos_to_ics(&[calendar_caldav::TaskExportRow {
        task: task.clone(),
        attendees: attendees.to_vec(),
        alarms: vec![],
        vtimezones: vec![],
    }])
    .replacen(
        "BEGIN:VCALENDAR",
        &format!("BEGIN:VCALENDAR\r\nMETHOD:{method}"),
        1,
    )
}

/// Resolves the inbound iTIP method: absent METHOD means REPLY (legacy mail
/// clients send bare replies).
pub(crate) fn inbound_method(raw: Option<&str>) -> String {
    match raw.map(str::to_ascii_uppercase) {
        Some(method) if !method.is_empty() => method,
        _ => "REPLY".to_owned(),
    }
}

/// Stable dedupe key for inbound mail with no Message-ID: a hash over the
/// sender and the raw calendar text, so distinct replies never collapse onto
/// the empty string in the uniqueness key.
pub(crate) fn fallback_message_id(from: &str, calendar_text: &str) -> String {
    format!(
        "sha256-{}",
        calendar_auth::hex_encode(&calendar_auth::sha256(
            format!("{from}\n{calendar_text}").as_bytes()
        ))
    )
}

/// Shared-secret gate for the inbound webhook: fail-closed, so an unset
/// POSTMARK_INBOUND_SECRET disables the endpoint instead of opening it.
pub(crate) fn inbound_secret_ok(secret_env: &str, headers: &HeaderMap) -> bool {
    if secret_env.is_empty() {
        return false;
    }
    let supplied = headers
        .get("x-postmark-inbound-secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    crate::auth::constant_time_eq(supplied.as_bytes(), secret_env.as_bytes())
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
    if !inbound_secret_ok(&secret, &headers) {
        if secret.is_empty() {
            tracing::warn!("postmark inbound rejected: POSTMARK_INBOUND_SECRET not configured");
        }
        return Err(AppError::Forbidden);
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
        // Idempotent on (event, attendee, method, Message-ID); empty
        // Message-IDs fall back to a content hash so replies stay distinct.
        let message_id = if message_id.is_empty() {
            fallback_message_id(&from, &text)
        } else {
            message_id.clone()
        };
        for parsed in &events {
            let Some((kind, subject_id)) = find_subject_by_uid(&pool, &parsed.uid).await else {
                continue;
            };
            let method = inbound_method(parsed.method.as_deref());
            let recorded = db::scheduling::record_inbound(
                &pool,
                kind,
                subject_id,
                &from,
                &method,
                &message_id,
            )
            .await?;
            if recorded.is_none() {
                continue; // duplicate delivery
            }
            match method.as_str() {
                "REPLY" => {
                    if let Some(attendee) = &parsed.attendees.iter().find(|a| {
                        a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&from))
                    }) && let Some(partstat) = &attendee.partstat
                    {
                        db::scheduling::reply(&pool, kind, subject_id, &from, partstat).await?;
                    }
                }
                "CANCEL" => {
                    // Organizer cancelled: mark the subject, leave partstat alone.
                    let table = kind.table();
                    sqlx::query(&format!(
                        "UPDATE {table} SET status = 'CANCELLED', updated_at = now() WHERE id = $1"
                    ))
                    .bind(subject_id)
                    .execute(&pool)
                    .await
                    .map_err(db::DbError::from)?;
                }
                // Other iTIP methods are recorded only.
                _ => {}
            }
        }
    }
    Ok((StatusCode::OK, Json(json!({"ok": true}))).into_response())
}

/// The origin row for a UID, in events first, then tasks (stage 8c: inbound
/// replies find assigned tasks too). Copies never match (R7).
async fn find_subject_by_uid(pool: &sqlx::PgPool, uid: &str) -> Option<(SubjectKind, Uuid)> {
    let event: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM events WHERE uid = $1 AND deleted_at IS NULL AND origin_id IS NULL LIMIT 1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(id) = event {
        return Some((SubjectKind::Event, id));
    }
    let task: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM tasks WHERE uid = $1 AND deleted_at IS NULL AND origin_id IS NULL LIMIT 1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    task.map(|id| (SubjectKind::Task, id))
}

/// Sends every pending outbound iTIP message through the subject tenant's
/// email provider (Postmark preferred, then SMTP). Renders per kind: events
/// via VEVENT bodies, tasks via VTODO bodies. Idempotent: messages move to
/// `sent` exactly once.
pub(crate) async fn send_pending(pool: &sqlx::PgPool, crypto: Option<&Crypto>) {
    let Ok(pending) = db::scheduling::pending_outbound(pool).await else {
        return;
    };
    for message in pending {
        let (kind, subject_id) = message.subject();
        // Load the subject row and render per kind; a gone subject fails the
        // message instead of looping forever.
        let (summary, body, tenant_id) = match kind {
            SubjectKind::Event => match db::get_event(pool, subject_id).await {
                Ok((event, _)) => {
                    let tenant_id = db::get_calendar(pool, event.calendar_id)
                        .await
                        .ok()
                        .map(|cal| cal.tenant_id);
                    let attendees = db::list_attendees(pool, event.id).await.unwrap_or_default();
                    let location = db::location_for_event(pool, &event).await;
                    let body = itip_body(&event, &attendees, location, &message.method);
                    (event.summary, body, tenant_id)
                }
                Err(_) => {
                    db::scheduling::mark_message_status(
                        pool,
                        message.id,
                        "failed",
                        Some("subject gone"),
                    )
                    .await
                    .ok();
                    continue;
                }
            },
            SubjectKind::Task => match db::tasks::get_task(pool, subject_id).await {
                Ok((task, _)) => {
                    let tenant_id = db::get_calendar(pool, task.calendar_id)
                        .await
                        .ok()
                        .map(|cal| cal.tenant_id);
                    let attendees = db::tasks::list_task_attendees(pool, task.id)
                        .await
                        .unwrap_or_default();
                    let body = itip_todo_body(&task, &attendees, &message.method);
                    (task.summary, body, tenant_id)
                }
                Err(_) => {
                    db::scheduling::mark_message_status(
                        pool,
                        message.id,
                        "failed",
                        Some("subject gone"),
                    )
                    .await
                    .ok();
                    continue;
                }
            },
        };
        let Some(provider) = load_email_provider(pool, tenant_id, crypto).await else {
            break; // no provider configured: leave messages pending
        };
        match provider
            .send(&message.attendee_email, &summary, &body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(secret: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(secret) = secret {
            headers.insert(
                "x-postmark-inbound-secret",
                HeaderValue::from_str(secret).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn empty_secret_is_fail_closed_even_with_a_header() {
        assert!(!inbound_secret_ok("", &headers_with(Some("anything"))));
    }

    #[test]
    fn correct_secret_passes() {
        assert!(inbound_secret_ok("s3cret", &headers_with(Some("s3cret"))));
    }

    #[test]
    fn wrong_secret_and_missing_header_rejected() {
        assert!(!inbound_secret_ok("s3cret", &headers_with(Some("nope"))));
        assert!(!inbound_secret_ok("s3cret", &headers_with(None)));
    }

    #[test]
    fn absent_method_means_reply_and_others_pass_through() {
        assert_eq!(inbound_method(None), "REPLY");
        assert_eq!(inbound_method(Some("")), "REPLY");
        assert_eq!(inbound_method(Some("REPLY")), "REPLY");
        assert_eq!(inbound_method(Some("cancel")), "CANCEL");
        assert_eq!(inbound_method(Some("Request")), "REQUEST");
    }

    #[test]
    fn fallback_message_id_is_stable_and_distinguishes_inputs() {
        let first = fallback_message_id("al@example.com", "BEGIN:VCALENDAR");
        assert_eq!(
            first,
            fallback_message_id("al@example.com", "BEGIN:VCALENDAR")
        );
        assert_ne!(
            first,
            fallback_message_id("al@example.com", "different text")
        );
        assert_ne!(
            first,
            fallback_message_id("sam@example.com", "BEGIN:VCALENDAR")
        );
        assert!(first.starts_with("sha256-"));
    }
}
