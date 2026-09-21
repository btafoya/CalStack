//! VJOURNAL storage. The small case (D9): modelled summary, first
//! DESCRIPTION, DTSTART, STATUS, CLASS, CATEGORIES, URL — one VJOURNAL per
//! resource, no overrides. Undated journals are notes.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, etag_for};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JournalRow {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub uid: String,
    pub href: Option<String>, // NULL = "{id}.ics"
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub tzid: Option<String>,
    /// Wall clock stored as if UTC; exported without Z or TZID.
    pub floating: bool,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    pub extra_props: serde_json::Value,
    pub sequence: i32,
    pub etag: String,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl JournalRow {
    /// The filename this journal is served under over CalDAV.
    pub fn resource_name(&self) -> String {
        self.href
            .clone()
            .unwrap_or_else(|| format!("{}.ics", self.id))
    }
}

#[derive(Debug, Default)]
pub struct NewJournalData {
    pub uid: String,
    pub href: Option<String>, // None = "{id}.ics" (API-created rows)
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub tzid: Option<String>,
    pub floating: bool,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    pub extra_props: Option<serde_json::Value>,
}

pub async fn create_journal(
    pool: &PgPool,
    calendar_id: Uuid,
    created_by: Uuid,
    data: &NewJournalData,
) -> Result<(JournalRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let id = Uuid::new_v4();
    if let Some(href) = &data.href
        && super::href_taken(&mut tx, calendar_id, href).await?
    {
        return Err(DbError::Conflict(format!(
            "href {href} already exists in this calendar"
        )));
    }
    let journal = sqlx::query_as::<_, JournalRow>(
        "INSERT INTO journals (
            id, calendar_id, uid, href,
            starts_at, start_date, tzid, floating,
            summary, description_html, description_text, url,
            status, class, categories, extra_props, created_by
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
         RETURNING *",
    )
    .bind(id)
    .bind(calendar_id)
    .bind(&data.uid)
    .bind(&data.href)
    .bind(data.starts_at)
    .bind(data.start_date)
    .bind(&data.tzid)
    .bind(data.floating)
    .bind(&data.summary)
    .bind(&data.description_html)
    .bind(&data.description_text)
    .bind(&data.url)
    .bind(&data.status)
    .bind(&data.class)
    .bind(&data.categories)
    .bind(data.extra_props.clone().unwrap_or(serde_json::json!([])))
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict("journal uid/href already exists".into())
        }
        other => other.into(),
    })?;
    let etag = etag_for(calendar_id, journal.sequence, journal.updated_at);
    sqlx::query("UPDATE journals SET etag = $2 WHERE id = $1")
        .bind(journal.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    super::record_change(&mut tx, calendar_id, journal.id, "journal", "created").await?;
    tx.commit().await?;
    Ok((journal, etag))
}

pub async fn get_journal(pool: &PgPool, journal_id: Uuid) -> Result<(JournalRow, String), DbError> {
    let journal = sqlx::query_as::<_, JournalRow>(
        "SELECT * FROM journals WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(journal_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let etag = etag_for(journal.calendar_id, journal.sequence, journal.updated_at);
    Ok((journal, etag))
}

/// The live journal served under `name` in a calendar.
pub async fn get_journal_by_href(
    pool: &PgPool,
    calendar_id: Uuid,
    name: &str,
) -> Result<(JournalRow, String), DbError> {
    let journal = sqlx::query_as::<_, JournalRow>(
        "SELECT * FROM journals
         WHERE calendar_id = $1 AND COALESCE(href, id::text || '.ics') = $2
           AND deleted_at IS NULL",
    )
    .bind(calendar_id)
    .bind(name)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let etag = etag_for(journal.calendar_id, journal.sequence, journal.updated_at);
    Ok((journal, etag))
}

#[derive(Debug, Default)]
pub struct JournalFilter {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Undated notes (no DTSTART at all).
    pub undated: bool,
    pub category: Option<String>,
}

/// Live journals of a calendar, newest first.
pub async fn list_journals(
    pool: &PgPool,
    calendar_id: Uuid,
    filter: &JournalFilter,
) -> Result<Vec<JournalRow>, DbError> {
    let mut sql =
        String::from("SELECT * FROM journals WHERE calendar_id = $1 AND deleted_at IS NULL");
    let mut n = 1u32;
    if filter.undated {
        n += 1;
        sql.push_str(&format!(
            " AND starts_at IS NULL AND start_date IS NULL AND ${n}"
        ));
    } else {
        if filter.from.is_some() {
            n += 1;
            sql.push_str(&format!(
                " AND COALESCE(starts_at, start_date::timestamp AT TIME ZONE 'UTC') >= ${n}"
            ));
        }
        if filter.to.is_some() {
            n += 1;
            sql.push_str(&format!(
                " AND COALESCE(starts_at, start_date::timestamp AT TIME ZONE 'UTC') < ${n}"
            ));
        }
    }
    if filter.category.is_some() {
        n += 1;
        sql.push_str(&format!(" AND ${n} = ANY(categories)"));
    }
    sql.push_str(" ORDER BY COALESCE(starts_at, start_date::timestamp AT TIME ZONE 'UTC') DESC NULLS LAST, created_at DESC");
    let mut q = sqlx::query_as::<_, JournalRow>(&sql).bind(calendar_id);
    if filter.undated {
        q = q.bind(true);
    }
    if let Some(from) = filter.from {
        q = q.bind(from);
    }
    if let Some(to) = filter.to {
        q = q.bind(to);
    }
    if let Some(category) = &filter.category {
        q = q.bind(category);
    }
    q.fetch_all(pool).await.map_err(Into::into)
}

#[derive(Debug, Default)]
pub struct JournalPatch {
    pub summary: Option<String>,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<chrono::NaiveDate>,
    pub tzid: Option<String>,
    pub floating: Option<bool>,
    pub status: Option<String>,
    pub class: Option<String>,
    /// Some(_) replaces the category list; None leaves it untouched.
    pub categories: Option<Vec<String>>,
}

/// Updates a journal guarded by its ETag.
pub async fn update_journal(
    pool: &PgPool,
    journal_id: Uuid,
    if_match: Option<&str>,
    patch: &JournalPatch,
) -> Result<(JournalRow, String), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, JournalRow>(
        "SELECT * FROM journals WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(journal_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !super::constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    // starts_at/start_date are mutually exclusive (CHECK constraint).
    let (starts_at, start_date) = if patch.start_date.is_some() {
        (None, patch.start_date)
    } else if patch.starts_at.is_some() {
        (patch.starts_at, None)
    } else {
        (current.starts_at, current.start_date)
    };
    let journal = sqlx::query_as::<_, JournalRow>(
        "UPDATE journals SET
            summary = COALESCE($2, summary),
            description_html = COALESCE($3, description_html),
            description_text = COALESCE($4, description_text),
            url = COALESCE($5, url),
            starts_at = $6,
            start_date = $7,
            tzid = COALESCE($8, tzid),
            floating = COALESCE($9, floating),
            status = COALESCE($10, status),
            class = COALESCE($11, class),
            categories = COALESCE($12, categories),
            sequence = sequence + 1,
            updated_at = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(journal_id)
    .bind(&patch.summary)
    .bind(&patch.description_html)
    .bind(&patch.description_text)
    .bind(&patch.url)
    .bind(starts_at)
    .bind(start_date)
    .bind(&patch.tzid)
    .bind(patch.floating)
    .bind(&patch.status)
    .bind(&patch.class)
    .bind(&patch.categories)
    .fetch_one(&mut *tx)
    .await?;
    let etag = etag_for(journal.calendar_id, journal.sequence, journal.updated_at);
    sqlx::query("UPDATE journals SET etag = $2 WHERE id = $1")
        .bind(journal.id)
        .bind(&etag)
        .execute(&mut *tx)
        .await?;
    super::record_change(
        &mut tx,
        journal.calendar_id,
        journal.id,
        "journal",
        "updated",
    )
    .await?;
    tx.commit().await?;
    Ok((journal, etag))
}

/// Soft delete guarded by ETag; records the deletion for sync clients.
pub async fn delete_journal(
    pool: &PgPool,
    journal_id: Uuid,
    if_match: Option<&str>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as::<_, JournalRow>(
        "SELECT * FROM journals WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(journal_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(expected) = if_match
        && !super::constant_time_eq_str(expected.trim_matches('"'), current.etag.trim_matches('"'))
    {
        return Err(DbError::Conflict("etag mismatch".into()));
    }
    sqlx::query("UPDATE journals SET deleted_at = now() WHERE id = $1")
        .bind(journal_id)
        .execute(&mut *tx)
        .await?;
    super::record_change(
        &mut tx,
        current.calendar_id,
        current.id,
        "journal",
        "deleted",
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
