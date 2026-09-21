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
use uuid::Uuid;

#[derive(serde::Serialize, utoipa::ToSchema)]
struct AdminUserView {
    id: Uuid,
    username: String,
    email: String,
    display_name: Option<String>,
    is_admin: bool,
    disabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn admin_user_view(user: &UserRow) -> AdminUserView {
    AdminUserView {
        id: user.id,
        username: user.username.clone(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        is_admin: user.is_admin,
        disabled: user.disabled_at.is_some(),
        created_at: user.created_at,
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/users",
    responses(
        (status = 200, description = "all accounts", body = Vec<AdminUserView>),
        (status = 403, description = "not admin"),
    )
)]
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
    Ok(Json(rows.iter().map(admin_user_view).collect::<Vec<_>>()))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct CreateUserBody {
    username: String,
    email: String,
    password: String,
    display_name: Option<String>,
    is_admin: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/admin/users",
    request_body = CreateUserBody,
    responses(
        (status = 201, description = "created", body = AdminUserView),
        (status = 400, description = "validation error"),
        (status = 403, description = "not admin"),
    )
)]
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

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct UpdateUserBody {
    is_admin: Option<bool>,
    disabled: Option<bool>,
}

#[utoipa::path(
    patch,
    path = "/api/admin/users/{id}",
    params(("id" = Uuid, Path, description = "user id (cannot be your own)")),
    request_body = UpdateUserBody,
    responses(
        (status = 200, description = "updated", body = AdminUserView),
        (status = 400, description = "cannot change your own admin/disabled status"),
        (status = 403, description = "not admin"),
        (status = 404, description = "absent"),
    )
)]
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

/// OpenAPI for the admin module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(list_users, create_user, update_user),
    components(schemas(CreateUserBody, UpdateUserBody, AdminUserView))
)]
pub(crate) struct AdminApi;
