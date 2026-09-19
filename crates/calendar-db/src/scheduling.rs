//! iTIP/iMIP message log (docs/PRD.md section 9; ADR-009): outbound REQUEST/
//! CANCEL dedupe and inbound Postmark webhook ingestion.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{AttendeeRow, DbError, jobs};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleMessageRow {
    pub id: Uuid,
    pub event_id: Uuid,
    pub attendee_email: String,
    pub method: String,
    pub direction: String,
    pub status: String,
    pub message_id: Option<String>,
    pub error: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Records an outbound intent (idempotent on the unique key).
pub async fn record_outbound(
    pool: &PgPool,
    event_id: Uuid,
    attendee_email: &str,
    method: &str,
) -> Result<Option<ScheduleMessageRow>, DbError> {
    sqlx::query_as::<_, ScheduleMessageRow>(
        "INSERT INTO schedule_messages (id, event_id, attendee_email, method, direction)
         VALUES ($1, $2, $3, $4, 'outbound')
         ON CONFLICT (event_id, attendee_email, method, direction, message_id) DO NOTHING
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(event_id)
    .bind(attendee_email)
    .bind(method)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

/// Pending outbound messages (ready for the send job).
pub async fn pending_outbound(pool: &PgPool) -> Result<Vec<ScheduleMessageRow>, DbError> {
    sqlx::query_as::<_, ScheduleMessageRow>(
        "SELECT * FROM schedule_messages
         WHERE direction = 'outbound' AND status = 'pending'
         ORDER BY created_at LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn mark_message_status(
    pool: &PgPool,
    message_id: Uuid,
    status: &str,
    error: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE schedule_messages SET status = $2, error = $3,
            processed_at = CASE WHEN $2 IN ('sent', 'processed', 'failed') THEN now() ELSE processed_at END
         WHERE id = $1",
    )
    .bind(message_id)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Inbound message dedupe anchor on Message-ID; None when already seen.
pub async fn record_inbound(
    pool: &PgPool,
    event_id: Uuid,
    attendee_email: &str,
    method: &str,
    message_id: &str,
) -> Result<Option<ScheduleMessageRow>, DbError> {
    sqlx::query_as::<_, ScheduleMessageRow>(
        "INSERT INTO schedule_messages (id, event_id, attendee_email, method, direction, status, message_id)
         VALUES ($1, $2, $3, $4, 'inbound', 'received', $5)
         ON CONFLICT (event_id, attendee_email, method, direction, message_id) DO NOTHING
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(event_id)
    .bind(attendee_email)
    .bind(method)
    .bind(message_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

/// The attendee row for an event + email (for RSVP updates).
/// Records outbound REQUEST intents for all non-organizer attendees and
/// queues the send job. Call after event creation/update with attendees.
pub async fn schedule_requests(pool: &PgPool, event_id: Uuid) {
    let attendees = match list_attendees(pool, event_id).await {
        Ok(attendees) => attendees,
        Err(_) => return,
    };
    let (event, _) = match super::get_event(pool, event_id).await {
        Ok(event) => event,
        Err(_) => return,
    };
    let mut changed = false;
    for attendee in &attendees {
        // SMS-only attendees (no email) can't receive iMIP; skip.
        let Some(email) = attendee.email.as_deref() else {
            continue;
        };
        if email.eq_ignore_ascii_case(&event.organizer_email) {
            continue;
        }
        if record_outbound(pool, event_id, email, "REQUEST")
            .await
            .ok()
            .flatten()
            .is_some()
        {
            changed = true;
        }
    }
    if changed {
        jobs::enqueue(pool, "imip_send", serde_json::json!({}), None, 0)
            .await
            .ok();
    }
}

async fn list_attendees(pool: &PgPool, event_id: Uuid) -> Result<Vec<AttendeeRow>, DbError> {
    sqlx::query_as::<_, AttendeeRow>(
        "SELECT * FROM event_attendees WHERE event_id = $1 ORDER BY created_at",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn update_partstat(
    pool: &PgPool,
    event_id: Uuid,
    email: &str,
    partstat: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE event_attendees SET partstat = $3, updated_at = now()
         WHERE event_id = $1 AND email = $2",
    )
    .bind(event_id)
    .bind(email)
    .bind(partstat)
    .execute(pool)
    .await?;
    Ok(())
}
