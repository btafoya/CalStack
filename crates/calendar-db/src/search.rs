//! Search (docs/PRD.md section 12): PostgreSQL-native over summary, plain
//! description, attendees, location fields, categories and metadata.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, EventRow};

/// Free-text search across every calendar the user can read (owner,
/// read_write, read_only; free_busy-only access yields nothing). Text hits on
/// the generated tsvector; attendees and locations match by substring.
pub struct SearchQuery {
    pub text: String,
    pub attendee: Option<String>,
    pub limit: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchHit {
    pub event: EventRow,
    pub rank: f32,
}

pub async fn search_events(
    pool: &PgPool,
    user_id: Uuid,
    query: &SearchQuery,
) -> Result<Vec<SearchHit>, DbError> {
    let limit = if query.limit <= 0 { 50 } else { query.limit };
    let pattern = format!("%{}%", query.attendee.as_deref().unwrap_or_default());
    #[derive(sqlx::FromRow)]
    struct Raw {
        #[sqlx(flatten)]
        event: EventRow,
        rank: f32,
    }
    let rows = sqlx::query_as::<_, Raw>(
        "SELECT e.*, ts_rank(e.search_vector, websearch_to_tsquery('simple', $2)) AS rank
         FROM events e
         JOIN calendars c ON c.id = e.calendar_id AND c.deleted_at IS NULL
         WHERE c.deleted_at IS NULL AND e.deleted_at IS NULL
           AND EXISTS (
             SELECT 1 FROM calendar_acl acl
             WHERE acl.calendar_id = e.calendar_id AND acl.principal_user_id = $1
               AND acl.capability IN ('owner', 'read_write', 'read_only')
           )
           AND (
             (e.search_vector @@ websearch_to_tsquery('simple', $2) AND $2 <> '')
             OR EXISTS (SELECT 1 FROM event_attendees a
                        WHERE a.event_id = e.id AND (a.email ILIKE $3 OR a.display_name ILIKE $3) AND $4)
             OR EXISTS (SELECT 1 FROM locations l
                        WHERE l.id = e.location_id AND (l.display_name ILIKE $3 OR l.formatted_address ILIKE $3) AND $4)
             OR $5 = ANY(e.categories)
           )
         ORDER BY rank DESC, e.starts_at
         LIMIT $6",
    )
    .bind(user_id)
    .bind(&query.text)
    .bind(&pattern)
    .bind(query.attendee.is_some())
    .bind(query.attendee.clone().unwrap_or_default())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            event: r.event,
            rank: r.rank,
        })
        .collect())
}

/// Recent change entries across readable calendars — the application change
/// stream (docs/PRD.md section 11), surfaced over OpenAPI.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChangeRow {
    pub seq: i64,
    pub calendar_id: Uuid,
    pub resource_id: Uuid,
    pub operation: String,
    pub changed_at: DateTime<Utc>,
}

/// Changes after a sync-token across every calendar the user can read.
pub async fn list_changes_since(
    pool: &PgPool,
    user_id: Uuid,
    since_seq: i64,
    limit: i64,
) -> Result<Vec<ChangeRow>, DbError> {
    sqlx::query_as::<_, ChangeRow>(
        "SELECT cl.seq, cl.calendar_id, cl.resource_id, cl.operation, cl.changed_at
         FROM change_log cl
         JOIN calendar_acl acl ON acl.calendar_id = cl.calendar_id
             AND acl.principal_user_id = $1
             AND acl.capability IN ('owner', 'read_write', 'read_only')
         WHERE cl.seq > $2
         ORDER BY cl.seq
         LIMIT $3",
    )
    .bind(user_id)
    .bind(since_seq)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}
