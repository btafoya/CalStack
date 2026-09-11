//! Admin user management: list/create accounts and toggle is_admin/disabled.
//! Gated per-handler on is_admin, matching /api/audit (extras.rs).

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, patch},
};
use calendar_db::{self as db, UserRow};
use serde_json::json;
use uuid::Uuid;

fn admin_user_view(user: &UserRow) -> serde_json::Value {
    json!({
        "id": user.id, "username": user.username, "email": user.email,
        "display_name": user.display_name, "is_admin": user.is_admin,
        "disabled": user.disabled_at.is_some(), "created_at": user.created_at,
    })
}

async fn list_users(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    if !auth.user.is_admin {
        return Err(AppError::Forbidden);
    }
    let rows: Vec<UserRow> = sqlx::query_as("SELECT * FROM users ORDER BY created_at")
        .fetch_all(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter().map(admin_user_view).collect::<Vec<_>>()
    )))
}

#[derive(serde::Deserialize)]
struct CreateUserBody {
    username: String,
    email: String,
    password: String,
    display_name: Option<String>,
    is_admin: Option<bool>,
}

async fn create_user(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    if !auth.user.is_admin {
        return Err(AppError::Forbidden);
    }
    calendar_core::validate_username(&body.username)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    calendar_core::validate_email(&body.email).map_err(|e| AppError::bad_request(e.to_string()))?;
    if body.password.len() < 8 {
        return Err(AppError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    let hash = calendar_auth::hash_password(&body.password)
        .map_err(|e| AppError::internal(e.to_string()))?;
    let mut user = db::create_user(
        &pool,
        &body.username,
        &body.email,
        body.display_name.as_deref(),
        &hash,
    )
    .await
    .map_err(|e| match e {
        db::DbError::Conflict(msg) => AppError::bad_request(msg),
        other => other.into(),
    })?;
    if body.is_admin.unwrap_or(false) {
        sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
            .bind(user.id)
            .execute(&pool)
            .await
            .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
        user.is_admin = true;
    }
    Ok((
        axum::http::StatusCode::CREATED,
        Json(admin_user_view(&user)),
    ))
}

#[derive(serde::Deserialize)]
struct UpdateUserBody {
    is_admin: Option<bool>,
    disabled: Option<bool>,
}

async fn update_user(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    if !auth.user.is_admin {
        return Err(AppError::Forbidden);
    }
    if user_id == auth.user.id {
        return Err(AppError::bad_request(
            "cannot change your own admin/disabled status",
        ));
    }
    if let Some(is_admin) = body.is_admin {
        sqlx::query("UPDATE users SET is_admin = $1 WHERE id = $2")
            .bind(is_admin)
            .bind(user_id)
            .execute(&pool)
            .await
            .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    }
    if let Some(disabled) = body.disabled {
        sqlx::query(
            "UPDATE users SET disabled_at = CASE WHEN $1 THEN now() ELSE NULL END WHERE id = $2",
        )
        .bind(disabled)
        .bind(user_id)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    }
    let user = db::find_user_by_id(&pool, user_id).await?;
    Ok(Json(admin_user_view(&user)))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/admin/users", get(list_users).post(create_user))
        .route("/api/admin/users/{id}", patch(update_user))
}
