//! Category registry: display metadata attached to event category strings by
//! slug match. Events keep their freeform `text[]`; nothing here constrains
//! what clients send (hybrid model — see docs/DESIGN-category-registry.md).
use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::DbError;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CategoryRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub calendar_id: Option<Uuid>,
    pub slug: String,
    pub name: String,
    pub color: String,
    pub sort_order: i32,
}

#[derive(Debug, Default, Clone)]
pub struct NewCategory {
    pub calendar_id: Option<Uuid>,
    pub slug: String,
    pub name: String,
    pub color: String,
    pub sort_order: i32,
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Default, Clone)]
pub struct CategoryUpdate {
    pub slug: Option<String>,
    pub name: Option<String>,
    pub color: Option<String>,
    pub sort_order: Option<i32>,
}

/// slug -> display info for events in one calendar: that calendar's rows
/// shadowing tenant-wide ones. One query per request; never per event.
pub type CategoryRegistry = HashMap<String, CategoryInfo>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CategoryInfo {
    pub name: String,
    pub color: String,
}

/// Registry for events in `calendar_id`. Calendar-scoped rows are read last so
/// they overwrite tenant-wide rows sharing a slug (calendar wins).
pub async fn registry_for_calendar(
    pool: &PgPool,
    calendar_id: Uuid,
) -> Result<CategoryRegistry, DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        slug: String,
        name: String,
        color: String,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.slug, c.name, c.color FROM categories c
         JOIN calendars cal ON cal.tenant_id = c.tenant_id
         WHERE cal.id = $1 AND (c.calendar_id = $1 OR c.calendar_id IS NULL)
         ORDER BY c.calendar_id NULLS FIRST",
    )
    .bind(calendar_id)
    .fetch_all(pool)
    .await?;
    // ponytail: HashMap insert-overwrite gives calendar-wins shadowing; switch
    // to ordered Vec if display order of merged scopes ever matters.
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.slug,
                CategoryInfo {
                    name: r.name,
                    color: r.color,
                },
            )
        })
        .collect())
}

/// Tenant-wide rows plus rows on calendars the user has any ACL entry for.
pub async fn list_visible(
    pool: &PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
    calendar_id: Option<Uuid>,
) -> Result<Vec<CategoryRow>, DbError> {
    sqlx::query_as::<_, CategoryRow>(
        "SELECT c.id, c.tenant_id, c.calendar_id, c.slug, c.name, c.color, c.sort_order
         FROM categories c
         LEFT JOIN calendar_acl a ON a.calendar_id = c.calendar_id AND a.principal_user_id = $2
         WHERE c.tenant_id = $1 AND (c.calendar_id IS NULL OR a.principal_user_id IS NOT NULL)
           AND ($3::uuid IS NULL OR c.calendar_id = $3 OR c.calendar_id IS NULL)
         ORDER BY c.sort_order, c.slug",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(calendar_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<CategoryRow, DbError> {
    sqlx::query_as(
        "SELECT id, tenant_id, calendar_id, slug, name, color, sort_order
         FROM categories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn create(
    pool: &PgPool,
    tenant_id: Uuid,
    new: &NewCategory,
) -> Result<CategoryRow, DbError> {
    sqlx::query_as(
        "INSERT INTO categories (id, tenant_id, calendar_id, slug, name, color, sort_order, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, tenant_id, calendar_id, slug, name, color, sort_order",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(new.calendar_id)
    .bind(&new.slug)
    .bind(&new.name)
    .bind(&new.color)
    .bind(new.sort_order)
    .bind(new.created_by)
    .fetch_one(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict(format!("category slug '{}' already in scope", new.slug))
        }
        _ => DbError::Sql(e),
    })
}

/// Applies field updates and, when the slug changes, rewrites event category
/// strings in the same transaction so no events orphan. The rewrite follows
/// the row's scope: calendar rows touch that calendar, tenant-wide rows touch
/// the whole tenant.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    changes: &CategoryUpdate,
) -> Result<CategoryRow, DbError> {
    let row = get(pool, id).await?;
    let old_slug = row.slug.clone();
    let new_slug = changes.slug.clone().unwrap_or_else(|| row.slug.clone());
    let new_name = changes.name.clone().unwrap_or_else(|| row.name.clone());
    let new_color = changes.color.clone().unwrap_or_else(|| row.color.clone());
    let new_sort = changes.sort_order.unwrap_or(row.sort_order);

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE categories SET slug = $2, name = $3, color = $4, sort_order = $5, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(&new_slug)
        .bind(&new_name)
        .bind(&new_color)
        .bind(new_sort)
        .execute(&mut *tx)
        .await?;
    if new_slug != old_slug {
        match row.calendar_id {
            Some(calendar_id) => {
                sqlx::query("UPDATE events SET categories = array_replace(categories, $1, $2) WHERE calendar_id = $3 AND $1 = ANY(categories)")
                    .bind(&old_slug)
                    .bind(&new_slug)
                    .bind(calendar_id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE events SET categories = array_replace(categories, $1, $2)
                     WHERE calendar_id IN (SELECT id FROM calendars WHERE tenant_id = $3)
                       AND $1 = ANY(categories)",
                )
                .bind(&old_slug)
                .bind(&new_slug)
                .bind(row.tenant_id)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    get(pool, id).await
}

/// Removes the registry row only — event strings keep working as freeform text.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<(), DbError> {
    let n = sqlx::query("DELETE FROM categories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}
