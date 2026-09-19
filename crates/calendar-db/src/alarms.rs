//! VALARM storage and due-alarm scanning (schema in 0001; worker in
//! calendar-server). Alarm triggers are computed per occurrence in Rust —
//! recurrence expansion is not SQL's job (ADR-002).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, EventRow};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AlarmRow {
    pub id: Uuid,
    pub event_id: Uuid,
    pub action: String,          // DISPLAY | EMAIL
    pub related: Option<String>, // START | END
    pub offset_interval: Option<sqlx::postgres::types::PgInterval>,
    pub trigger_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub recipient_emails: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl AlarmRow {
    /// Relative offset in seconds (only plain time intervals are accepted on
    /// write, so this is loss-free).
    pub fn offset_secs(&self) -> Option<i64> {
        self.offset_interval
            .as_ref()
            .map(|i| i.microseconds / 1_000_000)
    }
}

/// Replaces all alarms of an event (VALARM set is fully owned by its resource)
/// inside the caller's transaction.
pub async fn replace_alarms(
    tx: &mut sqlx::PgConnection,
    event_id: Uuid,
    alarms: &[NewAlarm],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM event_alarms WHERE event_id = $1")
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    for alarm in alarms {
        sqlx::query(
            "INSERT INTO event_alarms
                (id, event_id, action, related, offset_interval, trigger_at,
                 description, summary, recipient_emails)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(&alarm.action)
        .bind(alarm.related.as_deref())
        .bind(
            alarm
                .offset_secs
                .map(|s| sqlx::postgres::types::PgInterval {
                    months: 0,
                    days: 0,
                    microseconds: s * 1_000_000,
                }),
        )
        .bind(alarm.trigger_at)
        .bind(&alarm.description)
        .bind(&alarm.summary)
        .bind(&alarm.recipient_emails)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

pub async fn list_alarms(pool: &PgPool, event_id: Uuid) -> Result<Vec<AlarmRow>, DbError> {
    sqlx::query_as::<_, AlarmRow>(
        "SELECT * FROM event_alarms WHERE event_id = $1 ORDER BY created_at",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[derive(Debug, Default)]
pub struct NewAlarm {
    pub action: String,          // DISPLAY | EMAIL
    pub related: Option<String>, // START | END
    pub offset_secs: Option<i64>,
    pub trigger_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub recipient_emails: Vec<String>,
}

/// Live alarms joined with their event and calendar, for the scan worker.
pub struct AlarmScanRow {
    pub alarm: AlarmRow,
    pub event: EventRow,
    pub calendar_id: Uuid,
}

/// Every alarm on a non-exception, non-all-day, non-deleted event.
pub async fn list_scannable_alarms(pool: &PgPool) -> Result<Vec<AlarmScanRow>, DbError> {
    #[derive(sqlx::FromRow)]
    struct Joined {
        #[sqlx(flatten)]
        alarm: AlarmRow,
        #[sqlx(flatten)]
        event: EventRow,
    }
    let rows = sqlx::query_as::<_, Joined>(
        "SELECT a.*, e.*
         FROM event_alarms a
         JOIN events e ON e.id = a.event_id AND e.deleted_at IS NULL
         JOIN calendars c ON c.id = e.calendar_id AND c.deleted_at IS NULL
         WHERE e.master_event_id IS NULL AND NOT e.all_day",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AlarmScanRow {
            calendar_id: r.event.calendar_id,
            alarm: r.alarm,
            event: r.event,
        })
        .collect())
}

/// Idempotent notification insert; returns true when it was created.
pub async fn create_notification_deduped(
    pool: &PgPool,
    user_id: Uuid,
    channel: &str,
    title: Option<&str>,
    body: Option<&str>,
    data: Option<serde_json::Value>,
    dedupe_key: &str,
) -> Result<bool, DbError> {
    let n = sqlx::query(
        "INSERT INTO notifications (id, user_id, channel, title, body, data, dedupe_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (dedupe_key) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(channel)
    .bind(title)
    .bind(body)
    .bind(data)
    .bind(dedupe_key)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}
