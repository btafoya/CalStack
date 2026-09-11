//! Capped bytea attachments (ADR-010): metadata + bytes in PostgreSQL, the
//! size cap is env-configurable and enforced on write.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, EventRow};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AttachmentRow {
    pub id: Uuid,
    pub event_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub byte_size: i64,
    pub sha256: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

pub async fn create_attachment(
    pool: &PgPool,
    event_id: Uuid,
    filename: &str,
    content_type: &str,
    data: &[u8],
    checksum: &[u8],
) -> Result<AttachmentRow, DbError> {
    sqlx::query_as::<_, AttachmentRow>(
        "INSERT INTO attachments (id, event_id, filename, content_type, byte_size, sha256, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, event_id, filename, content_type, byte_size, sha256, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(event_id)
    .bind(filename)
    .bind(content_type)
    .bind(data.len() as i64)
    .bind(checksum)
    .bind(data)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Metadata only (no bytes).
pub async fn get_attachment_meta(
    pool: &PgPool,
    attachment_id: Uuid,
) -> Result<AttachmentRow, DbError> {
    sqlx::query_as::<_, AttachmentRow>(
        "SELECT id, event_id, filename, content_type, byte_size, sha256, created_at
         FROM attachments WHERE id = $1",
    )
    .bind(attachment_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// (meta, bytes).
pub async fn get_attachment(
    pool: &PgPool,
    attachment_id: Uuid,
) -> Result<(AttachmentRow, Vec<u8>), DbError> {
    #[derive(sqlx::FromRow)]
    struct Full {
        #[sqlx(flatten)]
        meta: AttachmentRow,
        data: Vec<u8>,
    }
    let row = sqlx::query_as::<_, Full>(
        "SELECT id, event_id, filename, content_type, byte_size, sha256, created_at, data
         FROM attachments WHERE id = $1",
    )
    .bind(attachment_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok((row.meta, row.data))
}

pub async fn list_attachments(
    pool: &PgPool,
    event_id: Uuid,
) -> Result<Vec<AttachmentRow>, DbError> {
    sqlx::query_as::<_, AttachmentRow>(
        "SELECT id, event_id, filename, content_type, byte_size, sha256, created_at
         FROM attachments WHERE event_id = $1 ORDER BY created_at",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn delete_attachment(pool: &PgPool, attachment_id: Uuid) -> Result<(), DbError> {
    let n = sqlx::query("DELETE FROM attachments WHERE id = $1")
        .bind(attachment_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// The event an attachment belongs to (for the ACL guard).
pub async fn event_of_attachment(pool: &PgPool, attachment_id: Uuid) -> Result<EventRow, DbError> {
    let event_id: Option<Uuid> =
        sqlx::query_scalar("SELECT event_id FROM attachments WHERE id = $1")
            .bind(attachment_id)
            .fetch_optional(pool)
            .await?;
    match event_id {
        Some(event_id) => {
            let (event, _) = super::get_event(pool, event_id).await?;
            Ok(event)
        }
        None => Err(DbError::NotFound),
    }
}
