//! Category registry (docs/DESIGN-category-registry.md): per-tenant display
//! metadata for event category strings. Calendar-scoped rows are managed by
//! calendar owners; tenant-wide rows by admins. Events keep their freeform
//! strings — deleting a registry row never touches event data.

use crate::{AppError, AppState, require_admin, require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{patch, post},
};
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use serde_json::json;
use uuid::Uuid;

fn validate_category_body(slug: &str, name: &str, color: &str) -> Result<(), AppError> {
    calendar_core::validate_slug(slug).map_err(|e| AppError::bad_request(e.to_string()))?;
    calendar_core::validate_category_color(color)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    if name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    Ok(())
}

/// Auth for a registry row: Owner on its calendar, admin for tenant-wide.
async fn require_row_manager(
    pool: &sqlx::PgPool,
    row: &db::categories::CategoryRow,
    user_id: Uuid,
    is_admin: bool,
) -> Result<(), AppError> {
    match row.calendar_id {
        Some(calendar_id) => {
            require_capability(pool, calendar_id, user_id, CalendarCapability::Owner).await?;
        }
        None if !is_admin => return Err(AppError::Forbidden),
        None => {}
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct CategoryBody {
    /// Calendar this category applies to; omit/null for tenant-wide (admin only).
    calendar_id: Option<Uuid>,
    slug: String,
    name: String,
    color: String,
    sort_order: Option<i32>,
}

async fn create_category(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CategoryBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    validate_category_body(&body.slug, &body.name, &body.color)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    if let Some(calendar_id) = body.calendar_id {
        require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    } else {
        require_admin(&auth)?;
    }
    let row = db::categories::create(
        &pool,
        tenant_id,
        &db::categories::NewCategory {
            calendar_id: body.calendar_id,
            slug: body.slug,
            name: body.name,
            color: body.color,
            sort_order: body.sort_order.unwrap_or(0),
            created_by: Some(auth.user.id),
        },
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "id": row.id, "tenant_id": row.tenant_id, "calendar_id": row.calendar_id,
            "slug": row.slug, "name": row.name, "color": row.color, "sort_order": row.sort_order,
        })),
    ))
}

#[derive(serde::Deserialize)]
struct CategoriesQuery {
    calendar_id: Option<Uuid>,
}

async fn list_categories(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CategoriesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let rows =
        db::categories::list_visible(&pool, tenant_id, auth.user.id, query.calendar_id).await?;
    Ok(Json(json!(rows)))
}

#[derive(serde::Deserialize)]
struct CategoryPatchBody {
    slug: Option<String>,
    name: Option<String>,
    color: Option<String>,
    sort_order: Option<i32>,
}

async fn patch_category(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<CategoryPatchBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let row = db::categories::get(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    require_row_manager(&pool, &row, auth.user.id, auth.user.is_admin).await?;
    if let Some(color) = &body.color {
        calendar_core::validate_category_color(color)
            .map_err(|e| AppError::bad_request(e.to_string()))?;
    }
    if let Some(slug) = &body.slug {
        calendar_core::validate_slug(slug).map_err(|e| AppError::bad_request(e.to_string()))?;
    }
    let row = db::categories::update(
        &pool,
        id,
        &db::categories::CategoryUpdate {
            slug: body.slug,
            name: body.name,
            color: body.color,
            sort_order: body.sort_order,
        },
    )
    .await?;
    Ok(Json(json!(row)))
}

async fn delete_category(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let row = db::categories::get(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    require_row_manager(&pool, &row, auth.user.id, auth.user.is_admin).await?;
    db::categories::delete(&pool, id).await?;
    Ok(Json(json!({"ok": true})))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/categories",
            post(create_category).get(list_categories),
        )
        .route(
            "/api/categories/{id}",
            patch(patch_category).delete(delete_category),
        )
}
