//! iTIP/iMIP scheduling (docs/PRD.md section 9, ADR-009; docs/
//! TASKS_JOURNALS_DESIGN.md section 6, stages 8a-8c): one dispatch primitive
//! for events and tasks — outbound REQUEST/CANCEL intents with sequence
//! dedupe plus internal delivery as organizer-rebuilt copies — and one reply
//! primitive that makes RSVPs visible to sync.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, etag_for, record_change};

/// Which table a scheduling subject lives in. Table and column names come
/// from this enum, never from user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    Event,
    Task,
}

impl SubjectKind {
    pub fn table(self) -> &'static str {
        match self {
            SubjectKind::Event => "events",
            SubjectKind::Task => "tasks",
        }
    }

    pub fn attendee_table(self) -> &'static str {
        match self {
            SubjectKind::Event => "event_attendees",
            SubjectKind::Task => "task_attendees",
        }
    }

    pub fn alarm_table(self) -> &'static str {
        match self {
            SubjectKind::Event => "event_alarms",
            SubjectKind::Task => "task_alarms",
        }
    }

    /// The schedule_messages FK column.
    pub fn fk(self) -> &'static str {
        match self {
            SubjectKind::Event => "event_id",
            SubjectKind::Task => "task_id",
        }
    }

    /// The master-pointer column (master_event_id / master_task_id).
    pub fn master_col(self) -> &'static str {
        match self {
            SubjectKind::Event => "master_event_id",
            SubjectKind::Task => "master_task_id",
        }
    }

    /// The calendar `components` member that gates the default collection.
    pub fn component(self) -> &'static str {
        match self {
            SubjectKind::Event => "VEVENT",
            SubjectKind::Task => "VTODO",
        }
    }

    pub fn resource_type(self) -> &'static str {
        match self {
            SubjectKind::Event => "event",
            SubjectKind::Task => "task",
        }
    }
}

// ============ message log ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleMessageRow {
    pub id: Uuid,
    pub event_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub sequence: Option<i32>,
    pub attendee_email: String,
    pub method: String,
    pub direction: String,
    pub status: String,
    pub message_id: Option<String>,
    pub error: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ScheduleMessageRow {
    /// The subject this message belongs to (0017 CHECK: exactly one is set).
    pub fn subject(&self) -> (SubjectKind, Uuid) {
        match (self.event_id, self.task_id) {
            (_, Some(id)) => (SubjectKind::Task, id),
            (Some(id), _) => (SubjectKind::Event, id),
            (None, None) => unreachable!("schedule_messages subject CHECK"),
        }
    }
}

/// Records an outbound intent (idempotent on the (subject, attendee, method,
/// direction, sequence, message_id) index — the design section 6 dedupe key).
pub async fn record_outbound(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    attendee_email: &str,
    method: &str,
    sequence: i32,
) -> Result<Option<ScheduleMessageRow>, DbError> {
    let sql = format!(
        "INSERT INTO schedule_messages (id, {}, attendee_email, method, direction, sequence)
         VALUES ($1, $2, $3, $4, 'outbound', $5)
         ON CONFLICT DO NOTHING
         RETURNING *",
        kind.fk()
    );
    sqlx::query_as::<_, ScheduleMessageRow>(&sql)
        .bind(Uuid::new_v4())
        .bind(subject_id)
        .bind(attendee_email)
        .bind(method)
        .bind(sequence)
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

/// Inbound message dedupe anchors on Message-ID (or the caller-supplied
/// fallback hash when the mail has none). Uniqueness is the (subject,
/// attendee, method, direction, sequence, message_id) index.
pub async fn record_inbound(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    attendee_email: &str,
    method: &str,
    message_id: &str,
) -> Result<Option<ScheduleMessageRow>, DbError> {
    let sql = format!(
        "INSERT INTO schedule_messages (id, {}, attendee_email, method, direction, status, message_id)
         VALUES ($1, $2, $3, $4, 'inbound', 'received', $5)
         ON CONFLICT DO NOTHING
         RETURNING *",
        kind.fk()
    );
    sqlx::query_as::<_, ScheduleMessageRow>(&sql)
        .bind(Uuid::new_v4())
        .bind(subject_id)
        .bind(attendee_email)
        .bind(method)
        .bind(message_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

// ============ subject loading ============

#[derive(Debug, Clone, sqlx::FromRow)]
struct SubjectRow {
    id: Uuid,
    sequence: i32,
    summary: String,
    organizer_user_id: Option<Uuid>,
    organizer_email: Option<String>,
}

async fn load_subject(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
) -> Result<Option<SubjectRow>, DbError> {
    // Design guard R7: only origin rows dispatch or receive replies; a copy
    // id is silently not a subject.
    let sql = format!(
        "SELECT id, sequence, summary, organizer_user_id,
                organizer_email::text AS organizer_email
         FROM {} WHERE id = $1 AND origin_id IS NULL AND deleted_at IS NULL FOR UPDATE",
        kind.table()
    );
    sqlx::query_as::<_, SubjectRow>(&sql)
        .bind(subject_id)
        .fetch_optional(tx)
        .await
        .map_err(Into::into)
}

/// The organizer check: by stored user id, else by email match.
async fn is_organizer(pool: &PgPool, subject: &SubjectRow, actor: Uuid) -> bool {
    if subject.organizer_user_id == Some(actor) {
        return true;
    }
    let Some(expected) = subject.organizer_email.as_deref() else {
        return false;
    };
    let actor_email: Option<String> =
        sqlx::query_scalar("SELECT email::text FROM users WHERE id = $1")
            .bind(actor)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    actor_email
        .map(|e| e.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

// ============ dispatch (design section 6) ============

/// The single scheduling entry point, called from the API create/patch/delete
/// and DAV flush/DELETE paths for both kinds. Runs only for the organizer's
/// own object (`origin_id IS NULL`, actor is the organizer). Per non-organizer
/// attendee with an email, diffs against the schedule_messages history by
/// (attendee, sequence):
///
/// - internal attendee: (re)build a copy into their default collection; no
///   email is ever sent to them;
/// - external attendee: REQUEST when new, updated REQUEST when the row's
///   SEQUENCE moved past their last-sent sequence, CANCEL when removed;
/// - copies of removed attendees are set to STATUS:CANCELLED and notified.
///
/// No-op (Ok) when the organizer guard fails; `DbError::NotFound` when the
/// row is gone.
pub async fn dispatch(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    actor: Uuid,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let subject = load_subject(&mut tx, kind, subject_id)
        .await?
        .ok_or(DbError::NotFound)?;
    if !is_organizer(pool, &subject, actor).await {
        return Ok(()); // only the organizer's own object dispatches
    }
    let attendees = list_subject_attendees(&mut tx, kind, subject_id).await?;

    let mut queued = false;
    let mut current: HashSet<String> = HashSet::new();
    let mut internal_users: HashSet<Uuid> = HashSet::new();
    for (email, user_id) in &attendees {
        let Some(email) = email else { continue };
        if email.eq_ignore_ascii_case(subject.organizer_email.as_deref().unwrap_or("")) {
            continue;
        }
        current.insert(email.to_lowercase());
        let user = match user_id {
            Some(u) => Some(*u),
            None => lookup_user_id(&mut tx, email).await?,
        };
        // Internal delivery only when the user owns a collection that allows
        // the kind (design: otherwise treated as external).
        if let Some(user) = user
            && let Some(calendar) = default_collection(&mut tx, user, kind.component()).await?
        {
            internal_users.insert(user);
            rebuild_copy(&mut tx, kind, &subject, calendar, user).await?;
            continue;
        }
        if request_needed(&mut tx, kind, subject_id, email, subject.sequence).await? {
            record_outbound_tx(
                &mut tx,
                kind,
                subject_id,
                email,
                "REQUEST",
                subject.sequence,
            )
            .await?;
            queued = true;
        }
    }

    // Attendees removed from the object: external ones get METHOD:CANCEL,
    // former internal ones have their copy cancelled below.
    for email in past_request_attendees(&mut tx, kind, subject_id).await? {
        if current.contains(&email.to_lowercase()) {
            continue;
        }
        if lookup_user_id(&mut tx, &email).await?.is_some() {
            continue; // former internal: handled via copies
        }
        if !cancel_recorded(&mut tx, kind, subject_id, &email).await? {
            record_outbound_tx(
                &mut tx,
                kind,
                subject_id,
                &email,
                "CANCEL",
                subject.sequence,
            )
            .await?;
            queued = true;
        }
    }

    // Copies of attendees no longer invited (and of dropped overrides): the
    // organizer's removal is authoritative.
    cancel_stray_copies(&mut tx, kind, &subject, &current, &internal_users).await?;

    tx.commit().await?;
    if queued {
        super::jobs::enqueue(pool, "imip_send", serde_json::json!({}), None, 0)
            .await
            .ok();
    }
    Ok(())
}

/// Cancel path: the object was deleted (or the meeting cancelled). Records
/// METHOD:CANCEL for every external attendee and sets every live copy to
/// STATUS:CANCELLED (SEQUENCE bumped, still visible, RFC 5546) with an in-app
/// notification. Called with the actor for the organizer guard; the row may
/// already be soft-deleted, so it is loaded without the liveness filter.
pub async fn dispatch_cancel(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    actor: Uuid,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "SELECT id, calendar_id, sequence, summary, organizer_user_id,
                organizer_email::text AS organizer_email
         FROM {} WHERE id = $1 AND origin_id IS NULL FOR UPDATE",
        kind.table()
    );
    let subject = sqlx::query_as::<_, SubjectRow>(&sql)
        .bind(subject_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DbError::NotFound)?;
    if !is_organizer(pool, &subject, actor).await {
        return Ok(());
    }
    let attendees = list_subject_attendees(&mut tx, kind, subject_id).await?;
    let mut queued = false;
    for (email, user_id) in &attendees {
        let Some(email) = email else { continue };
        if email.eq_ignore_ascii_case(subject.organizer_email.as_deref().unwrap_or("")) {
            continue;
        }
        let internal = match user_id {
            Some(u) => Some(*u),
            None => lookup_user_id(&mut tx, email).await?,
        };
        if let Some(user) = internal
            && let Some(calendar) = default_collection(&mut tx, user, kind.component()).await?
        {
            cancel_copies_for(&mut tx, kind, subject_id, calendar, user, &subject.summary).await?;
            continue;
        }
        record_outbound_tx(&mut tx, kind, subject_id, email, "CANCEL", subject.sequence).await?;
        queued = true;
    }
    tx.commit().await?;
    if queued {
        super::jobs::enqueue(pool, "imip_send", serde_json::json!({}), None, 0)
            .await
            .ok();
    }
    Ok(())
}

type AttendeeRef = (Option<String>, Option<Uuid>);

async fn list_subject_attendees(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
) -> Result<Vec<AttendeeRef>, DbError> {
    let sql = format!(
        "SELECT email::text AS email, user_id FROM {} WHERE {} = $1 ORDER BY created_at",
        kind.attendee_table(),
        kind.fk()
    );
    sqlx::query_as::<_, AttendeeRef>(&sql)
        .bind(subject_id)
        .fetch_all(tx)
        .await
        .map_err(Into::into)
}

async fn lookup_user_id(tx: &mut sqlx::PgConnection, email: &str) -> Result<Option<Uuid>, DbError> {
    sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(tx)
        .await
        .map_err(Into::into)
}

/// The attendee's default collection for the kind: the first calendar they
/// own, by order_index then slug, whose component set includes the kind.
async fn default_collection(
    tx: &mut sqlx::PgConnection,
    user_id: Uuid,
    component: &str,
) -> Result<Option<Uuid>, DbError> {
    sqlx::query_scalar(
        "SELECT c.id FROM calendars c
         JOIN calendar_acl a ON a.calendar_id = c.id
           AND a.principal_user_id = $1 AND a.capability = 'owner'
         WHERE c.deleted_at IS NULL AND $2::text = ANY(c.components)
         ORDER BY c.order_index, c.slug LIMIT 1",
    )
    .bind(user_id)
    .bind(component)
    .fetch_optional(tx)
    .await
    .map_err(Into::into)
}

async fn request_needed(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
    email: &str,
    sequence: i32,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT MAX(sequence) FROM schedule_messages
         WHERE {} = $1 AND attendee_email = $2 AND method = 'REQUEST' AND direction = 'outbound'",
        kind.fk()
    );
    let last: Option<i32> = sqlx::query_scalar(&sql)
        .bind(subject_id)
        .bind(email)
        .fetch_one(tx)
        .await?;
    Ok(last.is_none() || last < Some(sequence))
}

async fn past_request_attendees(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
) -> Result<Vec<String>, DbError> {
    let sql = format!(
        "SELECT DISTINCT attendee_email::text FROM schedule_messages
         WHERE {} = $1 AND method = 'REQUEST' AND direction = 'outbound'",
        kind.fk()
    );
    sqlx::query_scalar(&sql)
        .bind(subject_id)
        .fetch_all(tx)
        .await
        .map_err(Into::into)
}

async fn cancel_recorded(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
    email: &str,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM schedule_messages
         WHERE {} = $1 AND attendee_email = $2 AND method = 'CANCEL' AND direction = 'outbound')",
        kind.fk()
    );
    sqlx::query_scalar(&sql)
        .bind(subject_id)
        .bind(email)
        .fetch_one(tx)
        .await
        .map_err(Into::into)
}

async fn record_outbound_tx(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
    attendee_email: &str,
    method: &str,
    sequence: i32,
) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO schedule_messages (id, {}, attendee_email, method, direction, sequence)
         VALUES ($1, $2, $3, $4, 'outbound', $5)
         ON CONFLICT DO NOTHING",
        kind.fk()
    );
    sqlx::query(&sql)
        .bind(Uuid::new_v4())
        .bind(subject_id)
        .bind(attendee_email)
        .bind(method)
        .bind(sequence)
        .execute(tx)
        .await?;
    Ok(())
}

// ============ delivered copies ============

/// Builds or rebuilds the organizer-driven copy of `subject` (master plus its
/// live overrides) in the attendee's default collection, inside the caller's
/// transaction: fields come from the origin on every dispatch (organizer
/// wins), the change_log rows and ctag bump ride along, and a fresh copy
/// carries an in-app notification. Does nothing when the origin is gone.
async fn rebuild_copy(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject: &SubjectRow,
    calendar_id: Uuid,
    attendee_user: Uuid,
) -> Result<(), DbError> {
    let Some(master_copy) = upsert_copy(tx, kind, subject.id, calendar_id, None).await? else {
        return Ok(());
    };
    let sql = format!(
        "SELECT id FROM {} WHERE {} = $1 AND deleted_at IS NULL",
        kind.table(),
        kind.master_col()
    );
    let overrides: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(subject.id)
        .fetch_all(&mut *tx)
        .await?;
    for override_id in overrides {
        upsert_copy(tx, kind, override_id, calendar_id, Some(master_copy)).await?;
    }
    drop_dead_copies(tx, kind, subject.id).await?;
    replace_copy_attendees(tx, kind, subject.id, calendar_id).await?;
    copy_alarms(tx, kind, subject.id, calendar_id).await?;
    notify_user(
        tx,
        attendee_user,
        "New invitation",
        &subject.summary,
        &format!("sched-invite-{master_copy}-{}", subject.sequence),
    )
    .await?;
    Ok(())
}

/// Inserts or updates one copy row from its origin row. Returns the copy id,
/// or None when the origin row is gone.
async fn upsert_copy(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    origin_id: Uuid,
    calendar_id: Uuid,
    copy_master_id: Option<Uuid>,
) -> Result<Option<Uuid>, DbError> {
    let sql = format!(
        "SELECT id FROM {} WHERE origin_id = $1 AND calendar_id = $2 AND deleted_at IS NULL LIMIT 1",
        kind.table()
    );
    let existing: Option<Uuid> = sqlx::query_scalar(&sql)
        .bind(origin_id)
        .bind(calendar_id)
        .fetch_optional(&mut *tx)
        .await?;
    let copy_id = match existing {
        Some(copy_id) => {
            let updated = update_copy_from_origin(&mut *tx, kind, copy_id, origin_id).await?;
            if let Some((sequence, updated_at, cal)) = updated {
                let etag = etag_for(cal, sequence, updated_at);
                let sql = format!("UPDATE {} SET etag = $2 WHERE id = $1", kind.table());
                sqlx::query(&sql)
                    .bind(copy_id)
                    .bind(&etag)
                    .execute(&mut *tx)
                    .await?;
                record_change(&mut *tx, cal, copy_id, kind.resource_type(), "updated").await?;
            }
            copy_id
        }
        None => {
            let Some((sequence, updated_at, cal)) =
                insert_copy(&mut *tx, kind, origin_id, calendar_id, copy_master_id).await?
            else {
                return Ok(None);
            };
            let copy_id: Uuid = sqlx::query_scalar(&format!(
                "SELECT id FROM {} WHERE origin_id = $1 AND calendar_id = $2 AND deleted_at IS NULL",
                kind.table()
            ))
            .bind(origin_id)
            .bind(calendar_id)
            .fetch_one(&mut *tx)
            .await?;
            let etag = etag_for(cal, sequence, updated_at);
            let sql = format!("UPDATE {} SET etag = $2 WHERE id = $1", kind.table());
            sqlx::query(&sql)
                .bind(copy_id)
                .bind(&etag)
                .execute(&mut *tx)
                .await?;
            record_change(&mut *tx, cal, copy_id, kind.resource_type(), "created").await?;
            copy_id
        }
    };
    Ok(Some(copy_id))
}

/// Copies of origin rows that were soft-deleted (removed overrides, or the
/// series already cancelled) stop being served.
async fn drop_dead_copies(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    origin_master_id: Uuid,
) -> Result<(), DbError> {
    let sql = format!(
        "SELECT c.id, c.calendar_id FROM {} c
         JOIN {} o ON o.id = c.origin_id
         WHERE (c.origin_id = $1 OR o.{} = $1) AND c.deleted_at IS NULL
           AND o.deleted_at IS NOT NULL",
        kind.table(),
        kind.table(),
        kind.master_col()
    );
    let stale: Vec<(Uuid, Uuid)> = sqlx::query_as(&sql)
        .bind(origin_master_id)
        .fetch_all(&mut *tx)
        .await?;
    for (copy_id, calendar_id) in stale {
        let sql = format!(
            "UPDATE {} SET deleted_at = now() WHERE id = $1",
            kind.table()
        );
        sqlx::query(&sql).bind(copy_id).execute(&mut *tx).await?;
        record_change(
            &mut *tx,
            calendar_id,
            copy_id,
            kind.resource_type(),
            "deleted",
        )
        .await?;
    }
    Ok(())
}

/// Replaces the copy rows' attendees with the origin's (delete + re-insert;
/// the origin's attendee partstats are authoritative because `reply` keeps
/// them in sync).
async fn replace_copy_attendees(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    origin_master_id: Uuid,
    calendar_id: Uuid,
) -> Result<(), DbError> {
    let sql = format!(
        "DELETE FROM {att} WHERE {fk} IN (
             SELECT c.id FROM {tbl} c WHERE c.calendar_id = $1
               AND (c.origin_id = $2 OR c.{master} = $2)
         )",
        att = kind.attendee_table(),
        fk = kind.fk(),
        tbl = kind.table(),
        master = kind.master_col()
    );
    sqlx::query(&sql)
        .bind(calendar_id)
        .bind(origin_master_id)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "INSERT INTO {att} (id, {fk}, user_id, contact_id, email, display_name, telephone, role, partstat, rsvp)
         SELECT gen_random_uuid(), c.id, a.user_id, a.contact_id, a.email, a.display_name,
                a.telephone, a.role, a.partstat, a.rsvp
         FROM {tbl} c
         JOIN {att} a ON a.{fk} = c.origin_id
         WHERE c.calendar_id = $1 AND (c.origin_id = $2 OR c.{master} = $2)",
        att = kind.attendee_table(),
        fk = kind.fk(),
        tbl = kind.table(),
        master = kind.master_col()
    );
    sqlx::query(&sql)
        .bind(calendar_id)
        .bind(origin_master_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn copy_alarms(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    origin_master_id: Uuid,
    calendar_id: Uuid,
) -> Result<(), DbError> {
    let sql = format!(
        "DELETE FROM {alarms} WHERE {fk} IN (
             SELECT c.id FROM {tbl} c WHERE c.calendar_id = $1
               AND (c.origin_id = $2 OR c.{master} = $2)
         )",
        alarms = kind.alarm_table(),
        fk = kind.fk(),
        tbl = kind.table(),
        master = kind.master_col()
    );
    sqlx::query(&sql)
        .bind(calendar_id)
        .bind(origin_master_id)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "INSERT INTO {alarms} (id, {fk}, action, related, offset_interval, trigger_at, description, summary, recipient_emails, notify_channels)
         SELECT gen_random_uuid(), c.id, a.action, a.related, a.offset_interval, a.trigger_at,
                a.description, a.summary, a.recipient_emails, a.notify_channels
         FROM {tbl} c
         JOIN {alarms} a ON a.{fk} = c.origin_id
         WHERE c.calendar_id = $1 AND (c.origin_id = $2 OR c.{master} = $2)",
        alarms = kind.alarm_table(),
        fk = kind.fk(),
        tbl = kind.table(),
        master = kind.master_col()
    );
    sqlx::query(&sql)
        .bind(calendar_id)
        .bind(origin_master_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Live copies of the series whose owner is no longer an invited attendee:
/// set to STATUS:CANCELLED (kept visible, RFC 5546) and notified. Copies on
/// calendars owned by still-invited users are left alone.
///
/// Copies always carry a non-NULL origin_id; the organizer's own RECURRENCE-ID
/// exceptions do not — restricting the master-pointer branch to copies keeps
/// a master's dispatch from cancelling its own exception rows.
async fn cancel_stray_copies(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject: &SubjectRow,
    current_emails: &HashSet<String>,
    internal_users: &HashSet<Uuid>,
) -> Result<(), DbError> {
    let sql = format!(
        "SELECT c.id, c.calendar_id, a.principal_user_id FROM {} c
         JOIN calendar_acl a ON a.calendar_id = c.calendar_id AND a.capability = 'owner'
         WHERE (c.origin_id = $1 OR (c.origin_id IS NOT NULL AND c.{} = $1))
           AND c.deleted_at IS NULL
         ORDER BY c.id",
        kind.table(),
        kind.master_col()
    );
    let rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(&sql)
        .bind(subject.id)
        .fetch_all(&mut *tx)
        .await?;
    let mut cancelled: HashSet<Uuid> = HashSet::new();
    for (copy_id, calendar_id, owner) in rows {
        if cancelled.contains(&copy_id) || internal_users.contains(&owner) {
            continue;
        }
        let owner_email: Option<String> =
            sqlx::query_scalar("SELECT email::text FROM users WHERE id = $1")
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        if let Some(email) = &owner_email
            && current_emails.contains(&email.to_lowercase())
        {
            continue;
        }
        cancelled.insert(copy_id);
        cancel_copy_row(tx, kind, copy_id, calendar_id, owner, &subject.summary).await?;
    }
    Ok(())
}

/// STATUS:CANCELLED on one copy: sequence bump, etag, change_log, ctag,
/// in-app notification; the row stays visible (RFC 5546).
async fn cancel_copy_row(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    copy_id: Uuid,
    calendar_id: Uuid,
    notify_user_id: Uuid,
    summary: &str,
) -> Result<(), DbError> {
    let sql = format!(
        "UPDATE {} SET status = 'CANCELLED', sequence = sequence + 1, updated_at = now()
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING sequence, updated_at",
        kind.table()
    );
    let Some((sequence, updated_at)): Option<(i32, DateTime<Utc>)> = sqlx::query_as(&sql)
        .bind(copy_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(());
    };
    let etag = etag_for(calendar_id, sequence, updated_at);
    let sql = format!("UPDATE {} SET etag = $2 WHERE id = $1", kind.table());
    sqlx::query(&sql)
        .bind(copy_id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    record_change(
        &mut *tx,
        calendar_id,
        copy_id,
        kind.resource_type(),
        "updated",
    )
    .await?;
    notify_user(
        tx,
        notify_user_id,
        "Meeting cancelled",
        summary,
        &format!("sched-cancel-{copy_id}-{sequence}"),
    )
    .await?;
    Ok(())
}

/// Cancels every live copy of the subject series in one calendar (organizer
/// deleted the object or removed the attendee).
async fn cancel_copies_for(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    subject_id: Uuid,
    calendar_id: Uuid,
    attendee_user: Uuid,
    summary: &str,
) -> Result<(), DbError> {
    let sql = format!(
        "SELECT id FROM {} WHERE calendar_id = $1 AND (origin_id = $2 OR {} = $2)
           AND deleted_at IS NULL",
        kind.table(),
        kind.master_col()
    );
    let copies: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(calendar_id)
        .bind(subject_id)
        .fetch_all(&mut *tx)
        .await?;
    for copy_id in copies {
        cancel_copy_row(tx, kind, copy_id, calendar_id, attendee_user, summary).await?;
    }
    Ok(())
}

// ---- per-kind copy SQL (the two tables differ in columns) ----

/// (sequence, updated_at, calendar_id) of the written copy.
type CopyTouched = (i32, DateTime<Utc>, Uuid);

async fn update_copy_from_origin(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    copy_id: Uuid,
    origin_id: Uuid,
) -> Result<Option<CopyTouched>, DbError> {
    let sql = match kind {
        SubjectKind::Event => "UPDATE events c SET
                uid = o.uid, starts_at = o.starts_at, ends_at = o.ends_at,
                start_date = o.start_date, end_date = o.end_date, duration = o.duration,
                tzid = o.tzid, all_day = o.all_day, rrule = o.rrule, rdate = o.rdate, exdate = o.exdate,
                summary = o.summary, description_html = o.description_html,
                description_text = o.description_text, url = o.url, status = o.status,
                priority = o.priority, class = o.class, transp = o.transp, categories = o.categories,
                location_id = o.location_id, organizer_user_id = o.organizer_user_id,
                organizer_email = o.organizer_email, organizer_name = o.organizer_name,
                sequence = o.sequence, updated_at = now()
             FROM events o
             WHERE c.id = $1 AND o.id = $2 AND o.deleted_at IS NULL
             RETURNING c.sequence, c.updated_at, c.calendar_id",
        SubjectKind::Task => "UPDATE tasks c SET
                uid = o.uid, starts_at = o.starts_at, start_date = o.start_date,
                due_at = o.due_at, due_date = o.due_date, duration = o.duration,
                tzid = o.tzid, floating = o.floating, completed_at = o.completed_at,
                rrule = o.rrule, rdate = o.rdate, exdate = o.exdate,
                summary = o.summary, description_html = o.description_html,
                description_text = o.description_text, url = o.url, location = o.location,
                status = o.status, percent_complete = o.percent_complete, priority = o.priority,
                class = o.class, categories = o.categories, parent_uid = o.parent_uid,
                sort_order = o.sort_order, extra_props = o.extra_props,
                organizer_user_id = o.organizer_user_id, organizer_email = o.organizer_email,
                organizer_name = o.organizer_name, sequence = o.sequence, updated_at = now()
             FROM tasks o
             WHERE c.id = $1 AND o.id = $2 AND o.deleted_at IS NULL
             RETURNING c.sequence, c.updated_at, c.calendar_id",
    };
    sqlx::query_as(sql)
        .bind(copy_id)
        .bind(origin_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Into::into)
}

async fn insert_copy(
    tx: &mut sqlx::PgConnection,
    kind: SubjectKind,
    origin_id: Uuid,
    calendar_id: Uuid,
    copy_master_id: Option<Uuid>,
) -> Result<Option<CopyTouched>, DbError> {
    let sql = match kind {
        SubjectKind::Event => "INSERT INTO events (
                id, calendar_id, uid, master_event_id, recurrence_id, recurrence_id_date,
                starts_at, ends_at, start_date, end_date, duration, tzid, all_day,
                rrule, rdate, exdate, summary, description_html, description_text, url,
                status, priority, class, transp, categories, location_id,
                organizer_user_id, organizer_email, organizer_name, origin_id, sequence, created_by
             )
             SELECT gen_random_uuid(), $1, o.uid, $2, o.recurrence_id, o.recurrence_id_date,
                o.starts_at, o.ends_at, o.start_date, o.end_date, o.duration, o.tzid, o.all_day,
                o.rrule, o.rdate, o.exdate, o.summary, o.description_html, o.description_text, o.url,
                o.status, o.priority, o.class, o.transp, o.categories, o.location_id,
                o.organizer_user_id, o.organizer_email, o.organizer_name, o.id, o.sequence, o.created_by
             FROM events o WHERE o.id = $3 AND o.deleted_at IS NULL
             RETURNING sequence, updated_at, calendar_id",
        SubjectKind::Task => "INSERT INTO tasks (
                id, calendar_id, uid, master_task_id, recurrence_id, recurrence_id_date,
                starts_at, start_date, due_at, due_date, duration, tzid, floating, completed_at,
                rrule, rdate, exdate, summary, description_html, description_text, url, location,
                status, percent_complete, priority, class, categories, parent_uid, sort_order,
                extra_props, organizer_user_id, organizer_email, organizer_name, origin_id, sequence, created_by
             )
             SELECT gen_random_uuid(), $1, o.uid, $2, o.recurrence_id, o.recurrence_id_date,
                o.starts_at, o.start_date, o.due_at, o.due_date, o.duration, o.tzid, o.floating, o.completed_at,
                o.rrule, o.rdate, o.exdate, o.summary, o.description_html, o.description_text, o.url, o.location,
                o.status, o.percent_complete, o.priority, o.class, o.categories, o.parent_uid, o.sort_order,
                o.extra_props, o.organizer_user_id, o.organizer_email, o.organizer_name, o.id, o.sequence, o.created_by
             FROM tasks o WHERE o.id = $3 AND o.deleted_at IS NULL
             RETURNING sequence, updated_at, calendar_id",
    };
    sqlx::query_as(sql)
        .bind(calendar_id)
        .bind(copy_master_id)
        .bind(origin_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Into::into)
}

// ============ reply (design section 6) ============

const VALID_PARTSTATS: &[&str] = &[
    "NEEDS-ACTION",
    "ACCEPTED",
    "DECLINED",
    "TENTATIVE",
    "DELEGATED",
    "COMPLETED",
    "IN-PROCESS",
];

/// The single RSVP primitive: sets the attendee's PARTSTAT on the origin row
/// and touches the parent row (updated_at, etag, change_log, ctag — sequence
/// unchanged) so the organizer's clients see the reply on their next sync.
/// Internal callers resolve the attendee email from the copy or the API;
/// external ones come from the inbound iMIP webhook.
pub async fn reply(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    attendee_email: &str,
    partstat: &str,
) -> Result<(), DbError> {
    let partstat = partstat.to_ascii_uppercase();
    if !VALID_PARTSTATS.contains(&partstat.as_str()) {
        return Err(DbError::Conflict(format!("invalid PARTSTAT {partstat}")));
    }
    let mut tx = pool.begin().await?;
    // R7: replies apply to origin rows only.
    let sql = format!(
        "SELECT id, calendar_id, {} FROM {}
         WHERE id = $1 AND origin_id IS NULL AND deleted_at IS NULL FOR UPDATE",
        kind.master_col(),
        kind.table()
    );
    let (row_id, calendar_id, master_id): (Uuid, Uuid, Option<Uuid>) = sqlx::query_as(&sql)
        .bind(subject_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DbError::NotFound)?;
    let touched = master_id.unwrap_or(row_id);
    let sql = format!(
        "UPDATE {} SET updated_at = now() WHERE id = $1 RETURNING sequence, updated_at",
        kind.table()
    );
    let (sequence, updated_at): (i32, DateTime<Utc>) = sqlx::query_as(&sql)
        .bind(touched)
        .fetch_one(&mut *tx)
        .await?;
    let etag = etag_for(calendar_id, sequence, updated_at);
    let sql = format!("UPDATE {} SET etag = $2 WHERE id = $1", kind.table());
    sqlx::query(&sql)
        .bind(touched)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "UPDATE {} SET partstat = $3, updated_at = now()
         WHERE {} = $1 AND email = $2",
        kind.attendee_table(),
        kind.fk()
    );
    sqlx::query(&sql)
        .bind(subject_id)
        .bind(attendee_email)
        .bind(&partstat)
        .execute(&mut *tx)
        .await?;
    record_change(
        &mut tx,
        calendar_id,
        touched,
        kind.resource_type(),
        "updated",
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Resolves which of a user's attendee identities sits on a subject row (the
/// origin for RSVPs; also works on a copy for attendee-side edits). Returns
/// the attendee row's own email.
pub async fn attendee_email_for_user(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
    user_id: Uuid,
) -> Result<Option<String>, DbError> {
    let sql = format!(
        "SELECT a.email::text FROM {} a
         JOIN users u ON u.id = $2
         WHERE a.{} = $1 AND (a.user_id = $2 OR a.email = u.email)
           AND a.email IS NOT NULL LIMIT 1",
        kind.attendee_table(),
        kind.fk()
    );
    sqlx::query_scalar(&sql)
        .bind(subject_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Whether `subject_id` is a delivered copy; Some(origin id) when it is.
pub async fn origin_of(
    pool: &PgPool,
    kind: SubjectKind,
    subject_id: Uuid,
) -> Result<Option<Uuid>, DbError> {
    let sql = format!(
        "SELECT origin_id FROM {} WHERE id = $1 AND deleted_at IS NULL",
        kind.table()
    );
    sqlx::query_scalar(&sql)
        .bind(subject_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
        .map(|origin: Option<Option<Uuid>>| origin.flatten())
}

/// Attendee-side decline: a DELETE of a delivered copy sets the attendee's
/// PARTSTAT to DECLINED on the origin, then soft-deletes the copy. No-op for
/// rows that are not copies. The actor must own the copy's calendar.
pub async fn decline_copy(
    pool: &PgPool,
    kind: SubjectKind,
    copy_id: Uuid,
    actor: Uuid,
) -> Result<(), DbError> {
    let Some(origin_id) = origin_of(pool, kind, copy_id).await? else {
        return Ok(());
    };
    // Authorization: the actor must own the calendar the copy lives in.
    let sql = format!(
        "SELECT principal_user_id FROM calendar_acl
         WHERE calendar_id = (SELECT calendar_id FROM {} WHERE id = $1)
           AND principal_user_id = $2 AND capability = 'owner' LIMIT 1",
        kind.table()
    );
    let owner: Option<Uuid> = sqlx::query_scalar(&sql)
        .bind(copy_id)
        .bind(actor)
        .fetch_optional(pool)
        .await?;
    if owner.is_none() {
        return Err(DbError::NotFound);
    }
    if let Some(email) = attendee_email_for_user(pool, kind, origin_id, actor).await? {
        // The origin may already be gone (organizer deleted it first); the
        // copy still goes away.
        reply(pool, kind, origin_id, &email, "DECLINED").await.ok();
    }
    let mut tx = pool.begin().await?;
    let sql = format!(
        "UPDATE {} SET deleted_at = now() WHERE id = $1",
        kind.table()
    );
    sqlx::query(&sql).bind(copy_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn notify_user(
    tx: &mut sqlx::PgConnection,
    user_id: Uuid,
    title: &str,
    body: &str,
    dedupe_key: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO notifications (id, user_id, channel, title, body, dedupe_key)
         VALUES (gen_random_uuid(), $1, 'in_app', $2, $3, $4)
         ON CONFLICT (dedupe_key) DO NOTHING",
    )
    .bind(user_id)
    .bind(title)
    .bind(body)
    .bind(dedupe_key)
    .execute(tx)
    .await?;
    Ok(())
}

// ============ tests ============

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .unwrap();
        crate::migrate(&pool).await.unwrap();
        Some(pool)
    }

    /// Two users (organizer + attendee), each with a personal tenant and a
    /// default calendar allowing both kinds.
    struct TwoUsers {
        organizer: Uuid,
        attendee: Uuid,
        organizer_cal: Uuid,
        attendee_cal: Uuid,
        attendee_email: String,
    }

    async fn two_users(pool: &sqlx::PgPool) -> TwoUsers {
        let mk = |n: String| async move {
            let user = Uuid::new_v4();
            sqlx::query("INSERT INTO users (id, username, email) VALUES ($1, $2, $3)")
                .bind(user)
                .bind(format!("u-{}", user.simple()))
                .bind(format!("{}@sched.test", user.simple()))
                .execute(pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $2, true)",
            )
            .bind(user)
            .bind(user.simple().to_string())
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'owner')",
            )
            .bind(user)
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
            let cal = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO calendars (id, tenant_id, slug, name, created_by, components)
                 VALUES ($1, $2, $3, $3, $4, $5)",
            )
            .bind(cal)
            .bind(user)
            .bind(format!("{n}-{}", user.simple()))
            .bind(user)
            .bind(["VEVENT", "VTODO"].map(str::to_string).to_vec())
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO calendar_acl (calendar_id, principal_user_id, capability, can_manage_acl)
                 VALUES ($1, $2, 'owner', true)",
            )
            .bind(cal)
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
            (user, cal)
        };
        let (organizer, organizer_cal) = mk("org".into()).await;
        let (attendee, attendee_cal) = mk("att".into()).await;
        let attendee_email =
            sqlx::query_scalar::<_, String>("SELECT email::text FROM users WHERE id = $1")
                .bind(attendee)
                .fetch_one(pool)
                .await
                .unwrap();
        TwoUsers {
            organizer,
            attendee,
            organizer_cal,
            attendee_cal,
            attendee_email,
        }
    }

    /// Organizer's event with one attendee (internal when user_id is set).
    async fn create_event_with_attendee(
        pool: &sqlx::PgPool,
        f: &TwoUsers,
        attendee_user: Option<Uuid>,
        attendee_email: Option<&str>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, starts_at, ends_at,
                organizer_user_id, organizer_email)
             VALUES ($1, $2, $3, 'standup', now(), now(), $4,
                (SELECT email FROM users WHERE id = $4))",
        )
        .bind(id)
        .bind(f.organizer_cal)
        .bind(Uuid::new_v4().to_string())
        .bind(f.organizer)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO event_attendees (id, event_id, user_id, email)
             VALUES (gen_random_uuid(), $1, $2, $3)",
        )
        .bind(id)
        .bind(attendee_user)
        .bind(attendee_email)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn outbound_count(pool: &sqlx::PgPool, subject_id: Uuid, method: &str) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM schedule_messages
             WHERE event_id = $1 AND method = '{method}' AND direction = 'outbound'"
        ))
        .bind(subject_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn copy_of(pool: &sqlx::PgPool, calendar_id: Uuid, kind: SubjectKind) -> Uuid {
        sqlx::query_scalar(&format!(
            "SELECT id FROM {} WHERE calendar_id = $1 AND origin_id IS NOT NULL",
            kind.table()
        ))
        .bind(calendar_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn internal_attendee_gets_copy_no_email() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event =
            create_event_with_attendee(&pool, &f, Some(f.attendee), Some(&f.attendee_email)).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        // The copy lands in the attendee's default calendar, pointing at the
        // origin with the organizer's fields preserved.
        let (origin_id, organizer_email): (Uuid, String) =
            sqlx::query_as("SELECT origin_id, organizer_email::text FROM events WHERE id = $1")
                .bind(copy_of(&pool, f.attendee_cal, SubjectKind::Event).await)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(origin_id, event);
        assert_eq!(
            organizer_email,
            format!("{}@sched.test", f.organizer.simple())
        );
        // No outbound email for the internal attendee.
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 0);
        // Change log row + ctag bump + in-app notification on the attendee side.
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM change_log WHERE calendar_id = $1 AND resource_type = 'event'",
        )
        .bind(f.attendee_cal)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(logged > 0);
        let notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND channel = 'in_app'",
        )
        .bind(f.attendee)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(notified > 0);
        // Never dispatch from a copy: a second dispatch from the copy is a
        // no-op guard (the copy id resolves to no subject at all).
        let copy_id = copy_of(&pool, f.attendee_cal, SubjectKind::Event).await;
        assert!(matches!(
            dispatch(&pool, SubjectKind::Event, copy_id, f.organizer).await,
            Err(DbError::NotFound)
        ));
        assert_eq!(outbound_count(&pool, copy_id, "REQUEST").await, 0);
    }

    #[tokio::test]
    async fn external_attendee_dedupes_by_sequence() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event = create_event_with_attendee(&pool, &f, None, Some("ext@sched.test")).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 1);
        // Repeated dispatch at the same sequence: no re-queue.
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 1);
        // Sequence bump: one updated REQUEST.
        sqlx::query("UPDATE events SET sequence = sequence + 1, updated_at = now() WHERE id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 2);
        // A non-organizer cannot dispatch.
        dispatch(&pool, SubjectKind::Event, event, f.attendee)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 2);
    }

    #[tokio::test]
    async fn organizer_edit_rebuilds_copy_and_reply_survives() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event =
            create_event_with_attendee(&pool, &f, Some(f.attendee), Some(&f.attendee_email)).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        let copy_id = copy_of(&pool, f.attendee_cal, SubjectKind::Event).await;
        // Attendee partstat-only edit on the copy: accepted through reply().
        let email = attendee_email_for_user(&pool, SubjectKind::Event, copy_id, f.attendee)
            .await
            .unwrap()
            .unwrap();
        reply(&pool, SubjectKind::Event, event, &email, "ACCEPTED")
            .await
            .unwrap();
        // Organizer update: the copy's fields follow the organizer...
        sqlx::query("UPDATE events SET summary = 'renamed' WHERE id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE events SET sequence = sequence + 1, updated_at = now() WHERE id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        let (summary, copy_partstat): (String, String) = sqlx::query_as(
            "SELECT e.summary, a.partstat FROM events e
             JOIN event_attendees a ON a.event_id = e.id WHERE e.id = $1",
        )
        .bind(copy_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(summary, "renamed");
        // ...and the attendee's own reply survives, because the origin row
        // (kept current by reply) is the rebuild source.
        assert_eq!(copy_partstat, "ACCEPTED");
        let updated_log: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM change_log WHERE calendar_id = $1 AND resource_id = $2
               AND operation = 'updated'",
        )
        .bind(f.attendee_cal)
        .bind(copy_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(updated_log > 0);
        assert_eq!(outbound_count(&pool, event, "REQUEST").await, 0);
    }

    #[tokio::test]
    async fn attendee_copy_delete_declines_origin() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event =
            create_event_with_attendee(&pool, &f, Some(f.attendee), Some(&f.attendee_email)).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        let copy_id = copy_of(&pool, f.attendee_cal, SubjectKind::Event).await;
        decline_copy(&pool, SubjectKind::Event, copy_id, f.attendee)
            .await
            .unwrap();
        let partstat: String =
            sqlx::query_scalar("SELECT partstat FROM event_attendees WHERE event_id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(partstat, "DECLINED");
        let gone: bool =
            sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM events WHERE id = $1")
                .bind(copy_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(gone);
        // Deleting an origin row is not a decline (no-op guard).
        decline_copy(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, event, "CANCEL").await, 0);
    }

    #[tokio::test]
    async fn organizer_delete_cancels_copies_and_notifies() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event =
            create_event_with_attendee(&pool, &f, Some(f.attendee), Some(&f.attendee_email)).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        // Organizer soft-deletes, then cancels.
        sqlx::query("UPDATE events SET deleted_at = now() WHERE id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        dispatch_cancel(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        let (status, seq, still_visible): (Option<String>, i32, bool) =
            sqlx::query_as("SELECT status, sequence, deleted_at IS NULL FROM events WHERE id = $1")
                .bind(copy_of(&pool, f.attendee_cal, SubjectKind::Event).await)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status.as_deref(), Some("CANCELLED"));
        assert_eq!(seq, 1);
        assert!(still_visible);
        let notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND title = 'Meeting cancelled'",
        )
        .bind(f.attendee)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(notified > 0);
        // Internal attendees are never emailed.
        assert_eq!(outbound_count(&pool, event, "CANCEL").await, 0);
    }

    #[tokio::test]
    async fn reply_touches_origin_etag_and_change_log() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event = create_event_with_attendee(&pool, &f, None, Some("ext@sched.test")).await;
        let before: String = sqlx::query_scalar("SELECT etag FROM events WHERE id = $1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap();
        reply(
            &pool,
            SubjectKind::Event,
            event,
            "ext@sched.test",
            "TENTATIVE",
        )
        .await
        .unwrap();
        let (after, seq): (String, i32) =
            sqlx::query_as("SELECT etag, sequence FROM events WHERE id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(after, before);
        assert_eq!(seq, 0); // sequence unchanged, etag moved
        let partstat: String =
            sqlx::query_scalar("SELECT partstat FROM event_attendees WHERE event_id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(partstat, "TENTATIVE");
        let logged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM change_log WHERE resource_id = $1 AND resource_type = 'event'",
        )
        .bind(event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(logged > 0);
        // Partstat vocabulary is enforced.
        assert!(matches!(
            reply(&pool, SubjectKind::Event, event, "ext@sched.test", "MAYBE").await,
            Err(DbError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn task_dispatch_and_reply() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        // Organizer task assigned to the internal attendee.
        let task_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO tasks (id, calendar_id, uid, summary, due_at,
                organizer_user_id, organizer_email)
             VALUES ($1, $2, $3, 'ship it', now(), $4, (SELECT email FROM users WHERE id = $4))",
        )
        .bind(task_id)
        .bind(f.organizer_cal)
        .bind(Uuid::new_v4().to_string())
        .bind(f.organizer)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_attendees (id, task_id, user_id, email)
             VALUES (gen_random_uuid(), $1, $2, $3)",
        )
        .bind(task_id)
        .bind(f.attendee)
        .bind(&f.attendee_email)
        .execute(&pool)
        .await
        .unwrap();
        dispatch(&pool, SubjectKind::Task, task_id, f.organizer)
            .await
            .unwrap();
        assert_eq!(outbound_count(&pool, task_id, "REQUEST").await, 0);
        let copy_id = copy_of(&pool, f.attendee_cal, SubjectKind::Task).await;
        // Reply against the task origin (the inbound-webhook shape).
        reply(
            &pool,
            SubjectKind::Task,
            task_id,
            &f.attendee_email,
            "ACCEPTED",
        )
        .await
        .unwrap();
        let partstat: String =
            sqlx::query_scalar("SELECT partstat FROM task_attendees WHERE task_id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(partstat, "ACCEPTED");
        // Decline by deleting the copy.
        decline_copy(&pool, SubjectKind::Task, copy_id, f.attendee)
            .await
            .unwrap();
        let partstat: String =
            sqlx::query_scalar("SELECT partstat FROM task_attendees WHERE task_id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(partstat, "DECLINED");
    }

    #[tokio::test]
    async fn removed_internal_attendees_copy_is_cancelled() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = two_users(&pool).await;
        let event =
            create_event_with_attendee(&pool, &f, Some(f.attendee), Some(&f.attendee_email)).await;
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        // Organizer removes the attendee and updates.
        sqlx::query("DELETE FROM event_attendees WHERE event_id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE events SET sequence = sequence + 1, updated_at = now() WHERE id = $1")
            .bind(event)
            .execute(&pool)
            .await
            .unwrap();
        dispatch(&pool, SubjectKind::Event, event, f.organizer)
            .await
            .unwrap();
        let (status, deleted): (Option<String>, bool) =
            sqlx::query_as("SELECT status, deleted_at IS NOT NULL FROM events WHERE id = $1")
                .bind(copy_of(&pool, f.attendee_cal, SubjectKind::Event).await)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status.as_deref(), Some("CANCELLED"));
        assert!(!deleted); // kept visible (RFC 5546)
        let notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND title = 'Meeting cancelled'",
        )
        .bind(f.attendee)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(notified > 0);
    }
}
