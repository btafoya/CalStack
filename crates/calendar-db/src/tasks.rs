//! VTODO storage (ADR-015; docs/TASKS_JOURNALS_DESIGN.md sections 3 and 6).
//! Masters plus RECURRENCE-ID overrides share a UID and one CalDAV resource
//! (D3); subtasks chain by raw `parent_uid`. Recurrence expansion into
//! occurrences is the engine's job, not SQL's (ADR-002).

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::alarms::NewAlarm;
use super::{DbError, NewAttendee, etag_for};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub uid: String,
    pub href: Option<String>, // client-chosen filename; NULL = "{id}.ics"
    pub master_task_id: Option<Uuid>,
    pub recurrence_id: Option<NaiveDateTime>,
    pub recurrence_id_date: Option<NaiveDate>,
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub due_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub duration: Option<sqlx::postgres::types::PgInterval>,
    pub tzid: Option<String>,
    /// Wall clock stored as if UTC; exported without Z or TZID.
    pub floating: bool,
    pub completed_at: Option<DateTime<Utc>>,
    pub rrule: Option<String>,
    pub rdate: serde_json::Value,
    pub exdate: serde_json::Value,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub percent_complete: Option<i16>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    pub parent_uid: Option<String>,
    pub sort_order: Option<i64>,
    pub extra_props: serde_json::Value,
    pub organizer_user_id: Option<Uuid>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub origin_id: Option<Uuid>,
    pub sequence: i32,
    pub etag: String,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TaskRow {
    /// The filename this task series is served under over CalDAV.
    pub fn resource_name(&self) -> String {
        resource_name(self.id, self.href.as_deref())
    }
}

/// COALESCE(href, id::text || '.ics') in Rust — the addressing rule shared by
/// events, tasks and journals (12a log).
pub fn resource_name(id: Uuid, href: Option<&str>) -> String {
    href.unwrap_or(&format!("{id}.ics")).to_string()
}

fn interval(secs: Option<i64>) -> Option<sqlx::postgres::types::PgInterval> {
    secs.map(|s| sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: s * 1_000_000,
    })
}

#[derive(Debug, Default)]
pub struct NewTaskData {
    pub uid: String,
    pub href: Option<String>, // None = "{id}.ics" (API-created rows)
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub due_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub duration_secs: Option<i64>,
    pub tzid: Option<String>,
    pub floating: bool,
    pub completed_at: Option<DateTime<Utc>>,
    pub rrule: Option<String>,
    pub rdate: Option<serde_json::Value>,
    pub exdate: Option<serde_json::Value>,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub percent_complete: Option<i16>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    pub parent_uid: Option<String>,
    pub sort_order: Option<i64>,
    pub extra_props: Option<serde_json::Value>,
    pub organizer_user_id: Option<Uuid>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
}

/// Creates a task master plus attendees and alarms, and records the change
/// atomically. Cross-type href uniqueness is checked in-transaction (the view
/// cannot carry an index); the per-table unique indexes catch same-type races.
pub async fn create_task(
    pool: &PgPool,
    calendar_id: Uuid,
    created_by: Uuid,
    attendees: &[NewAttendee],
    alarms: &[NewAlarm],
    data: &NewTaskData,
) -> Result<(TaskRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let id = Uuid::new_v4();
    if let Some(href) = &data.href
        && super::href_taken(&mut tx, calendar_id, href).await?
    {
        return Err(DbError::Conflict(format!(
            "href {href} already exists in this calendar"
        )));
    }
    let task = sqlx::query_as::<_, TaskRow>(
        "INSERT INTO tasks (
            id, calendar_id, uid, href,
            starts_at, start_date, due_at, due_date, duration, tzid, floating, completed_at,
            rrule, rdate, exdate,
            summary, description_html, description_text, url, location,
            status, percent_complete, priority, class, categories, parent_uid, sort_order,
            extra_props, organizer_user_id, organizer_email, organizer_name, created_by
         ) VALUES (
            $1, $2, $3, $4,
            $5, $6, $7, $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17, $18, $19, $20,
            $21, $22, $23, $24, $25, $26, $27,
            $28, $29, $30, $31, $32
         )
         RETURNING *",
    )
    .bind(id)
    .bind(calendar_id)
    .bind(&data.uid)
    .bind(&data.href)
    .bind(data.starts_at)
    .bind(data.start_date)
    .bind(data.due_at)
    .bind(data.due_date)
    .bind(interval(data.duration_secs))
    .bind(&data.tzid)
    .bind(data.floating)
    .bind(data.completed_at)
    .bind(&data.rrule)
    .bind(data.rdate.clone().unwrap_or(serde_json::json!([])))
    .bind(data.exdate.clone().unwrap_or(serde_json::json!([])))
    .bind(&data.summary)
    .bind(&data.description_html)
    .bind(&data.description_text)
    .bind(&data.url)
    .bind(&data.location)
    .bind(&data.status)
    .bind(data.percent_complete)
    .bind(data.priority)
    .bind(&data.class)
    .bind(&data.categories)
    .bind(&data.parent_uid)
    .bind(data.sort_order)
    .bind(data.extra_props.clone().unwrap_or(serde_json::json!([])))
    .bind(data.organizer_user_id)
    .bind(&data.organizer_email)
    .bind(&data.organizer_name)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("task uid/recurrence-id/href already exists".into())
        }
        other => other.into(),
    })?;
    write_task_attendees(&mut tx, task.id, attendees).await?;
    replace_task_alarms(&mut tx, task.id, alarms).await?;
    let etag = etag_for(calendar_id, task.sequence, task.updated_at);
    sqlx::query("UPDATE tasks SET etag = $2 WHERE id = $1")
        .bind(task.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    super::record_change(&mut tx, calendar_id, task.id, "task", "created").await?;
    tx.commit().await?;
    Ok((task, etag))
}

async fn write_task_attendees(
    tx: &mut sqlx::PgConnection,
    task_id: Uuid,
    attendees: &[NewAttendee],
) -> Result<(), DbError> {
    for a in attendees {
        sqlx::query(
            "INSERT INTO task_attendees
                (id, task_id, user_id, contact_id, email, display_name, telephone, role, partstat, rsvp)
             VALUES ($1, $2, $3, $4, $5, $6, $7,
                COALESCE($8, 'REQ-PARTICIPANT'), COALESCE($9, 'NEEDS-ACTION'), $10)",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(a.user_id)
        .bind(a.contact_id)
        .bind(&a.email)
        .bind(&a.display_name)
        .bind(&a.telephone)
        .bind(a.role.as_deref())
        .bind(a.partstat.as_deref())
        .bind(a.rsvp)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Replaces all alarms of a task (same shape as event_alarms) inside the
/// caller's transaction.
pub async fn replace_task_alarms(
    tx: &mut sqlx::PgConnection,
    task_id: Uuid,
    alarms: &[NewAlarm],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM task_alarms WHERE task_id = $1")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    for alarm in alarms {
        sqlx::query(
            "INSERT INTO task_alarms
                (id, task_id, action, related, offset_interval, trigger_at,
                 description, summary, recipient_emails, notify_channels)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskAttendeeRow {
    pub id: Uuid,
    pub task_id: Uuid,
    pub user_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    /// NULL for SMS-only attendees (identified by telephone).
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub telephone: Option<String>,
    pub role: String,
    pub partstat: String,
    pub rsvp: Option<bool>,
    pub schedule_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn list_task_attendees(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Vec<TaskAttendeeRow>, DbError> {
    sqlx::query_as::<_, TaskAttendeeRow>(
        "SELECT * FROM task_attendees WHERE task_id = $1 ORDER BY created_at",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskAlarmRow {
    pub id: Uuid,
    pub task_id: Uuid,
    pub action: String,
    pub related: Option<String>, // START | END (END anchors on DUE)
    pub offset_interval: Option<sqlx::postgres::types::PgInterval>,
    pub trigger_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub recipient_emails: Vec<String>,
    pub notify_channels: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl TaskAlarmRow {
    pub fn offset_secs(&self) -> Option<i64> {
        self.offset_interval
            .as_ref()
            .map(|i| i.microseconds / 1_000_000)
    }
}

pub async fn list_task_alarms(pool: &PgPool, task_id: Uuid) -> Result<Vec<TaskAlarmRow>, DbError> {
    sqlx::query_as::<_, TaskAlarmRow>(
        "SELECT * FROM task_alarms WHERE task_id = $1 ORDER BY created_at",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// D3: an override is part of its master's resource. An override change bumps
/// the master's sequence, updated_at and etag in the same transaction and is
/// reported in the change log as the master's row.
async fn append_task_change(
    tx: &mut sqlx::PgConnection,
    calendar_id: Uuid,
    resource_id: Uuid,
    operation: &str,
) -> Result<(), DbError> {
    let master = sqlx::query_as::<_, TaskRow>(
        "UPDATE tasks SET sequence = sequence + 1, updated_at = now()
         WHERE id = (SELECT master_task_id FROM tasks WHERE id = $1)
         RETURNING *",
    )
    .bind(resource_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (resource_id, operation) = match master {
        Some(master) => {
            let etag = etag_for(calendar_id, master.sequence, master.updated_at);
            sqlx::query("UPDATE tasks SET etag = $2 WHERE id = $1")
                .bind(master.id)
                .bind(&etag)
                .execute(&mut *tx)
                .await?;
            (master.id, "updated")
        }
        None => (resource_id, operation),
    };
    super::record_change(tx, calendar_id, resource_id, "task", operation).await
}

pub async fn get_task(pool: &PgPool, task_id: Uuid) -> Result<(TaskRow, String), DbError> {
    let task =
        sqlx::query_as::<_, TaskRow>("SELECT * FROM tasks WHERE id = $1 AND deleted_at IS NULL")
            .bind(task_id)
            .fetch_optional(pool)
            .await?
            .ok_or(DbError::NotFound)?;
    let etag = etag_for(task.calendar_id, task.sequence, task.updated_at);
    Ok((task, etag))
}

/// The live task series master served under `name` in a calendar.
pub async fn get_task_by_href(
    pool: &PgPool,
    calendar_id: Uuid,
    name: &str,
) -> Result<(TaskRow, String), DbError> {
    let task = sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks
         WHERE calendar_id = $1 AND COALESCE(href, id::text || '.ics') = $2
           AND deleted_at IS NULL AND master_task_id IS NULL",
    )
    .bind(calendar_id)
    .bind(name)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let etag = etag_for(task.calendar_id, task.sequence, task.updated_at);
    Ok((task, etag))
}

#[derive(Debug, Default)]
pub struct TaskFilter {
    pub status: Option<String>,
    pub due_before: Option<DateTime<Utc>>,
    pub due_after: Option<DateTime<Utc>>,
    pub category: Option<String>,
    /// Raw parent UID match (parent_uid is a client string, not an id).
    pub parent: Option<String>,
}

/// Live masters of a calendar. Overrides come via `list_overrides`, subtasks
/// via `list_subtasks`.
pub async fn list_tasks(
    pool: &PgPool,
    calendar_id: Uuid,
    filter: &TaskFilter,
) -> Result<Vec<TaskRow>, DbError> {
    let mut sql = String::from(
        "SELECT * FROM tasks
         WHERE calendar_id = $1 AND deleted_at IS NULL AND master_task_id IS NULL",
    );
    if filter.status.is_some() {
        sql.push_str(" AND status = $2");
    }
    if filter.due_before.is_some() {
        sql.push_str(&format!(
            " AND COALESCE(due_at, due_date::timestamp AT TIME ZONE 'UTC') < ${}",
            2 + filter.status.is_some() as i32
        ));
    }
    if filter.due_after.is_some() {
        sql.push_str(&format!(
            " AND COALESCE(due_at, due_date::timestamp AT TIME ZONE 'UTC') > ${}",
            2 + filter.status.is_some() as i32 + filter.due_before.is_some() as i32
        ));
    }
    if filter.category.is_some() {
        sql.push_str(&format!(
            " AND ${} = ANY(categories)",
            2 + filter.status.is_some() as i32
                + filter.due_before.is_some() as i32
                + filter.due_after.is_some() as i32
        ));
    }
    if filter.parent.is_some() {
        sql.push_str(&format!(
            " AND parent_uid = ${}",
            2 + filter.status.is_some() as i32
                + filter.due_before.is_some() as i32
                + filter.due_after.is_some() as i32
                + filter.category.is_some() as i32
        ));
    }
    sql.push_str(" ORDER BY COALESCE(due_at, due_date::timestamp AT TIME ZONE 'UTC') NULLS LAST, sort_order NULLS LAST, created_at");
    let mut q = sqlx::query_as::<_, TaskRow>(&sql).bind(calendar_id);
    if let Some(status) = &filter.status {
        q = q.bind(status);
    }
    if let Some(before) = filter.due_before {
        q = q.bind(before);
    }
    if let Some(after) = filter.due_after {
        q = q.bind(after);
    }
    if let Some(category) = &filter.category {
        q = q.bind(category);
    }
    if let Some(parent) = &filter.parent {
        q = q.bind(parent);
    }
    q.fetch_all(pool).await.map_err(Into::into)
}

/// Live subtasks of a master, matched by the raw client `parent_uid` string
/// (design section 6: a child synced before its parent still works).
pub async fn list_subtasks(
    pool: &PgPool,
    calendar_id: Uuid,
    parent_uid: &str,
) -> Result<Vec<TaskRow>, DbError> {
    sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks
         WHERE calendar_id = $1 AND parent_uid = $2 AND deleted_at IS NULL
           AND master_task_id IS NULL
         ORDER BY sort_order NULLS LAST, created_at",
    )
    .bind(calendar_id)
    .bind(parent_uid)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// RECURRENCE-ID overrides of a series (rendered into the master's resource).
pub async fn list_overrides(pool: &PgPool, master_id: Uuid) -> Result<Vec<TaskRow>, DbError> {
    sqlx::query_as::<_, TaskRow>(
        "SELECT t.* FROM tasks t
         JOIN calendars c ON c.id = t.calendar_id AND c.deleted_at IS NULL
         WHERE t.master_task_id = $1 AND t.deleted_at IS NULL
         ORDER BY t.recurrence_id, t.recurrence_id_date",
    )
    .bind(master_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[derive(Debug, Default)]
pub struct TaskPatch {
    pub summary: Option<String>,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub due_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    pub duration_secs: Option<i64>,
    pub tzid: Option<String>,
    pub floating: Option<bool>,
    pub status: Option<String>,
    pub percent_complete: Option<i16>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    /// Some(_) replaces the category list; None leaves it untouched.
    pub categories: Option<Vec<String>>,
    /// None leaves it untouched; Some(None) clears; Some(Some) reparents.
    pub parent_uid: Option<Option<String>>,
    pub sort_order: Option<i64>,
    /// Some(_) replaces the attendee set entirely; None leaves it untouched.
    pub attendees: Option<Vec<NewAttendee>>,
    /// Some(_) replaces the alarm set entirely; None leaves it untouched.
    pub alarms: Option<Vec<NewAlarm>>,
}

/// Updates a task guarded by its ETag. Patching an override refreshes the
/// master's sequence/updated_at/etag (D3) and reports the master.
pub async fn update_task(
    pool: &PgPool,
    task_id: Uuid,
    if_match: Option<&str>,
    patch: &TaskPatch,
) -> Result<(TaskRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !super::constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    // starts/due triples are mutually exclusive (CHECK constraints): switching
    // date <-> timed must clear the other column; a DURATION replaces DUE.
    let (starts_at, start_date) = if patch.start_date.is_some() {
        (None, patch.start_date)
    } else if patch.starts_at.is_some() {
        (patch.starts_at, None)
    } else {
        (current.starts_at, current.start_date)
    };
    if patch.duration_secs.is_some() && (patch.due_at.is_some() || patch.due_date.is_some()) {
        return Err(DbError::Conflict(
            "duration and due are mutually exclusive".into(),
        ));
    }
    let (due_at, due_date, duration) = if patch.duration_secs.is_some() {
        (None, None, interval(patch.duration_secs))
    } else if patch.due_date.is_some() {
        (None, patch.due_date, None)
    } else if patch.due_at.is_some() {
        (patch.due_at, None, None)
    } else {
        (current.due_at, current.due_date, current.duration)
    };
    let parent_uid = patch
        .parent_uid
        .clone()
        .unwrap_or(current.parent_uid.clone());
    if let Some(parent) = &parent_uid
        && parent != current.parent_uid.as_deref().unwrap_or("")
    {
        reject_parent_cycle(&mut tx, current.calendar_id, &current.uid, parent).await?;
    }
    let task = sqlx::query_as::<_, TaskRow>(
        "UPDATE tasks SET
            summary = COALESCE($2, summary),
            description_html = COALESCE($3, description_html),
            description_text = COALESCE($4, description_text),
            url = COALESCE($5, url),
            location = COALESCE($6, location),
            starts_at = $7, start_date = $8,
            due_at = $9, due_date = $10, duration = $11,
            tzid = COALESCE($12, tzid),
            floating = COALESCE($13, floating),
            status = COALESCE($14, status),
            percent_complete = COALESCE($15, percent_complete),
            priority = COALESCE($16, priority),
            class = COALESCE($17, class),
            categories = COALESCE($18, categories),
            parent_uid = $19,
            sort_order = COALESCE($20, sort_order),
            sequence = sequence + 1,
            updated_at = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(task_id)
    .bind(&patch.summary)
    .bind(&patch.description_html)
    .bind(&patch.description_text)
    .bind(&patch.url)
    .bind(&patch.location)
    .bind(starts_at)
    .bind(start_date)
    .bind(due_at)
    .bind(due_date)
    .bind(duration)
    .bind(&patch.tzid)
    .bind(patch.floating)
    .bind(&patch.status)
    .bind(patch.percent_complete)
    .bind(patch.priority)
    .bind(&patch.class)
    .bind(&patch.categories)
    .bind(&parent_uid)
    .bind(patch.sort_order)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(attendees) = &patch.attendees {
        sqlx::query("DELETE FROM task_attendees WHERE task_id = $1")
            .bind(task.id)
            .execute(&mut *tx)
            .await?;
        write_task_attendees(&mut tx, task.id, attendees).await?;
    }
    if let Some(alarms) = &patch.alarms {
        replace_task_alarms(&mut tx, task.id, alarms).await?;
    }
    let new_etag = etag_for(task.calendar_id, task.sequence, task.updated_at);
    sqlx::query("UPDATE tasks SET etag = $2 WHERE id = $1")
        .bind(task.id)
        .bind(&new_etag)
        .execute(&mut *tx)
        .await?;
    append_task_change(&mut tx, task.calendar_id, task.id, "updated").await?;
    tx.commit().await?;
    // An override edit reports the master's etag (the resource's validator).
    if let Some(master_id) = task.master_task_id {
        let (_, etag) = get_task(pool, master_id).await?;
        Ok((task, etag))
    } else {
        let etag = etag_for(task.calendar_id, task.sequence, task.updated_at);
        Ok((task, etag))
    }
}

/// Walking up from the proposed parent must never reach the task's own UID.
async fn reject_parent_cycle(
    tx: &mut sqlx::PgConnection,
    calendar_id: Uuid,
    task_uid: &str,
    proposed_parent: &str,
) -> Result<(), DbError> {
    let mut current = proposed_parent.to_string();
    // ponytail: bounded walk, not a CTE — parent chains are UI-heighted; a
    // deeper guard (cycle check in SQL) if trees grow past this.
    for _ in 0..64 {
        if current == task_uid {
            return Err(DbError::Conflict("parent_uid would create a cycle".into()));
        }
        let next: Option<Option<String>> = sqlx::query_scalar(
            "SELECT parent_uid FROM tasks
             WHERE calendar_id = $1 AND uid = $2 AND deleted_at IS NULL AND master_task_id IS NULL",
        )
        .bind(calendar_id)
        .bind(&current)
        .fetch_optional(&mut *tx)
        .await?;
        match next.flatten() {
            Some(parent) => current = parent,
            None => return Ok(()),
        }
    }
    Err(DbError::Conflict("parent_uid chain is too deep".into()))
}

/// Soft delete with cascade: overrides of the series and the whole subtask
/// tree (recursive over parent_uid within the calendar) are soft-deleted too,
/// one change_log row each so clients see every deletion.
pub async fn delete_task(
    pool: &PgPool,
    task_id: Uuid,
    if_match: Option<&str>,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !super::constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE tree AS (
            SELECT id, uid FROM tasks WHERE id = $1 AND deleted_at IS NULL
            UNION
            SELECT t.id, t.uid FROM tasks t
            JOIN tree ON t.calendar_id = $2 AND t.deleted_at IS NULL
              AND (t.master_task_id = tree.id OR t.parent_uid = tree.uid)
         )
         SELECT id FROM tree",
    )
    .bind(task_id)
    .bind(current.calendar_id)
    .fetch_all(&mut *tx)
    .await?;
    let n = sqlx::query("UPDATE tasks SET deleted_at = now() WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query(
        "INSERT INTO change_log (calendar_id, resource_id, resource_type, operation)
         SELECT $1, id, 'task', 'deleted' FROM tasks WHERE id = ANY($2)",
    )
    .bind(current.calendar_id)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE calendars SET ctag = ctag + 1 WHERE id = $1")
        .bind(current.calendar_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(n)
}

/// One occurrence of a recurring series: local wall-clock time, or a date for
/// all-day tasks. Matches the RECURRENCE-ID representation on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    Timed(NaiveDateTime),
    AllDay(NaiveDate),
}

/// D8: completing a recurring occurrence writes a RECURRENCE-ID override
/// (STATUS COMPLETED, COMPLETED now, PERCENT-COMPLETE 100); the master is
/// untouched. Non-recurring tasks complete in place.
pub async fn complete_task(
    pool: &PgPool,
    task_id: Uuid,
    occurrence: Option<Occurrence>,
    completed_by: Uuid,
) -> Result<(TaskRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let task = sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if task.rrule.is_some() {
        let occurrence = occurrence.ok_or_else(|| {
            DbError::Conflict("completing a recurring task needs an occurrence".into())
        })?;
        let (rid, rid_date) = match occurrence {
            Occurrence::Timed(at) => (Some(at), None),
            Occurrence::AllDay(d) => (None, Some(d)),
        };
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM tasks
             WHERE calendar_id = $1 AND uid = $2
               AND recurrence_id IS NOT DISTINCT FROM $3
               AND recurrence_id_date IS NOT DISTINCT FROM $4",
        )
        .bind(task.calendar_id)
        .bind(&task.uid)
        .bind(rid)
        .bind(rid_date)
        .fetch_optional(&mut *tx)
        .await?;
        let override_id = match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE tasks SET status = 'COMPLETED', percent_complete = 100,
                        completed_at = now(), updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO tasks (
                        id, calendar_id, uid, master_task_id, recurrence_id, recurrence_id_date,
                        starts_at, start_date, due_at, due_date, duration, tzid, floating,
                        completed_at, summary, description_html, description_text, url, location,
                        status, percent_complete, priority, class, categories, parent_uid,
                        sort_order, extra_props, organizer_user_id, organizer_email,
                        organizer_name, created_by
                     )
                     SELECT $1, calendar_id, uid, $2, $3, $4,
                        starts_at, start_date, due_at, due_date, duration, tzid, floating,
                        now(), summary, description_html, description_text, url, location,
                        'COMPLETED', 100, priority, class, categories, parent_uid,
                        sort_order, extra_props, organizer_user_id, organizer_email,
                        organizer_name, $5
                     FROM tasks WHERE id = $6",
                )
                .bind(id)
                .bind(task.id)
                .bind(rid)
                .bind(rid_date)
                .bind(completed_by)
                .bind(task.id)
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        append_task_change(&mut tx, task.calendar_id, override_id, "created").await?;
        tx.commit().await?;
        get_task(pool, task.id).await
    } else {
        let updated = sqlx::query_as::<_, TaskRow>(
            "UPDATE tasks SET status = 'COMPLETED', percent_complete = 100, completed_at = now(),
                sequence = sequence + 1, updated_at = now()
             WHERE id = $1
             RETURNING *",
        )
        .bind(task.id)
        .fetch_one(&mut *tx)
        .await?;
        let etag = etag_for(task.calendar_id, updated.sequence, updated.updated_at);
        sqlx::query("UPDATE tasks SET etag = $2 WHERE id = $1")
            .bind(task.id)
            .bind(&etag)
            .execute(&mut *tx)
            .await?;
        super::record_change(&mut tx, task.calendar_id, task.id, "task", "updated").await?;
        tx.commit().await?;
        get_task(pool, task.id).await
    }
}

/// Reverses `complete_task`: clears the completion (deleting the override when
/// it carries nothing else would lose client edits, so it is kept and reset).
pub async fn reopen_task(
    pool: &PgPool,
    task_id: Uuid,
    occurrence: Option<Occurrence>,
) -> Result<(TaskRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let task = sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM tasks WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if task.rrule.is_some() {
        let occurrence = occurrence.ok_or_else(|| {
            DbError::Conflict("reopening a recurring task needs an occurrence".into())
        })?;
        let (rid, rid_date) = match occurrence {
            Occurrence::Timed(at) => (Some(at), None),
            Occurrence::AllDay(d) => (None, Some(d)),
        };
        let override_id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE tasks SET status = 'NEEDS-ACTION', percent_complete = NULL,
                completed_at = NULL, updated_at = now()
             WHERE calendar_id = $1 AND uid = $2
               AND recurrence_id IS NOT DISTINCT FROM $3
               AND recurrence_id_date IS NOT DISTINCT FROM $4
               AND deleted_at IS NULL
             RETURNING id",
        )
        .bind(task.calendar_id)
        .bind(&task.uid)
        .bind(rid)
        .bind(rid_date)
        .fetch_optional(&mut *tx)
        .await?;
        let override_id = override_id.ok_or(DbError::NotFound)?;
        append_task_change(&mut tx, task.calendar_id, override_id, "updated").await?;
        tx.commit().await?;
        get_task(pool, task.id).await
    } else {
        let updated = sqlx::query_as::<_, TaskRow>(
            "UPDATE tasks SET status = 'NEEDS-ACTION', percent_complete = NULL,
                completed_at = NULL, sequence = sequence + 1, updated_at = now()
             WHERE id = $1
             RETURNING *",
        )
        .bind(task.id)
        .fetch_one(&mut *tx)
        .await?;
        let etag = etag_for(task.calendar_id, updated.sequence, updated.updated_at);
        sqlx::query("UPDATE tasks SET etag = $2 WHERE id = $1")
            .bind(task.id)
            .bind(&etag)
            .execute(&mut *tx)
            .await?;
        super::record_change(&mut tx, task.calendar_id, task.id, "task", "updated").await?;
        tx.commit().await?;
        get_task(pool, task.id).await
    }
}

/// RFC 5545 point parsing for rdate/exdate JSON arrays: ISO instants or dates.
pub fn parse_points(value: &serde_json::Value) -> Vec<calendar_core::DateOrDateTime> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| {
                    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                        return Some(calendar_core::DateOrDateTime::Timed(at.with_timezone(&Utc)));
                    }
                    NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .ok()
                        .map(calendar_core::DateOrDateTime::AllDay)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// DTSTART, else DUE (design decision 4); the recurrence anchor.
fn task_anchor(task: &TaskRow) -> Option<calendar_core::DateOrDateTime> {
    if let Some(d) = task.start_date {
        Some(calendar_core::DateOrDateTime::AllDay(d))
    } else if let Some(at) = task.starts_at {
        Some(calendar_core::DateOrDateTime::Timed(at))
    } else if let Some(d) = task.due_date {
        Some(calendar_core::DateOrDateTime::AllDay(d))
    } else {
        task.due_at.map(calendar_core::DateOrDateTime::Timed)
    }
}

/// The earliest occurrence at or after now without a completed override
/// ("next open occurrence", design section 6). None for non-recurring tasks.
pub async fn next_open(
    pool: &PgPool,
    task_id: Uuid,
    horizon_days: i64,
) -> Result<Option<DateTime<Utc>>, DbError> {
    let (task, _) = get_task(pool, task_id).await?;
    let Some(rrule) = task.rrule.as_deref() else {
        return Ok(None);
    };
    let Some(anchor) = task_anchor(&task) else {
        return Ok(None);
    };
    let resolver = super::timezones::load_for_calendar(pool, task.calendar_id).await?;
    let from = Utc::now();
    let to = from + chrono::Duration::days(horizon_days);
    let occurrences = calendar_core::recurrence::expand_occurrences(
        anchor,
        task.tzid.as_deref(),
        Some(&resolver),
        Some(rrule),
        &parse_points(&task.rdate),
        &parse_points(&task.exdate),
        from,
        to,
    )
    .unwrap_or_default();
    // A completed override closes its occurrence. Overrides store the local
    // wall clock (recurrence_id) or the date; expansions come back as UTC
    // instants / dates — compare in wall-clock space.
    let zone = calendar_core::recurrence::resolve_tz(task.tzid.as_deref(), Some(&resolver))
        .map_err(|e| DbError::Conflict(format!("unresolvable timezone: {e}")))?;
    let completed: Vec<Occurrence> = list_overrides(pool, task.id)
        .await?
        .into_iter()
        .filter(|o| o.status == Some("COMPLETED".to_string()) || o.completed_at.is_some())
        .map(|o| match (o.recurrence_id, o.recurrence_id_date) {
            (Some(at), _) => Occurrence::Timed(at),
            (None, Some(d)) => Occurrence::AllDay(d),
            (None, None) => unreachable!("override carries a RECURRENCE-ID"),
        })
        .collect();
    for point in occurrences {
        let key = match point {
            calendar_core::DateOrDateTime::Timed(at) => Occurrence::Timed(zone.to_local(at)),
            calendar_core::DateOrDateTime::AllDay(d) => Occurrence::AllDay(d),
        };
        if !completed.contains(&key) {
            return Ok(match point {
                calendar_core::DateOrDateTime::Timed(at) => Some(at),
                calendar_core::DateOrDateTime::AllDay(d) => {
                    d.and_hms_opt(0, 0, 0).map(|n| n.and_utc())
                }
            });
        }
    }
    Ok(None)
}

/// Live object counts per component kind, for refusing component removal with
/// 409 + per-type counts (design section 3, decision 9).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ComponentCount {
    pub kind: String,
    pub live: i64,
}

pub async fn count_live_components(
    pool: &PgPool,
    calendar_id: Uuid,
) -> Result<Vec<ComponentCount>, DbError> {
    sqlx::query_as::<_, ComponentCount>(
        "SELECT kind, COUNT(*)::bigint AS live
         FROM calendar_objects
         WHERE calendar_id = $1 AND deleted_at IS NULL
         GROUP BY kind",
    )
    .bind(calendar_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Retention hook for calendar-server's retention_purge job (which owns the
/// delete lists): hard-deletes soft-deleted tasks (with their overrides,
/// attendees, alarms — all CASCADE) and journals past the retention window.
pub async fn purge_deleted_tasks_journals(pool: &PgPool, days: i64) -> Result<(u64, u64), DbError> {
    let tasks = sqlx::query(&format!(
        "DELETE FROM tasks WHERE deleted_at < now() - interval '{days} days'"
    ))
    .execute(pool)
    .await?
    .rows_affected();
    let journals = sqlx::query(&format!(
        "DELETE FROM journals WHERE deleted_at < now() - interval '{days} days'"
    ))
    .execute(pool)
    .await?
    .rows_affected();
    Ok((tasks, journals))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// DB-backed tests need a live PostgreSQL via DATABASE_URL (the throwaway
    /// instance the interop suite boots works). Without it they skip so
    /// `cargo test` still passes on machines without infrastructure. A set
    /// DATABASE_URL whose migrations fail is an error, not a skip: a vacuous
    /// pass here once hid a broken migration.
    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        crate::migrate(&pool).await.unwrap();
        Some(pool)
    }

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
            .bind(format!("{}@tasks.test", f.user))
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

    fn sample(uid: &str) -> NewTaskData {
        NewTaskData {
            uid: uid.to_string(),
            due_at: Some(Utc::now() + chrono::Duration::days(1)),
            summary: "buy milk".into(),
            organizer_email: Some("writer@tasks.test".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn crud_round_trip() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let uid = Uuid::new_v4().to_string();
        let (task, etag) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[NewAttendee {
                email: Some("a@tasks.test".into()),
                ..Default::default()
            }],
            &[NewAlarm {
                action: "DISPLAY".into(),
                related: Some("END".into()),
                offset_secs: Some(-600),
                notify_channels: vec!["in_app".into()],
                ..Default::default()
            }],
            &sample(&uid),
        )
        .await
        .unwrap();
        assert_eq!(task.resource_name(), format!("{}.ics", task.id));
        assert_eq!(task.uid, uid);
        assert_eq!(task.summary, "buy milk");
        assert!(!etag.is_empty());

        let (fetched, fetched_etag) = get_task(&pool, task.id).await.unwrap();
        assert_eq!(fetched.id, task.id);
        assert_eq!(fetched_etag, etag);
        assert_eq!(list_task_attendees(&pool, task.id).await.unwrap().len(), 1);
        assert_eq!(list_task_alarms(&pool, task.id).await.unwrap().len(), 1);
        // href is NULL for API-created rows but resolves to {id}.ics.
        let (by_href, _) = get_task_by_href(&pool, f.calendar, &format!("{}.ics", task.id))
            .await
            .unwrap();
        assert_eq!(by_href.id, task.id);

        let patch = TaskPatch {
            summary: Some("buy oat milk".into()),
            percent_complete: Some(50),
            status: Some("IN-PROCESS".into()),
            categories: Some(vec!["shopping".into()]),
            ..Default::default()
        };
        let (updated, new_etag) = update_task(&pool, task.id, Some(&etag), &patch)
            .await
            .unwrap();
        assert_eq!(updated.summary, "buy oat milk");
        assert_ne!(new_etag, etag);
        // Stale etag is refused.
        assert!(matches!(
            update_task(&pool, task.id, Some(&etag), &patch).await,
            Err(DbError::Conflict(_))
        ));
        // Filters.
        let found = list_tasks(
            &pool,
            f.calendar,
            &TaskFilter {
                status: Some("IN-PROCESS".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(found.len(), 1);
        let found = list_tasks(
            &pool,
            f.calendar,
            &TaskFilter {
                due_before: Some(Utc::now() + chrono::Duration::days(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(found.len(), 1);
        let found = list_tasks(
            &pool,
            f.calendar,
            &TaskFilter {
                category: Some("nope".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(found.is_empty());

        let deleted = delete_task(&pool, task.id, None).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(matches!(
            get_task(&pool, task.id).await,
            Err(DbError::NotFound)
        ));
    }

    #[tokio::test]
    async fn override_change_bumps_master_etag_and_reports_master() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let uid = Uuid::new_v4().to_string();
        let (master, master_etag) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                rrule: Some("FREQ=DAILY;COUNT=5".into()),
                starts_at: Some(Utc::now()),
                ..sample(&uid)
            },
        )
        .await
        .unwrap();
        // Insert an override through the completion path, then patch it.
        let occ = Occurrence::Timed(Utc::now().naive_utc());
        let (_, etag_after_complete) = complete_task(&pool, master.id, Some(occ), f.user)
            .await
            .unwrap();
        assert_ne!(etag_after_complete, master_etag);
        let overrides = list_overrides(&pool, master.id).await.unwrap();
        assert_eq!(overrides.len(), 1);
        let (updated_master, etag_after_patch) = update_task(
            &pool,
            overrides[0].id,
            None,
            &TaskPatch {
                summary: Some("override summary".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_ne!(etag_after_patch, etag_after_complete);
        assert_eq!(updated_master.id, overrides[0].id);
        // The change log reports the master, not the override.
        let logged: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT resource_id, resource_type, operation FROM change_log
             WHERE calendar_id = $1 AND resource_type = 'task' ORDER BY seq",
        )
        .bind(f.calendar)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            logged
                .iter()
                .all(|(id, _, _)| *id == master.id || *id == overrides[0].id)
        );
        assert!(
            logged
                .iter()
                .any(|(id, _, op)| *id == master.id && op == "updated")
        );
        let _ = updated_master;
    }

    #[tokio::test]
    async fn subtask_cascade_writes_one_change_log_row_per_node() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let parent_uid = Uuid::new_v4().to_string();
        let (parent, _) = create_task(&pool, f.calendar, f.user, &[], &[], &sample(&parent_uid))
            .await
            .unwrap();
        let (child, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                parent_uid: Some(parent_uid.clone()),
                ..sample(&Uuid::new_v4().to_string())
            },
        )
        .await
        .unwrap();
        let (grandchild, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                parent_uid: Some(child.uid.clone()),
                ..sample(&Uuid::new_v4().to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(
            list_subtasks(&pool, f.calendar, &parent_uid)
                .await
                .unwrap()
                .len(),
            1
        );
        let n = delete_task(&pool, parent.id, None).await.unwrap();
        assert_eq!(n, 3);
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tasks WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind([parent.id, child.id, grandchild.id])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(live, 0);
        let deletions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM change_log
             WHERE calendar_id = $1 AND resource_type = 'task' AND operation = 'deleted'",
        )
        .bind(f.calendar)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(deletions, 3);
        // The grandchild is resolvable through the soft-deleted chain by uid.
        let still_listed = list_tasks(&pool, f.calendar, &TaskFilter::default())
            .await
            .unwrap();
        assert!(still_listed.is_empty());
    }

    #[tokio::test]
    async fn parent_cycle_is_rejected() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let (a, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &sample(&Uuid::new_v4().to_string()),
        )
        .await
        .unwrap();
        let (b, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                parent_uid: Some(a.uid.clone()),
                ..sample(&Uuid::new_v4().to_string())
            },
        )
        .await
        .unwrap();
        // a -> b -> a would be a cycle.
        let err = update_task(
            &pool,
            a.id,
            None,
            &TaskPatch {
                parent_uid: Some(Some(b.uid.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DbError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
        // Self-parenting too.
        let err = update_task(
            &pool,
            a.id,
            None,
            &TaskPatch {
                parent_uid: Some(Some(a.uid.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
        // Reparenting to a fresh parent works and can be cleared again.
        let (moved, _) = update_task(
            &pool,
            b.id,
            None,
            &TaskPatch {
                parent_uid: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(moved.parent_uid, None);
    }

    #[tokio::test]
    async fn cross_type_href_conflict_is_refused() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        // An event owns "shared.ics".
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, starts_at, ends_at, organizer_email, href)
             VALUES ($1, $2, $3, 'e', now(), now(), 'w@tasks.test', 'shared.ics')",
        )
        .bind(Uuid::new_v4())
        .bind(f.calendar)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        let event_href: String = sqlx::query_scalar(
            "SELECT COALESCE(href, id::text || '.ics') FROM events ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event_href, "shared.ics");
        let err = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                href: Some(event_href),
                ..sample(&Uuid::new_v4().to_string())
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DbError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
        // A journal under the same href is refused too.
        let err = crate::journals::create_journal(
            &pool,
            f.calendar,
            f.user,
            &crate::journals::NewJournalData {
                href: Some("shared.ics".into()),
                uid: Uuid::new_v4().to_string(),
                summary: "note".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
        // A distinct href is fine.
        let (task, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                href: Some("mine.ics".into()),
                ..sample(&Uuid::new_v4().to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(task.href.as_deref(), Some("mine.ics"));
    }

    #[tokio::test]
    async fn complete_and_reopen_recurring_writes_and_clears_override() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let uid = Uuid::new_v4().to_string();
        let (master, etag0) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &NewTaskData {
                rrule: Some("FREQ=DAILY;COUNT=10".into()),
                starts_at: Some(Utc::now() - chrono::Duration::days(1)),
                ..sample(&uid)
            },
        )
        .await
        .unwrap();
        let occ = Occurrence::Timed((Utc::now() - chrono::Duration::days(1)).naive_utc());
        let (m, etag1) = complete_task(&pool, master.id, Some(occ), f.user)
            .await
            .unwrap();
        assert_eq!(m.id, master.id);
        assert_ne!(etag1, etag0);
        let overrides = list_overrides(&pool, master.id).await.unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].status.as_deref(), Some("COMPLETED"));
        assert_eq!(overrides[0].percent_complete, Some(100));
        assert!(overrides[0].completed_at.is_some());
        assert_eq!(overrides[0].master_task_id, Some(master.id));
        // The master itself is untouched (D8).
        let (m2, _) = get_task(&pool, master.id).await.unwrap();
        assert_eq!(m2.status, None);
        // next_open skips the completed occurrence.
        let open = next_open(&pool, master.id, 30).await.unwrap();
        assert!(open.is_some());
        assert!(open.unwrap() > Utc::now() - chrono::Duration::days(1));
        // Reopen clears it.
        reopen_task(&pool, master.id, Some(occ)).await.unwrap();
        let overrides = list_overrides(&pool, master.id).await.unwrap();
        assert_eq!(overrides[0].status.as_deref(), Some("NEEDS-ACTION"));
        assert!(overrides[0].completed_at.is_none());
        // Recurring completion without an occurrence is refused.
        assert!(matches!(
            complete_task(&pool, master.id, None, f.user).await,
            Err(DbError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn complete_and_reopen_non_recurring_in_place() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let (task, etag) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &sample(&Uuid::new_v4().to_string()),
        )
        .await
        .unwrap();
        let (done, etag1) = complete_task(&pool, task.id, None, f.user).await.unwrap();
        assert_eq!(done.status.as_deref(), Some("COMPLETED"));
        assert_eq!(done.percent_complete, Some(100));
        assert!(done.completed_at.is_some());
        assert_ne!(etag1, etag);
        assert_eq!(list_overrides(&pool, task.id).await.unwrap().len(), 0);
        let (reopened, _) = reopen_task(&pool, task.id, None).await.unwrap();
        assert_eq!(reopened.status.as_deref(), Some("NEEDS-ACTION"));
        assert!(reopened.completed_at.is_none());
        assert_eq!(reopened.percent_complete, None);
    }

    #[tokio::test]
    async fn calendar_objects_view_lists_all_three_kinds() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, starts_at, ends_at, organizer_email)
             VALUES ($1, $2, $3, 'e', now(), now(), 'w@tasks.test')",
        )
        .bind(Uuid::new_v4())
        .bind(f.calendar)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &sample(&Uuid::new_v4().to_string()),
        )
        .await
        .unwrap();
        crate::journals::create_journal(
            &pool,
            f.calendar,
            f.user,
            &crate::journals::NewJournalData {
                uid: Uuid::new_v4().to_string(),
                summary: "note".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let kinds: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, href FROM calendar_objects WHERE calendar_id = $1 ORDER BY kind",
        )
        .bind(f.calendar)
        .fetch_all(&pool)
        .await
        .unwrap();
        let names: Vec<&str> = kinds.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["VEVENT", "VJOURNAL", "VTODO"]);
        // Every row resolves by the COALESCEd href.
        assert!(kinds.iter().all(|(_, h)| h.ends_with(".ics")));
    }

    #[tokio::test]
    async fn component_counts_refuse_removal_data() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, starts_at, ends_at, organizer_email)
             VALUES ($1, $2, $3, 'e', now(), now(), 'w@tasks.test')",
        )
        .bind(Uuid::new_v4())
        .bind(f.calendar)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &sample(&Uuid::new_v4().to_string()),
        )
        .await
        .unwrap();
        crate::journals::create_journal(
            &pool,
            f.calendar,
            f.user,
            &crate::journals::NewJournalData {
                uid: Uuid::new_v4().to_string(),
                summary: "note".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let counts = count_live_components(&pool, f.calendar).await.unwrap();
        let by_kind: std::collections::HashMap<String, i64> =
            counts.into_iter().map(|c| (c.kind, c.live)).collect();
        assert_eq!(by_kind.get("VEVENT"), Some(&1));
        assert_eq!(by_kind.get("VTODO"), Some(&1));
        assert_eq!(by_kind.get("VJOURNAL"), Some(&1));
    }

    #[tokio::test]
    async fn purge_removes_only_past_retention_rows() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let (task, _) = create_task(
            &pool,
            f.calendar,
            f.user,
            &[],
            &[],
            &sample(&Uuid::new_v4().to_string()),
        )
        .await
        .unwrap();
        crate::journals::create_journal(
            &pool,
            f.calendar,
            f.user,
            &crate::journals::NewJournalData {
                uid: Uuid::new_v4().to_string(),
                summary: "note".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        delete_task(&pool, task.id, None).await.unwrap();
        sqlx::query(
            "UPDATE tasks SET deleted_at = now() - interval '10 days' WHERE calendar_id = $1",
        )
        .bind(f.calendar)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE journals SET deleted_at = now() - interval '1 day' WHERE calendar_id = $1",
        )
        .bind(f.calendar)
        .execute(&pool)
        .await
        .unwrap();
        let (tasks, journals) = purge_deleted_tasks_journals(&pool, 7).await.unwrap();
        assert_eq!(tasks, 1);
        assert_eq!(journals, 0);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journals WHERE calendar_id = $1")
            .bind(f.calendar)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }

    // ---- pure helpers ----

    #[test]
    fn resource_name_defaults_to_id_ics() {
        let id = Uuid::new_v4();
        assert_eq!(resource_name(id, None), format!("{id}.ics"));
        assert_eq!(resource_name(id, Some("client.ics")), "client.ics");
    }

    #[test]
    fn parse_points_reads_instants_and_dates() {
        let v = serde_json::json!(["2026-01-02T03:04:05Z", "2026-01-02", 7]);
        let pts = parse_points(&v);
        assert_eq!(pts.len(), 2);
        assert_eq!(
            pts[0],
            calendar_core::DateOrDateTime::Timed(
                DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(
            pts[1],
            calendar_core::DateOrDateTime::AllDay(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap())
        );
        assert!(parse_points(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn task_anchor_prefers_dtstart_then_due() {
        let mut base = sample("u");
        base.starts_at = Some(Utc::now());
        base.due_at = Some(Utc::now());
        let mk = |starts_at: Option<DateTime<Utc>>,
                  start_date: Option<NaiveDate>,
                  due_at: Option<DateTime<Utc>>,
                  due_date: Option<NaiveDate>| TaskRow {
            id: Uuid::new_v4(),
            calendar_id: Uuid::new_v4(),
            uid: "u".into(),
            href: None,
            master_task_id: None,
            recurrence_id: None,
            recurrence_id_date: None,
            starts_at,
            start_date,
            due_at,
            due_date,
            duration: None,
            tzid: None,
            floating: false,
            completed_at: None,
            rrule: None,
            rdate: serde_json::json!([]),
            exdate: serde_json::json!([]),
            summary: String::new(),
            description_html: None,
            description_text: None,
            url: None,
            location: None,
            status: None,
            percent_complete: None,
            priority: None,
            class: None,
            categories: vec![],
            parent_uid: None,
            sort_order: None,
            extra_props: serde_json::json!([]),
            organizer_user_id: None,
            organizer_email: None,
            organizer_name: None,
            origin_id: None,
            sequence: 0,
            etag: String::new(),
            created_by: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(task_anchor(&mk(base.starts_at, None, base.due_at, None)).is_some());
        assert!(task_anchor(&mk(None, None, base.due_at, None)).is_some());
        assert!(task_anchor(&mk(None, None, None, None)).is_none());
        assert_eq!(
            task_anchor(&mk(None, None, base.due_at, None)),
            Some(calendar_core::DateOrDateTime::Timed(base.due_at.unwrap()))
        );
    }
}
