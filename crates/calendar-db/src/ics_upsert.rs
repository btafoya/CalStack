//! CalDAV PUT storage: create/update/delete event rows from parsed iCalendar
//! data (one resource = one VEVENT). Kept here so calendar-caldav only maps
//! wire types onto it.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{DbError, EventRow, NewLocation};

/// Normalized event fields for the upsert path. Mirrors the events table; the
/// mapping from RFC 5545 properties lives in calendar-caldav.
#[derive(Debug, Default)]
pub struct IcsEventUpsert {
    pub uid: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub end_date: Option<chrono::NaiveDate>,
    pub duration_secs: Option<i64>,
    pub tzid: Option<String>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub rdate: serde_json::Value,
    pub exdate: serde_json::Value,
    pub summary: Option<String>,
    pub description_text: Option<String>,
    pub description_html: Option<String>,
    pub url: Option<String>,
    pub location_text: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub transp: Option<String>,
    pub categories: Vec<String>,
    pub sequence: Option<i32>,
    pub recurrence_id: Option<chrono::NaiveDateTime>,
    pub recurrence_id_date: Option<chrono::NaiveDate>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<IcsAttendee>,
    pub alarms: Vec<super::alarms::NewAlarm>,
}

#[derive(Debug, Default)]
pub struct IcsAttendee {
    pub email: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    pub rsvp: Option<bool>,
}

/// Resolves a parsed LOCATION property to a location row id: creates a new
/// row (locations are append-only, matching the web UI's write path) when
/// text is present, or None when the resource carries no LOCATION.
async fn resolve_location(
    pool: &sqlx::PgPool,
    location_text: &Option<String>,
) -> Result<Option<Uuid>, DbError> {
    match location_text {
        Some(text) if !text.is_empty() => {
            let loc = super::create_location(
                pool,
                &NewLocation {
                    display_name: Some(text.clone()),
                    ..Default::default()
                },
            )
            .await?;
            Ok(Some(loc.id))
        }
        _ => Ok(None),
    }
}

/// Creates the event row. When `resource_id` is given (the CalDAV URL's
/// uuid) the row id matches the URL; conflicts on (calendar, uid,
/// occurrence) surface as DbError::Conflict.
#[allow(clippy::too_many_arguments)]
pub async fn create_ics_event_inner(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    created_by: Uuid,
    organizer_user_id: Option<Uuid>,
    resource_id: Option<Uuid>,
    data: &IcsEventUpsert,
) -> Result<EventRow, DbError> {
    let location_id = resolve_location(pool, &data.location_text).await?;
    let mut tx = pool.begin().await?;
    let event = sqlx::query_as::<_, EventRow>(
        "INSERT INTO events (
            id, calendar_id, uid, master_event_id, recurrence_id, recurrence_id_date,
            starts_at, ends_at, start_date, end_date, duration, tzid, all_day,
            rrule, rdate, exdate,
            summary, description_html, description_text, url,
            status, priority, class, transp, categories, location_id,
            organizer_user_id, organizer_email, organizer_name, created_by
         ) VALUES (
            $1, $2, $3, $4, $5, $6,
            $7, $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17, $18, $19,
            $20, $21, $22, $23, $24, $25,
            $26, $27, $28, $29, $30
         )
         RETURNING *",
    )
    .bind(resource_id.unwrap_or_else(Uuid::new_v4))
    .bind(calendar_id)
    .bind(&data.uid)
    .bind(master_event_id(pool, calendar_id, data).await?)
    .bind(data.recurrence_id)
    .bind(data.recurrence_id_date)
    .bind(data.starts_at)
    .bind(data.ends_at)
    .bind(data.start_date)
    .bind(data.end_date)
    .bind(
        data.duration_secs
            .map(|s| sqlx::postgres::types::PgInterval {
                months: 0,
                days: 0,
                microseconds: s * 1_000_000,
            }),
    )
    .bind(&data.tzid)
    .bind(data.all_day)
    .bind(&data.rrule)
    .bind(&data.rdate)
    .bind(&data.exdate)
    .bind(data.summary.as_deref().unwrap_or(""))
    .bind(&data.description_html)
    .bind(&data.description_text)
    .bind(&data.url)
    .bind(&data.status)
    .bind(data.priority)
    .bind(&data.class)
    .bind(&data.transp)
    .bind(&data.categories)
    .bind(location_id)
    .bind(organizer_user_id)
    .bind(&data.organizer_email)
    .bind(&data.organizer_name)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("event uid/recurrence-id already exists".into())
        }
        other => other.into(),
    })?;
    write_attendees(&mut tx, event.id, &data.attendees).await?;
    let etag = super::event_etag(&event);
    sqlx::query("UPDATE events SET etag = $2 WHERE id = $1")
        .bind(event.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO change_log (calendar_id, resource_id, operation) VALUES ($1, $2, 'created')",
    )
    .bind(calendar_id)
    .bind(event.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE calendars SET ctag = ctag + 1 WHERE id = $1")
        .bind(calendar_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if !data.alarms.is_empty() {
        super::alarms::replace_alarms(pool, event.id, &data.alarms).await?;
    }
    Ok(event)
}

/// Replaces the full row content of an existing resource (PUT to a known URL).
pub async fn update_ics_event(
    pool: &sqlx::PgPool,
    event_id: Uuid,
    if_match: Option<&str>,
    data: &IcsEventUpsert,
) -> Result<EventRow, DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, EventRow>(
        "SELECT * FROM events WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !current.etag.is_empty()
        && !current
            .etag
            .trim_matches('"')
            .eq(expected.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    // A PUT replaces the whole resource, so an absent LOCATION clears it
    // (unlike the partial-patch API, which leaves fields it doesn't mention
    // untouched).
    let location_id = resolve_location(pool, &data.location_text).await?;
    // Recurring masters: RRULE updates are fine; exceptions never carry RRULE.
    let event = sqlx::query_as::<_, EventRow>(
        "UPDATE events SET
            uid = $2,
            starts_at = $3, ends_at = $4, start_date = $5, end_date = $6,
            duration = $7, tzid = $8, all_day = $9,
            rrule = $10, rdate = $11, exdate = $12,
            summary = $13, description_html = $14, description_text = $15, url = $16,
            status = $17, priority = $18, class = $19, transp = $20, categories = $21,
            organizer_email = $22, organizer_name = $23,
            sequence = GREATEST($24, sequence) + 1,
            location_id = $25,
            updated_at = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(event_id)
    .bind(&data.uid)
    .bind(data.starts_at)
    .bind(data.ends_at)
    .bind(data.start_date)
    .bind(data.end_date)
    .bind(
        data.duration_secs
            .map(|s| sqlx::postgres::types::PgInterval {
                months: 0,
                days: 0,
                microseconds: s * 1_000_000,
            }),
    )
    .bind(&data.tzid)
    .bind(data.all_day)
    .bind(&data.rrule)
    .bind(&data.rdate)
    .bind(&data.exdate)
    .bind(data.summary.as_deref().unwrap_or(""))
    .bind(&data.description_html)
    .bind(&data.description_text)
    .bind(&data.url)
    .bind(&data.status)
    .bind(data.priority)
    .bind(&data.class)
    .bind(&data.transp)
    .bind(&data.categories)
    .bind(data.organizer_email.clone())
    .bind(&data.organizer_name)
    .bind(data.sequence.unwrap_or(0))
    .bind(location_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM event_attendees WHERE event_id = $1")
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    write_attendees(&mut tx, event_id, &data.attendees).await?;
    let etag = super::event_etag(&event);
    sqlx::query("UPDATE events SET etag = $2 WHERE id = $1")
        .bind(event.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO change_log (calendar_id, resource_id, operation) VALUES ($1, $2, 'updated')",
    )
    .bind(event.calendar_id)
    .bind(event.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE calendars SET ctag = ctag + 1 WHERE id = $1")
        .bind(event.calendar_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    super::alarms::replace_alarms(pool, event.id, &data.alarms).await?;
    Ok(event)
}

/// The recurring master this parsed exception belongs to (same uid, no
/// master itself). PUT of an exception to a series without the master is
/// rejected.
async fn master_event_id(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    data: &IcsEventUpsert,
) -> Result<Option<Uuid>, DbError> {
    if data.recurrence_id.is_none() && data.recurrence_id_date.is_none() {
        return Ok(None);
    }
    sqlx::query_scalar(
        "SELECT id FROM events
         WHERE calendar_id = $1 AND uid = $2 AND master_event_id IS NULL AND deleted_at IS NULL",
    )
    .bind(calendar_id)
    .bind(&data.uid)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn write_attendees(
    tx: &mut sqlx::PgConnection,
    event_id: Uuid,
    attendees: &[IcsAttendee],
) -> Result<(), DbError> {
    for a in attendees {
        sqlx::query(
            "INSERT INTO event_attendees (id, event_id, email, display_name, role, partstat, rsvp)
             VALUES ($1, $2, $3, $4, COALESCE($5, 'REQ-PARTICIPANT'), COALESCE($6, 'NEEDS-ACTION'), $7)",
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(&a.email)
        .bind(&a.display_name)
        .bind(a.role.as_deref())
        .bind(a.partstat.as_deref())
        .bind(a.rsvp)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
