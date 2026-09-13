//! Public shares (ADR-004) and inbound subscriptions.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, EventRow};

// ============ public shares ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShareRow {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub event_id: Option<Uuid>,
    pub token_hash: Vec<u8>,
    pub allows_caldav: bool,
    pub created_by: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ShareRow {
    pub fn is_live(&self) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|e| e > Utc::now())
    }
}

pub async fn create_share(
    pool: &PgPool,
    calendar_id: Uuid,
    event_id: Option<Uuid>,
    token_hash: &[u8],
    allows_caldav: bool,
    created_by: Uuid,
    expires_at: Option<DateTime<Utc>>,
) -> Result<ShareRow, DbError> {
    sqlx::query_as::<_, ShareRow>(
        "INSERT INTO public_shares (id, calendar_id, event_id, token_hash, allows_caldav, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(calendar_id)
    .bind(event_id)
    .bind(token_hash)
    .bind(allows_caldav)
    .bind(created_by)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Live (unexpired, unrevoked) share by token hash; bumps last_accessed.
pub async fn find_live_share(pool: &PgPool, token_hash: &[u8]) -> Result<ShareRow, DbError> {
    let share = sqlx::query_as::<_, ShareRow>(
        "UPDATE public_shares SET last_accessed_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())
         RETURNING *",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok(share)
}

pub async fn list_shares_for_calendar(
    pool: &PgPool,
    calendar_id: Uuid,
) -> Result<Vec<ShareRow>, DbError> {
    sqlx::query_as::<_, ShareRow>(
        "SELECT * FROM public_shares WHERE calendar_id = $1 AND revoked_at IS NULL
         ORDER BY created_at",
    )
    .bind(calendar_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn revoke_share(pool: &PgPool, calendar_id: Uuid, share_id: Uuid) -> Result<(), DbError> {
    let n = sqlx::query(
        "UPDATE public_shares SET revoked_at = now() WHERE id = $1 AND calendar_id = $2 AND revoked_at IS NULL",
    )
    .bind(share_id)
    .bind(calendar_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

// ============ subscriptions (user follows someone else's public share) ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubscriptionRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub share_id: Uuid,
    pub color: Option<String>,
    pub order_index: i32,
    pub created_at: DateTime<Utc>,
}

/// A subscription plus the underlying share and calendar, for listing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubscriptionView {
    pub id: Uuid,
    pub color: Option<String>,
    pub allows_caldav: bool,
    pub calendar_name: String,
    pub calendar_slug: String,
    /// False once the owner revokes/expires the share or deletes the
    /// calendar; row is kept (not auto-deleted) so the UI can notice it.
    pub live: bool,
}

pub async fn create_subscription(
    pool: &PgPool,
    user_id: Uuid,
    share_id: Uuid,
    color: Option<String>,
) -> Result<SubscriptionRow, DbError> {
    sqlx::query_as::<_, SubscriptionRow>(
        "INSERT INTO subscriptions (id, user_id, share_id, color) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, share_id) DO UPDATE SET color = COALESCE($4, subscriptions.color)
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(share_id)
    .bind(color)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

pub async fn delete_subscription(
    pool: &PgPool,
    user_id: Uuid,
    subscription_id: Uuid,
) -> Result<(), DbError> {
    let n = sqlx::query("DELETE FROM subscriptions WHERE id = $1 AND user_id = $2")
        .bind(subscription_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// The calendar behind a live subscription, checked against subscription id
/// and user id together. NotFound covers "doesn't exist", "not yours", and
/// "dead share" alike — the caller doesn't get to distinguish those cases.
pub async fn subscribed_calendar_id(
    pool: &PgPool,
    user_id: Uuid,
    subscription_id: Uuid,
) -> Result<Uuid, DbError> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT sh.calendar_id
         FROM subscriptions s
         JOIN public_shares sh ON sh.id = s.share_id AND sh.revoked_at IS NULL
             AND (sh.expires_at IS NULL OR sh.expires_at > now())
         JOIN calendars c ON c.id = sh.calendar_id AND c.deleted_at IS NULL
         WHERE s.id = $1 AND s.user_id = $2",
    )
    .bind(subscription_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// All subscriptions for a user, including ones whose share was since
/// revoked/expired or whose calendar was deleted — `live` tells the caller
/// which; the row stays until the user dismisses it (`delete_subscription`)
/// so the UI can show a removal notice instead of the row just vanishing.
///
/// Columns are selected explicitly (not `s.*, sh.*, c.*`): subscriptions,
/// public_shares, and calendars each have an `id` and `created_at` column,
/// and sqlx resolves same-named columns by last occurrence, so a wildcard
/// join here silently returns the calendar's id for every row instead of
/// the subscription's.
pub async fn list_subscriptions(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<SubscriptionView>, DbError> {
    sqlx::query_as::<_, SubscriptionView>(
        "SELECT s.id, s.color,
                COALESCE(sh.allows_caldav, false) AS allows_caldav,
                COALESCE(c.name, 'Removed calendar') AS calendar_name,
                COALESCE(c.slug, '') AS calendar_slug,
                (sh.id IS NOT NULL AND sh.revoked_at IS NULL
                     AND (sh.expires_at IS NULL OR sh.expires_at > now())
                     AND c.id IS NOT NULL AND c.deleted_at IS NULL) AS live
         FROM subscriptions s
         LEFT JOIN public_shares sh ON sh.id = s.share_id
         LEFT JOIN calendars c ON c.id = sh.calendar_id
         WHERE s.user_id = $1
         ORDER BY s.order_index, s.created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

// ============ public feed content ============

/// Events of a calendar visible in a public feed: PRIVATE and CONFIDENTIAL
/// events are withheld (privacy rule, docs/PRD.md section 5).
pub async fn list_public_events(
    pool: &PgPool,
    calendar_id: Uuid,
) -> Result<Vec<EventRow>, DbError> {
    sqlx::query_as::<_, EventRow>(
        "SELECT * FROM events
         WHERE calendar_id = $1 AND deleted_at IS NULL
           AND (class IS NULL OR class = 'PUBLIC')",
    )
    .bind(calendar_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}
