//! VALARM storage and due-alarm scanning (schema in 0001; worker in
//! calendar-server). Alarm triggers are computed per occurrence in Rust —
//! recurrence expansion is not SQL's job (ADR-002).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::DbError;
use calendar_core::DateOrDateTime;

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
    /// App-side channel selection; sms/push never reach the ICS wire.
    /// DISPLAY/EMAIL wire alarms map to {in_app}/{in_app,email} on import.
    pub notify_channels: Vec<String>,
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
                 description, summary, recipient_emails, notify_channels)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
        .bind(&alarm.notify_channels)
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
    /// App-side channel selection; see AlarmRow.notify_channels.
    pub notify_channels: Vec<String>,
}

/// The alarm row of a scan union, stripped of its subject FK (the FK is
/// `ScanRow.subject_id`, which is an event id or a task id by `kind`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScanAlarm {
    pub id: Uuid,
    pub action: String,          // DISPLAY | EMAIL
    pub related: Option<String>, // START | END
    pub offset_interval: Option<sqlx::postgres::types::PgInterval>,
    pub trigger_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub recipient_emails: Vec<String>,
    pub notify_channels: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl ScanAlarm {
    /// Relative offset in seconds (only plain time intervals are accepted on
    /// write, so this is loss-free).
    pub fn offset_secs(&self) -> Option<i64> {
        self.offset_interval
            .as_ref()
            .map(|i| i.microseconds / 1_000_000)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanKind {
    Event,
    Task,
}

/// One alarm of the scan union (design §6): the alarm plus the scheduling
/// fields of the event or task it fires for. `start`/`end` are the anchors —
/// events: DTSTART / DTEND; tasks: DTSTART-else-DUE / DUE (`related = END`
/// maps to DUE). All-day anchors carry their date; the worker maps it to a
/// midnight instant in the subject's timezone.
#[derive(Debug, Clone)]
pub struct ScanRow {
    pub kind: ScanKind,
    pub subject_id: Uuid,
    pub calendar_id: Uuid,
    pub summary: String,
    pub alarm: ScanAlarm,
    /// START anchor: the event start, or the task's DTSTART-else-DUE.
    pub start: Option<DateOrDateTime>,
    /// END anchor: the event end, or the task's DUE.
    pub end: Option<DateOrDateTime>,
    pub rrule: Option<String>,
    pub rdate: serde_json::Value,
    pub exdate: serde_json::Value,
    pub tzid: Option<String>,
    /// Wall clock stored as if UTC (no tzid semantics).
    pub floating: bool,
    /// Completed RECURRENCE-ID occurrences of a recurring task master; their
    /// occurrences are skipped during expansion (design §6).
    pub completed_occurrences: Vec<super::tasks::Occurrence>,
}

fn point(at: Option<DateTime<Utc>>, date: Option<NaiveDate>) -> Option<DateOrDateTime> {
    match (at, date) {
        (Some(at), _) => Some(DateOrDateTime::Timed(at)),
        (None, Some(date)) => Some(DateOrDateTime::AllDay(date)),
        _ => None,
    }
}

#[derive(sqlx::FromRow)]
struct EventScan {
    alarm_id: Uuid,
    action: String,
    related: Option<String>,
    offset_interval: Option<sqlx::postgres::types::PgInterval>,
    trigger_at: Option<DateTime<Utc>>,
    alarm_description: Option<String>,
    alarm_summary: Option<String>,
    recipient_emails: Vec<String>,
    notify_channels: Vec<String>,
    alarm_created_at: DateTime<Utc>,
    subject_id: Uuid,
    calendar_id: Uuid,
    summary: String,
    starts_at: Option<DateTime<Utc>>,
    start_date: Option<NaiveDate>,
    ends_at: Option<DateTime<Utc>>,
    end_date: Option<NaiveDate>,
    rrule: Option<String>,
    rdate: serde_json::Value,
    exdate: serde_json::Value,
    tzid: Option<String>,
    floating: bool,
}

impl EventScan {
    fn into_scan_row(self) -> ScanRow {
        ScanRow {
            kind: ScanKind::Event,
            subject_id: self.subject_id,
            calendar_id: self.calendar_id,
            summary: self.summary,
            alarm: ScanAlarm {
                id: self.alarm_id,
                action: self.action,
                related: self.related,
                offset_interval: self.offset_interval,
                trigger_at: self.trigger_at,
                description: self.alarm_description,
                summary: self.alarm_summary,
                recipient_emails: self.recipient_emails,
                notify_channels: self.notify_channels,
                created_at: self.alarm_created_at,
            },
            start: point(self.starts_at, self.start_date),
            end: point(self.ends_at, self.end_date),
            rrule: self.rrule,
            rdate: self.rdate,
            exdate: self.exdate,
            tzid: self.tzid,
            floating: self.floating,
            completed_occurrences: Vec::new(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct TaskScan {
    alarm_id: Uuid,
    action: String,
    related: Option<String>,
    offset_interval: Option<sqlx::postgres::types::PgInterval>,
    trigger_at: Option<DateTime<Utc>>,
    alarm_description: Option<String>,
    alarm_summary: Option<String>,
    recipient_emails: Vec<String>,
    notify_channels: Vec<String>,
    alarm_created_at: DateTime<Utc>,
    subject_id: Uuid,
    calendar_id: Uuid,
    summary: String,
    master_task_id: Option<Uuid>,
    starts_at: Option<DateTime<Utc>>,
    start_date: Option<NaiveDate>,
    due_at: Option<DateTime<Utc>>,
    due_date: Option<NaiveDate>,
    rrule: Option<String>,
    rdate: serde_json::Value,
    exdate: serde_json::Value,
    tzid: Option<String>,
    floating: bool,
}

impl TaskScan {
    fn into_scan_row(self, completed: Vec<super::tasks::Occurrence>) -> ScanRow {
        ScanRow {
            kind: ScanKind::Task,
            subject_id: self.subject_id,
            calendar_id: self.calendar_id,
            summary: self.summary,
            alarm: ScanAlarm {
                id: self.alarm_id,
                action: self.action,
                related: self.related,
                offset_interval: self.offset_interval,
                trigger_at: self.trigger_at,
                description: self.alarm_description,
                summary: self.alarm_summary,
                recipient_emails: self.recipient_emails,
                notify_channels: self.notify_channels,
                created_at: self.alarm_created_at,
            },
            // DTSTART, else DUE (design decision 4) — the task_anchor rule.
            start: point(self.starts_at, self.start_date)
                .or_else(|| point(self.due_at, self.due_date)),
            end: point(self.due_at, self.due_date),
            rrule: self.rrule,
            rdate: self.rdate,
            exdate: self.exdate,
            tzid: self.tzid,
            floating: self.floating,
            completed_occurrences: completed,
        }
    }
}

/// Completed occurrences (RECURRENCE-ID wall clock, like `next_open`) of a
/// task series: the master's, or the row's own when it is an override.
async fn completed_task_occurrences(
    pool: &PgPool,
    task: &TaskScan,
) -> Vec<super::tasks::Occurrence> {
    let master = task.master_task_id.unwrap_or(task.subject_id);
    super::tasks::list_overrides(pool, master)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.status.as_deref() == Some("COMPLETED") || o.completed_at.is_some())
        .filter_map(|o| match (o.recurrence_id, o.recurrence_id_date) {
            (Some(at), _) => Some(super::tasks::Occurrence::Timed(at)),
            (None, Some(d)) => Some(super::tasks::Occurrence::AllDay(d)),
            (None, None) => None,
        })
        .collect()
}

/// Every alarm on a live (non-deleted) event or task, as a union of `ScanRow`
/// (design §6). Masters and RECURRENCE-ID overrides alike are returned —
/// override rows own their alarms, and the scan anchors their triggers on the
/// override's own times. All-day subjects are included; the worker maps their
/// date to a midnight instant in the subject's timezone. Tasks with STATUS
/// COMPLETED/CANCELLED (including completed recurring overrides) are excluded;
/// absolute triggers always survive that on a live task.
pub async fn list_scannable_alarms(pool: &PgPool) -> Result<Vec<ScanRow>, DbError> {
    let events: Vec<EventScan> = sqlx::query_as(
        "SELECT a.id AS alarm_id, a.action, a.related, a.offset_interval, a.trigger_at,
                a.description AS alarm_description, a.summary AS alarm_summary,
                a.recipient_emails, a.notify_channels, a.created_at AS alarm_created_at,
                e.id AS subject_id, e.calendar_id, e.summary,
                e.starts_at, e.start_date, e.ends_at, e.end_date,
                e.rrule, e.rdate, e.exdate, e.tzid, e.floating
         FROM event_alarms a
         JOIN events e ON e.id = a.event_id AND e.deleted_at IS NULL
         JOIN calendars c ON c.id = e.calendar_id AND c.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut rows: Vec<ScanRow> = events.into_iter().map(EventScan::into_scan_row).collect();
    let tasks: Vec<TaskScan> = sqlx::query_as(
        "SELECT ta.id AS alarm_id, ta.action, ta.related, ta.offset_interval, ta.trigger_at,
                ta.description AS alarm_description, ta.summary AS alarm_summary,
                ta.recipient_emails, ta.notify_channels, ta.created_at AS alarm_created_at,
                t.id AS subject_id, t.calendar_id, t.summary, t.master_task_id,
                t.starts_at, t.start_date, t.due_at, t.due_date,
                t.rrule, t.rdate, t.exdate, t.tzid, t.floating
         FROM task_alarms ta
         JOIN tasks t ON t.id = ta.task_id AND t.deleted_at IS NULL
           AND t.status IS DISTINCT FROM 'COMPLETED'
           AND t.status IS DISTINCT FROM 'CANCELLED'
           AND t.completed_at IS NULL
         JOIN calendars c ON c.id = t.calendar_id AND c.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for task in tasks {
        let completed = if task.rrule.is_some() {
            completed_task_occurrences(pool, &task).await
        } else {
            Vec::new()
        };
        rows.push(task.into_scan_row(completed));
    }
    Ok(rows)
}

/// Idempotent notification insert; returns true when it was created.
/// External recipients (channel rows without a user) pass None.
pub async fn create_notification_deduped(
    pool: &PgPool,
    user_id: Option<Uuid>,
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

/// Crash-recovery claim for dispatch: a row mid-send when the process died
/// must not re-send until its 60-second lease expires. Single worker per DB
/// by design — this closes the crash-between-send-and-sent_at window, not
/// multi-instance fan-out.
pub async fn claim_notification(pool: &PgPool, id: Uuid) -> Result<bool, DbError> {
    let n = sqlx::query(
        "UPDATE notifications SET claimed_until = now() + interval '60 seconds'
         WHERE id = $1 AND (claimed_until IS NULL OR claimed_until < now())",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SubsecRound;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::migrate(&pool).await.ok()?;
        Some(pool)
    }

    /// Minimal user + tenant + calendar, mirroring the tasks.rs fixture.
    struct Fixture {
        user: Uuid,
        calendar: Uuid,
    }

    async fn fixture(pool: &sqlx::PgPool) -> Fixture {
        let f = Fixture {
            user: Uuid::new_v4(),
            calendar: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO users (id, username, email) VALUES ($1, $2, $3)")
            .bind(f.user)
            .bind(format!("u-{}", f.user.simple()))
            .bind(format!("{}@alarms.test", f.user))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $2, true)")
            .bind(f.user)
            .bind(f.user.simple().to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(f.user)
        .bind(f.user)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendars (id, tenant_id, slug, name, created_by) VALUES ($1, $2, $3, $3, $4)",
        )
        .bind(f.calendar)
        .bind(f.user)
        .bind(f.user.simple().to_string())
        .bind(f.user)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_acl (calendar_id, principal_user_id, capability, can_manage_acl)
             VALUES ($1, $2, 'owner', true)",
        )
        .bind(f.calendar)
        .bind(f.user)
        .execute(pool)
        .await
        .unwrap();
        f
    }

    fn display_alarm(related: Option<&str>, offset_secs: i64) -> NewAlarm {
        NewAlarm {
            action: "DISPLAY".into(),
            related: related.map(str::to_string),
            offset_secs: Some(offset_secs),
            notify_channels: vec!["in_app".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn task_alarm_scans_with_due_anchor_and_completed_tasks_are_skipped() {
        let Some(pool) = test_pool().await else {
            return; // no DATABASE_URL: skip
        };
        let f = fixture(&pool).await;
        // timestamptz keeps microseconds; truncate so comparisons against the
        // re-read row are exact.
        let due = (Utc::now() + chrono::Duration::hours(2)).trunc_subsecs(6);
        let (task, _) = crate::tasks::create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[display_alarm(Some("END"), -600)],
            &crate::tasks::NewTaskData {
                uid: Uuid::new_v4().to_string(),
                due_at: Some(due),
                summary: "file taxes".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let rows = list_scannable_alarms(&pool).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.subject_id == task.id)
            .expect("task alarm must be scannable");
        assert_eq!(row.kind, ScanKind::Task);
        assert_eq!(row.summary, "file taxes");
        // related = END anchors on DUE.
        assert_eq!(row.end, Some(DateOrDateTime::Timed(due)));
        // DTSTART absent: the START anchor falls back to DUE.
        assert_eq!(row.start, Some(DateOrDateTime::Timed(due)));
        assert!(row.completed_occurrences.is_empty());

        // Completing the task removes its alarms from the scan.
        crate::tasks::complete_task(&pool, task.id, None, f.user)
            .await
            .unwrap();
        let rows = list_scannable_alarms(&pool).await.unwrap();
        assert!(rows.iter().all(|r| r.subject_id != task.id));
    }

    #[tokio::test]
    async fn recurring_task_alarm_carries_completed_overrides() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let start = (Utc::now() - chrono::Duration::days(1)).trunc_subsecs(6);
        let (master, _) = crate::tasks::create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[display_alarm(Some("START"), -900)],
            &crate::tasks::NewTaskData {
                uid: Uuid::new_v4().to_string(),
                rrule: Some("FREQ=DAILY;COUNT=5".into()),
                starts_at: Some(start),
                due_at: Some(start + chrono::Duration::hours(1)),
                summary: "water plants".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = crate::tasks::Occurrence::Timed(start.naive_utc());
        crate::tasks::complete_task(&pool, master.id, Some(first), f.user)
            .await
            .unwrap();
        let rows = list_scannable_alarms(&pool).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.subject_id == master.id)
            .expect("live recurring master still scans");
        assert_eq!(row.completed_occurrences, vec![first]);
    }

    #[tokio::test]
    async fn all_day_task_due_anchors_as_a_date() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let due = Utc::now().date_naive() + chrono::Duration::days(1);
        let (task, _) = crate::tasks::create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[display_alarm(Some("END"), 0)],
            &crate::tasks::NewTaskData {
                uid: Uuid::new_v4().to_string(),
                due_date: Some(due),
                summary: "all-day chore".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let rows = list_scannable_alarms(&pool).await.unwrap();
        let row = rows.iter().find(|r| r.subject_id == task.id).unwrap();
        assert_eq!(row.end, Some(DateOrDateTime::AllDay(due)));
        // No tzid: the worker maps the date to UTC midnight (floating as if UTC).
        assert_eq!(row.tzid, None);
    }

    #[tokio::test]
    async fn claim_holds_row_until_lease_expires() {
        let Some(pool) = test_pool().await else {
            return; // no DATABASE_URL: skip
        };
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO notifications (id, user_id, channel, dedupe_key)
             VALUES ($1, NULL, 'email', $2)",
        )
        .bind(id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        // First claim wins, second (crashed worker's lease) is refused.
        assert!(claim_notification(&pool, id).await.unwrap());
        assert!(!claim_notification(&pool, id).await.unwrap());
        // Releasing the claim (send failure path) makes it claimable again.
        sqlx::query("UPDATE notifications SET claimed_until = NULL WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(claim_notification(&pool, id).await.unwrap());
        sqlx::query("DELETE FROM notifications WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
}
