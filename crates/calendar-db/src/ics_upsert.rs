//! CalDAV PUT storage. A resource is a series (RFC 4791 4.1): one master
//! VEVENT plus its RECURRENCE-ID overrides, all sharing a UID. A PUT replaces
//! the whole series in one transaction. Kept here so calendar-caldav only maps
//! wire types onto it.

use chrono::{DateTime, Utc};
use sqlx::postgres::types::PgInterval;
use uuid::Uuid;

use super::{DbError, EventRow, NewLocation};

/// Optimistic-concurrency guard for a CalDAV PUT, re-verified inside the same
/// transaction as the write. dav-server checks `If-Match`/`If-None-Match` once
/// against request-start metadata, before the body is streamed; without this
/// re-check a concurrent PUT that commits in between is silently overwritten
/// (lost-update race). Mirrors the JSON API's in-transaction If-Match check in
/// `update_event`.
#[derive(Debug, Clone, PartialEq)]
pub enum PutPrecondition {
    /// No precondition header on the request.
    None,
    /// If-Match: the live resource at `href` must still carry this etag.
    MatchEtag(String),
    /// If-None-Match: * — the resource at `href` must not exist.
    NotExists,
}

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
    pub floating: bool,
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
    /// NOT NULL in the table: the caller defaults an organizer-less VEVENT to the writer.
    pub organizer_email: String,
    pub organizer_name: Option<String>,
    /// Set on insert only: the writer, when the organizer is the writer.
    pub organizer_user_id: Option<Uuid>,
    pub attendees: Vec<IcsAttendee>,
    pub alarms: Vec<super::alarms::NewAlarm>,
}

#[derive(Debug, Default)]
pub struct IcsAttendee {
    /// NULL for SMS-only attendees (`sms:` CAL-ADDRESS).
    pub email: Option<String>,
    pub telephone: Option<String>,
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

fn interval(secs: Option<i64>) -> Option<PgInterval> {
    secs.map(|s| PgInterval {
        months: 0,
        days: 0,
        microseconds: s * 1_000_000,
    })
}

/// Stores one PUT resource: the master (`overrides` empty for a plain event)
/// and its overrides, atomically, with a single change_log entry for the
/// resource. Returns the master row (its etag already refreshed) and whether
/// the resource is new to the client (created, or resurrected after a delete).
///
/// `href` is the filename the client PUT to. Overrides absent from the PUT
/// are removed. A UID that is live at another filename is a Conflict.
///
/// `precondition` is verified after the `FOR UPDATE` row lock, inside the
/// write transaction: a stale etag or a concurrently created resource aborts
/// the whole write with `DbError::Conflict`.
pub async fn put_series(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    created_by: Uuid,
    href: &str,
    master: &IcsEventUpsert,
    overrides: &[IcsEventUpsert],
    precondition: &PutPrecondition,
) -> Result<(EventRow, bool), DbError> {
    let master_location = resolve_location(pool, &master.location_text).await?;
    let mut override_locations = Vec::with_capacity(overrides.len());
    for o in overrides {
        override_locations.push(resolve_location(pool, &o.location_text).await?);
    }
    // A canonical "{uuid}.ics" URL keeps id == URL uuid and needs no stored
    // href; any other filename is remembered as the client sent it.
    let canonical = href
        .strip_suffix(".ics")
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|u| format!("{u}.ics") == href);

    let mut tx = pool.begin().await?;
    let at_href: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, etag FROM events
         WHERE calendar_id = $1 AND deleted_at IS NULL
           AND master_event_id IS NULL AND recurrence_id IS NULL AND recurrence_id_date IS NULL
           AND COALESCE(href, id::text || '.ics') = $2
         FOR UPDATE",
    )
    .bind(calendar_id)
    .bind(href)
    .fetch_optional(&mut *tx)
    .await?;
    // Precondition re-check while holding the row lock: a failed one aborts
    // the transaction so a concurrent write that won the race is preserved.
    match precondition {
        PutPrecondition::MatchEtag(expected) => match &at_href {
            Some((_, current))
                if super::constant_time_eq_str(
                    expected.trim_matches('"'),
                    current.trim_matches('"'),
                ) => {}
            _ => {
                return Err(DbError::Conflict(
                    "etag precondition failed: resource changed concurrently".into(),
                ));
            }
        },
        PutPrecondition::NotExists if at_href.is_some() => {
            return Err(DbError::Conflict(
                "resource was created concurrently".into(),
            ));
        }
        PutPrecondition::None | PutPrecondition::NotExists => {}
    }
    let by_uid: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, deleted_at FROM events
         WHERE calendar_id = $1 AND uid = $2
           AND master_event_id IS NULL AND recurrence_id IS NULL AND recurrence_id_date IS NULL
         FOR UPDATE",
    )
    .bind(calendar_id)
    .bind(&master.uid)
    .fetch_optional(&mut *tx)
    .await?;
    let (existing, resurrect) = match (&at_href, by_uid) {
        (Some((id, _)), _) => (Some(*id), false),
        (None, Some((_, None))) => {
            return Err(DbError::Conflict(
                "event uid already exists at another resource".into(),
            ));
        }
        (None, Some((id, Some(_)))) => (Some(id), true), // same UID PUT again after a delete
        (None, None) => (None, false),
    };
    let created = existing.is_none() || resurrect;
    let mut row = match existing {
        Some(id) => {
            update_master(
                &mut tx,
                id,
                resurrect.then_some(href),
                master,
                master_location,
            )
            .await?
        }
        None => {
            insert_row(
                &mut tx,
                calendar_id,
                created_by,
                canonical.unwrap_or_else(Uuid::new_v4),
                canonical.is_none().then_some(href),
                None,
                master_location,
                master,
            )
            .await?
        }
    };
    sqlx::query("DELETE FROM events WHERE master_event_id = $1")
        .bind(row.id)
        .execute(&mut *tx)
        .await?;
    for (data, location_id) in overrides.iter().zip(override_locations) {
        insert_row(
            &mut tx,
            calendar_id,
            created_by,
            Uuid::new_v4(),
            None,
            Some(row.id),
            location_id,
            data,
        )
        .await?;
    }
    row.etag = super::event_etag(&row);
    sqlx::query("UPDATE events SET etag = $2 WHERE id = $1")
        .bind(row.id)
        .bind(&row.etag)
        .execute(&mut *tx)
        .await?;
    super::append_change(
        &mut tx,
        calendar_id,
        row.id,
        if created { "created" } else { "updated" },
    )
    .await?;
    tx.commit().await?;
    Ok((row, created))
}

#[allow(clippy::too_many_arguments)]
async fn insert_row(
    tx: &mut sqlx::PgConnection,
    calendar_id: Uuid,
    created_by: Uuid,
    id: Uuid,
    href: Option<&str>,
    master_id: Option<Uuid>,
    location_id: Option<Uuid>,
    data: &IcsEventUpsert,
) -> Result<EventRow, DbError> {
    let row = sqlx::query_as::<_, EventRow>(
        "INSERT INTO events (
            id, calendar_id, uid, master_event_id, recurrence_id, recurrence_id_date,
            starts_at, ends_at, start_date, end_date, duration, tzid, all_day,
            rrule, rdate, exdate,
            summary, description_html, description_text, url,
            status, priority, class, transp, categories, location_id,
            organizer_user_id, organizer_email, organizer_name, created_by, href, floating
         ) VALUES (
            $1, $2, $3, $4, $5, $6,
            $7, $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17, $18, $19,
            $20, $21, $22, $23, $24, $25,
            $26, $27, $28, $29, $30, $31, $32
         )
         RETURNING *",
    )
    .bind(id)
    .bind(calendar_id)
    .bind(&data.uid)
    .bind(master_id)
    .bind(data.recurrence_id)
    .bind(data.recurrence_id_date)
    .bind(data.starts_at)
    .bind(data.ends_at)
    .bind(data.start_date)
    .bind(data.end_date)
    .bind(interval(data.duration_secs))
    .bind(&data.tzid)
    .bind(data.all_day)
    // Overrides never recur (CHECK); a stray RRULE on one is ignored.
    .bind(data.rrule.as_ref().filter(|_| master_id.is_none()))
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
    .bind(data.organizer_user_id)
    .bind(&data.organizer_email)
    .bind(&data.organizer_name)
    .bind(created_by)
    .bind(href)
    .bind(data.floating)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("event uid/recurrence-id already exists".into())
        }
        other => other.into(),
    })?;
    write_attendees(tx, row.id, &data.attendees).await?;
    super::alarms::replace_alarms(tx, row.id, &data.alarms).await?;
    Ok(row)
}

/// Replaces the full content of an existing master (a PUT replaces the whole
/// resource, so an absent LOCATION clears it, unlike the partial-patch API).
/// `href` is Some only when a soft-deleted master is brought back.
async fn update_master(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    href: Option<&str>,
    data: &IcsEventUpsert,
    location_id: Option<Uuid>,
) -> Result<EventRow, DbError> {
    let row = sqlx::query_as::<_, EventRow>(
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
            floating = $26,
            href = COALESCE($27, href),
            deleted_at = NULL,
            updated_at = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(id)
    .bind(&data.uid)
    .bind(data.starts_at)
    .bind(data.ends_at)
    .bind(data.start_date)
    .bind(data.end_date)
    .bind(interval(data.duration_secs))
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
    .bind(&data.organizer_email)
    .bind(&data.organizer_name)
    .bind(data.sequence.unwrap_or(0))
    .bind(location_id)
    .bind(data.floating)
    .bind(href)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("event uid already exists".into())
        }
        other => other.into(),
    })?;
    sqlx::query("DELETE FROM event_attendees WHERE event_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    write_attendees(tx, id, &data.attendees).await?;
    super::alarms::replace_alarms(tx, id, &data.alarms).await?;
    Ok(row)
}

async fn write_attendees(
    tx: &mut sqlx::PgConnection,
    event_id: Uuid,
    attendees: &[IcsAttendee],
) -> Result<(), DbError> {
    for a in attendees {
        sqlx::query(
            "INSERT INTO event_attendees (id, event_id, email, telephone, display_name, role, partstat, rsvp)
             VALUES ($1, $2, $3, $4, $5, COALESCE($6, 'REQ-PARTICIPANT'), COALESCE($7, 'NEEDS-ACTION'), $8)",
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(&a.email)
        .bind(&a.telephone)
        .bind(&a.display_name)
        .bind(a.role.as_deref())
        .bind(a.partstat.as_deref())
        .bind(a.rsvp)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// DB-backed tests need a live PostgreSQL via DATABASE_URL (the throwaway
    /// instance the interop suite boots works). Without it they skip so
    /// `cargo test` still passes on machines without infrastructure.
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
            .bind(format!("{}@putseries.test", f.user))
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
        sqlx::query("INSERT INTO calendars (id, tenant_id, slug, name, created_by) VALUES ($1, $2, $3, $3, $4)")
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

    fn sample(uid: &str) -> IcsEventUpsert {
        IcsEventUpsert {
            uid: uid.to_string(),
            starts_at: Some(Utc::now()),
            ends_at: Some(Utc::now() + chrono::Duration::hours(1)),
            summary: Some("before".into()),
            organizer_email: "writer@putseries.test".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn stale_etag_put_aborts_and_matching_etag_put_succeeds() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let uid = Uuid::new_v4().to_string();
        let (first, created) = put_series(
            &pool,
            f.calendar,
            f.user,
            "a.ics",
            &sample(&uid),
            &[],
            &PutPrecondition::None,
        )
        .await
        .unwrap();
        assert!(created);

        // A competing unconditional PUT commits, as a concurrent client would
        // between dav-server's header check and our flush().
        let mut competing = sample(&uid);
        competing.summary = Some("concurrent".into());
        let (second, _) = put_series(
            &pool,
            f.calendar,
            f.user,
            "a.ics",
            &competing,
            &[],
            &PutPrecondition::None,
        )
        .await
        .unwrap();

        // A PUT still preconditioned on the first etag is stale: it must fail
        // and abort, leaving the competing write intact.
        let mut stale = sample(&uid);
        stale.summary = Some("stale".into());
        let err = put_series(
            &pool,
            f.calendar,
            f.user,
            "a.ics",
            &stale,
            &[],
            &PutPrecondition::MatchEtag(first.etag.clone()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DbError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
        let after = crate::get_event_by_href(&pool, f.calendar, "a.ics")
            .await
            .unwrap()
            .0;
        assert_eq!(after.summary, "concurrent");

        // A PUT preconditioned on the current etag succeeds and refreshes it.
        let mut fresh = sample(&uid);
        fresh.summary = Some("fresh".into());
        let (third, _) = put_series(
            &pool,
            f.calendar,
            f.user,
            "a.ics",
            &fresh,
            &[],
            &PutPrecondition::MatchEtag(second.etag.clone()),
        )
        .await
        .unwrap();
        assert_eq!(third.summary, "fresh");
        assert_ne!(third.etag, second.etag);
        // The unquoted form dav-server sends on the wire matches too.
        let mut again = sample(&uid);
        again.summary = Some("again".into());
        put_series(
            &pool,
            f.calendar,
            f.user,
            "a.ics",
            &again,
            &[],
            &PutPrecondition::MatchEtag(third.etag.trim_matches('"').to_string()),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn create_only_put_rejects_existing_resource() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let f = fixture(&pool).await;
        let uid = Uuid::new_v4().to_string();
        put_series(
            &pool,
            f.calendar,
            f.user,
            "b.ics",
            &sample(&uid),
            &[],
            &PutPrecondition::None,
        )
        .await
        .unwrap();
        // If-None-Match: * against a resource that now exists fails.
        let err = put_series(
            &pool,
            f.calendar,
            f.user,
            "b.ics",
            &sample(&uid),
            &[],
            &PutPrecondition::NotExists,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DbError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
        // Against an absent resource it creates.
        let other = Uuid::new_v4().to_string();
        let (row, created) = put_series(
            &pool,
            f.calendar,
            f.user,
            "c.ics",
            &sample(&other),
            &[],
            &PutPrecondition::NotExists,
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(row.uid, other);
    }
}
