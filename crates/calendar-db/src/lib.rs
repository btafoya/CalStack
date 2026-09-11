//! PostgreSQL data layer: connection, migrations and repositories.

pub mod auth_ext;
pub mod ics_upsert;
pub mod jobs;
pub mod search;
pub mod sharing;

use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

/// Runs the embedded migrations (docs/PRD.md: default automatic migration).
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .map(|_| ())
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
}

// ============ users and tenancy ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub password_hash: Option<String>,
    pub is_admin: bool,
    pub timezone: Option<String>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a user with a password plus their personal tenant and membership.
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    display_name: Option<&str>,
    password_hash: &str,
) -> Result<UserRow, DbError> {
    let mut tx = pool.begin().await?;
    let user = sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (id, username, email, display_name, password_hash)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(email)
    .bind(display_name)
    .bind(password_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("username or email already exists".into())
        }
        other => other.into(),
    })?;

    let tenant_id = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $3, true)")
        .bind(tenant_id)
        .bind(format!("u-{}", user.id.as_simple()))
        .bind(display_name.unwrap_or(username))
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(tenant_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(user)
}

pub async fn find_user_by_email(pool: &PgPool, email: &str) -> Result<UserRow, DbError> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)
}

pub async fn find_user_by_username(pool: &PgPool, username: &str) -> Result<UserRow, DbError> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE username = $1")
        .bind(username)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)
}

pub async fn find_user_by_id(pool: &PgPool, id: Uuid) -> Result<UserRow, DbError> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)
}

pub async fn set_password(
    pool: &PgPool,
    user_id: Uuid,
    password_hash: &str,
) -> Result<(), DbError> {
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(user_id)
        .bind(password_hash)
        .execute(pool)
        .await?;
    Ok(())
}

// ============ calendars and ACLs ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CalendarRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
    pub order_index: i32,
    pub ctag: i64,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
pub struct NewCalendar {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
}

/// Creates a calendar with its ACL inside one transaction; the ACL set must
/// contain at least one owner (calendar-core::AclSet).
pub async fn create_calendar(
    pool: &PgPool,
    tenant_id: Uuid,
    new_calendar: &NewCalendar,
    created_by: Uuid,
    acl: &[(Uuid, calendar_core::CalendarCapability, bool)],
) -> Result<CalendarRow, DbError> {
    let mut tx = pool.begin().await?;
    let calendar = sqlx::query_as::<_, CalendarRow>(
        "INSERT INTO calendars (id, tenant_id, slug, name, description, color, timezone, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&new_calendar.slug)
    .bind(&new_calendar.name)
    .bind(new_calendar.description.as_deref())
    .bind(new_calendar.color.as_deref())
    .bind(new_calendar.timezone.as_deref())
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("calendar slug already exists".into())
        }
        other => other.into(),
    })?;
    for (principal, capability, manage) in acl {
        sqlx::query(
            "INSERT INTO calendar_acl (calendar_id, principal_user_id, capability, can_manage_acl)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(calendar.id)
        .bind(principal)
        .bind(capability.as_db_str())
        .bind(*manage)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(calendar)
}

/// Calendars the user holds any capability on, most privileged first.
pub async fn list_calendars_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<(CalendarRow, calendar_core::CalendarCapability)>, DbError> {
    #[derive(sqlx::FromRow)]
    struct AccessRow {
        #[sqlx(flatten)]
        calendar: CalendarRow,
        capability: String,
    }
    let rows = sqlx::query_as::<_, AccessRow>(
        "SELECT c.*, acl.capability
         FROM calendars c
         JOIN calendar_acl acl ON acl.calendar_id = c.id AND acl.principal_user_id = $1
         WHERE c.deleted_at IS NULL
         ORDER BY c.order_index, c.slug",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            calendar_core::CalendarCapability::from_db_str(&row.capability)
                .map(|c| (row.calendar, c))
                .ok_or_else(|| {
                    DbError::Sql(sqlx::Error::ColumnDecode {
                        index: "capability".into(),
                        source: "unknown capability".into(),
                    })
                })
        })
        .collect()
}

/// The user's capability on one calendar, ignoring soft-deleted calendars.
pub async fn calendar_capability(
    pool: &PgPool,
    calendar_id: Uuid,
    user_id: Uuid,
) -> Result<Option<calendar_core::CalendarCapability>, DbError> {
    let cap: Option<String> = sqlx::query_scalar(
        "SELECT acl.capability
         FROM calendar_acl acl
         JOIN calendars c ON c.id = acl.calendar_id AND c.deleted_at IS NULL
         WHERE acl.calendar_id = $1 AND acl.principal_user_id = $2",
    )
    .bind(calendar_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(cap.and_then(|c| calendar_core::CalendarCapability::from_db_str(&c)))
}

pub async fn get_calendar(pool: &PgPool, calendar_id: Uuid) -> Result<CalendarRow, DbError> {
    sqlx::query_as::<_, CalendarRow>("SELECT * FROM calendars WHERE id = $1 AND deleted_at IS NULL")
        .bind(calendar_id)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)
}

#[derive(Debug, Default)]
pub struct CalendarUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
    pub order_index: Option<i32>,
}

pub async fn update_calendar(
    pool: &PgPool,
    calendar_id: Uuid,
    changes: &CalendarUpdate,
) -> Result<CalendarRow, DbError> {
    sqlx::query_as::<_, CalendarRow>(
        "UPDATE calendars SET
            name = COALESCE($2, name),
            description = COALESCE($3, description),
            color = COALESCE($4, color),
            timezone = COALESCE($5, timezone),
            order_index = COALESCE($6, order_index),
            updated_at = now()
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING *",
    )
    .bind(calendar_id)
    .bind(changes.name.as_deref())
    .bind(changes.description.as_deref())
    .bind(changes.color.as_deref())
    .bind(changes.timezone.as_deref())
    .bind(changes.order_index)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// Soft delete (docs/PRD.md section 21): row stays for sync reporting.
pub async fn soft_delete_calendar(pool: &PgPool, calendar_id: Uuid) -> Result<(), DbError> {
    let n =
        sqlx::query("UPDATE calendars SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
            .bind(calendar_id)
            .execute(pool)
            .await?
            .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub async fn list_calendar_acl(
    pool: &PgPool,
    calendar_id: Uuid,
) -> Result<Vec<(Uuid, calendar_core::CalendarCapability, bool)>, DbError> {
    let rows = sqlx::query_as::<_, (Uuid, String, bool)>(
        "SELECT principal_user_id, capability, can_manage_acl FROM calendar_acl WHERE calendar_id = $1",
    )
    .bind(calendar_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(user, cap, manage)| {
            calendar_core::CalendarCapability::from_db_str(&cap)
                .map(|c| (user, c, manage))
                .ok_or_else(|| {
                    DbError::Sql(sqlx::Error::ColumnDecode {
                        index: "capability".into(),
                        source: "unknown capability".into(),
                    })
                })
        })
        .collect()
}

/// Replaces the whole ACL atomically (calendar-core::AclSet guarantees an owner).
pub async fn replace_calendar_acl(
    pool: &PgPool,
    calendar_id: Uuid,
    acl: &[(Uuid, calendar_core::CalendarCapability, bool)],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM calendar_acl WHERE calendar_id = $1")
        .bind(calendar_id)
        .execute(&mut *tx)
        .await?;
    for (principal, capability, manage) in acl {
        sqlx::query(
            "INSERT INTO calendar_acl (calendar_id, principal_user_id, capability, can_manage_acl)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(calendar_id)
        .bind(principal)
        .bind(capability.as_db_str())
        .bind(*manage)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The user's personal tenant id (signup creates exactly one).
pub async fn find_personal_tenant(pool: &PgPool, user_id: Uuid) -> Result<Uuid, DbError> {
    sqlx::query_scalar(
        "SELECT t.id FROM tenants t
         JOIN tenant_members m ON m.tenant_id = t.id AND m.user_id = $1
         WHERE t.is_personal",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

// ============ sessions ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: Vec<u8>,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &[u8],
    csrf_token: &str,
    ttl: Duration,
) -> Result<SessionRow, DbError> {
    sqlx::query_as::<_, SessionRow>(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(token_hash)
    .bind(csrf_token)
    .bind(Utc::now() + ttl)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Looks up a live session by token hash; bumps last_seen.
pub async fn find_live_session(pool: &PgPool, token_hash: &[u8]) -> Result<SessionRow, DbError> {
    let session = sqlx::query_as::<_, SessionRow>(
        "UPDATE sessions SET last_seen_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()
         RETURNING *",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok(session)
}

pub async fn revoke_session(pool: &PgPool, session_id: Uuid) -> Result<(), DbError> {
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ponytail: expired-session cleanup piggybacks on login; a sweeper job if volume ever demands it.
pub async fn delete_expired_sessions(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query(
        "DELETE FROM sessions WHERE expires_at < now() OR revoked_at < now() - interval '7 days'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ============ API tokens ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiTokenRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub token_hash: Vec<u8>,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub async fn create_api_token(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    token_hash: &[u8],
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> Result<ApiTokenRow, DbError> {
    sqlx::query_as::<_, ApiTokenRow>(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, scopes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(name)
    .bind(token_hash)
    .bind(scopes)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

pub async fn find_live_api_token(pool: &PgPool, token_hash: &[u8]) -> Result<ApiTokenRow, DbError> {
    sqlx::query_as::<_, ApiTokenRow>(
        "UPDATE api_tokens SET last_used_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())
         RETURNING *",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn list_api_tokens(pool: &PgPool, user_id: Uuid) -> Result<Vec<ApiTokenRow>, DbError> {
    sqlx::query_as::<_, ApiTokenRow>(
        "SELECT * FROM api_tokens WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn revoke_api_token(pool: &PgPool, user_id: Uuid, token_id: Uuid) -> Result<(), DbError> {
    let n = sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL")
        .bind(token_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

// ============ app passwords (CalDAV basic auth) ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppPasswordRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub password_hash: String,
    pub lookup_hash: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub async fn create_app_password(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    password_hash: &str,
    lookup_hash: &[u8],
) -> Result<AppPasswordRow, DbError> {
    sqlx::query_as::<_, AppPasswordRow>(
        "INSERT INTO app_passwords (id, user_id, name, password_hash, lookup_hash)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(name)
    .bind(password_hash)
    .bind(lookup_hash)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

pub async fn find_live_app_password(
    pool: &PgPool,
    lookup_hash: &[u8],
) -> Result<AppPasswordRow, DbError> {
    sqlx::query_as::<_, AppPasswordRow>(
        "UPDATE app_passwords SET last_used_at = now()
         WHERE lookup_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())
         RETURNING *",
    )
    .bind(lookup_hash)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn list_app_passwords(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<AppPasswordRow>, DbError> {
    sqlx::query_as::<_, AppPasswordRow>(
        "SELECT * FROM app_passwords WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn revoke_app_password(
    pool: &PgPool,
    user_id: Uuid,
    password_id: Uuid,
) -> Result<(), DbError> {
    let n = sqlx::query(
        "UPDATE app_passwords SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(password_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

// ============ events ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EventRow {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub uid: String,
    pub master_event_id: Option<Uuid>,
    pub recurrence_id: Option<chrono::NaiveDateTime>,
    pub recurrence_id_date: Option<chrono::NaiveDate>,
    pub is_exception: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub end_date: Option<chrono::NaiveDate>,
    pub duration: Option<sqlx::postgres::types::PgInterval>,
    pub tzid: Option<String>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub rdate: serde_json::Value,
    pub exdate: serde_json::Value,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub transp: Option<String>,
    pub categories: Vec<String>,
    pub location_id: Option<Uuid>,
    pub organizer_user_id: Option<Uuid>,
    pub organizer_email: String,
    pub organizer_name: Option<String>,
    pub sequence: i32,
    pub etag: String,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Writes one sync-visible change and bumps the calendar CTag; callers must
/// run this inside the same transaction as the resource mutation.
async fn append_change(
    tx: &mut sqlx::PgConnection,
    calendar_id: Uuid,
    resource_id: Uuid,
    operation: &str,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO change_log (calendar_id, resource_id, operation) VALUES ($1, $2, $3)")
        .bind(calendar_id)
        .bind(resource_id)
        .bind(operation)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE calendars SET ctag = ctag + 1 WHERE id = $1")
        .bind(calendar_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct NewEventData {
    pub master_id: Option<Uuid>,
    pub recurrence_id: Option<chrono::NaiveDateTime>,
    pub recurrence_id_date: Option<chrono::NaiveDate>,
    pub uid: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub end_date: Option<chrono::NaiveDate>,
    pub tzid: Option<String>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub rdate: Option<serde_json::Value>,
    pub exdate: Option<serde_json::Value>,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub transp: Option<String>,
    pub categories: Vec<String>,
    pub location_id: Option<Uuid>,
    pub organizer_user_id: Option<Uuid>,
    pub organizer_email: String,
    pub organizer_name: Option<String>,
}

/// ETag: strong validator derived from the mutation counter and row timestamp.
fn etag_for(calendar_id: Uuid, sequence: i32, updated_at: DateTime<Utc>) -> String {
    let digest = calendar_auth::sha256(
        format!("{calendar_id}-{sequence}-{}", updated_at.timestamp_millis()).as_bytes(),
    );
    format!("\"{}\"", calendar_auth::hex_encode(&digest[..8]))
}

/// Public ETag derivation for rows read outside the mutating functions.
pub fn event_etag(event: &EventRow) -> String {
    etag_for(event.calendar_id, event.sequence, event.updated_at)
}

/// Creates an event (or an exception when master_id is set) plus attendees,
/// and records the change atomically. Returns the row and its ETag.
pub async fn create_event(
    pool: &PgPool,
    calendar_id: Uuid,
    created_by: Uuid,
    attendees: &[NewAttendee],
    data: &NewEventData,
) -> Result<(EventRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let event = sqlx::query_as::<_, EventRow>(
        "INSERT INTO events (
            id, calendar_id, uid, master_event_id, recurrence_id, recurrence_id_date,
            starts_at, ends_at, start_date, end_date, tzid, all_day,
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
            $26, $27, $28, $29
         )
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(calendar_id)
    .bind(&data.uid)
    .bind(data.master_id)
    .bind(data.recurrence_id)
    .bind(data.recurrence_id_date)
    .bind(data.starts_at)
    .bind(data.ends_at)
    .bind(data.start_date)
    .bind(data.end_date)
    .bind(&data.tzid)
    .bind(data.all_day)
    .bind(&data.rrule)
    .bind(data.rdate.clone().unwrap_or(serde_json::json!([])))
    .bind(data.exdate.clone().unwrap_or(serde_json::json!([])))
    .bind(&data.summary)
    .bind(&data.description_html)
    .bind(&data.description_text)
    .bind(&data.url)
    .bind(&data.status)
    .bind(data.priority)
    .bind(&data.class)
    .bind(&data.transp)
    .bind(&data.categories)
    .bind(data.location_id)
    .bind(data.organizer_user_id)
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
    for a in attendees {
        sqlx::query(
            "INSERT INTO event_attendees
                (id, event_id, user_id, email, display_name, telephone, role, partstat, rsvp)
             VALUES ($1, $2, $3, $4, $5, $6,
                COALESCE($7, 'REQ-PARTICIPANT'), COALESCE($8, 'NEEDS-ACTION'), $9)",
        )
        .bind(Uuid::new_v4())
        .bind(event.id)
        .bind(a.user_id)
        .bind(&a.email)
        .bind(&a.display_name)
        .bind(&a.telephone)
        .bind(a.role.as_deref())
        .bind(a.partstat.as_deref())
        .bind(a.rsvp)
        .execute(&mut *tx)
        .await?;
    }
    let etag = etag_for(calendar_id, event.sequence, event.updated_at);
    sqlx::query("UPDATE events SET etag = $2 WHERE id = $1")
        .bind(event.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    append_change(&mut tx, calendar_id, event.id, "created").await?;
    tx.commit().await?;
    Ok((event, etag))
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct NewAttendee {
    pub user_id: Option<Uuid>,
    pub email: String,
    pub display_name: Option<String>,
    pub telephone: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    pub rsvp: Option<bool>,
}

#[derive(Debug, Default)]
pub struct EventPatch {
    pub summary: Option<String>,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub tzid: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub transp: Option<String>,
}

/// Updates an event guarded by its ETag. Returns (row, etag); DbError::NotFound
/// on missing row, DbError::Conflict on stale ETag.
pub async fn update_event(
    pool: &PgPool,
    event_id: Uuid,
    if_match: Option<&str>,
    patch: &EventPatch,
) -> Result<(EventRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, EventRow>(
        "SELECT * FROM events WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    let event = sqlx::query_as::<_, EventRow>(
        "UPDATE events SET
            summary = COALESCE($2, summary),
            description_html = COALESCE($3, description_html),
            description_text = COALESCE($4, description_text),
            url = COALESCE($5, url),
            starts_at = COALESCE($6, starts_at),
            ends_at = COALESCE($7, ends_at),
            tzid = COALESCE($8, tzid),
            status = COALESCE($9, status),
            priority = COALESCE($10, priority),
            class = COALESCE($11, class),
            transp = COALESCE($12, transp),
            sequence = sequence + 1,
            updated_at = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(event_id)
    .bind(&patch.summary)
    .bind(&patch.description_html)
    .bind(&patch.description_text)
    .bind(&patch.url)
    .bind(patch.starts_at)
    .bind(patch.ends_at)
    .bind(&patch.tzid)
    .bind(&patch.status)
    .bind(patch.priority)
    .bind(&patch.class)
    .bind(&patch.transp)
    .fetch_one(&mut *tx)
    .await?;
    let new_etag = etag_for(event.calendar_id, event.sequence, event.updated_at);
    sqlx::query("UPDATE events SET etag = $2 WHERE id = $1")
        .bind(event.id)
        .bind(&new_etag)
        .execute(&mut *tx)
        .await?;
    append_change(&mut tx, event.calendar_id, event.id, "updated").await?;
    tx.commit().await?;
    let etag = etag_for(event.calendar_id, event.sequence, event.updated_at);
    Ok((event, etag))
}

/// Soft delete guarded by ETag; records the deletion for sync clients.
pub async fn delete_event(
    pool: &PgPool,
    event_id: Uuid,
    if_match: Option<&str>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, EventRow>(
        "SELECT * FROM events WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    sqlx::query("UPDATE events SET deleted_at = now() WHERE id = $1")
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    append_change(&mut tx, current.calendar_id, current.id, "deleted").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_event(pool: &PgPool, event_id: Uuid) -> Result<(EventRow, String), DbError> {
    let event =
        sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE id = $1 AND deleted_at IS NULL")
            .bind(event_id)
            .fetch_optional(pool)
            .await?
            .ok_or(DbError::NotFound)?;
    let etag = etag_for(event.calendar_id, event.sequence, event.updated_at);
    Ok((event, etag))
}

/// Non-recurring events overlapping the window plus every recurring master
/// (expansion decides overlap; SQL cannot). Exceptions come separately via
/// list_exceptions.
/// Recurrence expansion into occurrences is the engine's job, not SQL's.
pub async fn list_events_in_range(
    pool: &PgPool,
    calendar_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<EventRow>, DbError> {
    sqlx::query_as::<_, EventRow>(
        "SELECT * FROM events
         WHERE calendar_id = $1 AND deleted_at IS NULL
           AND (
             (rrule IS NULL AND (
                (starts_at IS NOT NULL AND starts_at < $3 AND COALESCE(ends_at, starts_at) > $2)
                OR (start_date IS NOT NULL AND start_date::timestamp < ($3::timestamp AT TIME ZONE 'UTC')::date
                    AND COALESCE(end_date, start_date)::timestamp > ($2::timestamp AT TIME ZONE 'UTC')::date)
             ))
             OR rrule IS NOT NULL
           )",
    )
    .bind(calendar_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// All exception rows for the given masters (live calendars only).
pub async fn list_exceptions(pool: &PgPool, master_ids: &[Uuid]) -> Result<Vec<EventRow>, DbError> {
    if master_ids.is_empty() {
        return Ok(vec![]);
    }
    sqlx::query_as::<_, EventRow>(
        "SELECT e.* FROM events e
         JOIN calendars c ON c.id = e.calendar_id AND c.deleted_at IS NULL
         WHERE e.master_event_id = ANY($1) AND e.deleted_at IS NULL",
    )
    .bind(master_ids)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AttendeeRow {
    pub id: Uuid,
    pub event_id: Uuid,
    pub user_id: Option<Uuid>,
    pub email: String,
    pub display_name: Option<String>,
    pub telephone: Option<String>,
    pub role: String,
    pub partstat: String,
    pub rsvp: Option<bool>,
    pub schedule_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn list_attendees(pool: &PgPool, event_id: Uuid) -> Result<Vec<AttendeeRow>, DbError> {
    sqlx::query_as::<_, AttendeeRow>(
        "SELECT * FROM event_attendees WHERE event_id = $1 ORDER BY created_at",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub fn constant_time_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
