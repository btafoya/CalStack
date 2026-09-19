//! Web Push subscriptions (docs/PRD.md section 16): any signed-in user may
//! register a browser subscription; it stays dormant until a webpush
//! notification provider exists and an alarm selects the push channel.

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use calendar_db::{self as db};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct PushBody {
    endpoint: String,
    keys: PushKeys,
}

#[derive(serde::Deserialize)]
struct PushKeys {
    p256dh: String,
    auth: String,
}

async fn subscribe(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PushBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    if body.endpoint.is_empty() || body.keys.p256dh.is_empty() || body.keys.auth.is_empty() {
        return Err(AppError::BadRequest("endpoint and keys required".into()));
    }
    sqlx::query(
        "INSERT INTO push_subscriptions (id, user_id, endpoint, p256dh, auth, user_agent)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (user_id, endpoint) DO UPDATE SET
            p256dh = $3, auth = $5, created_at = now()",
    )
    .bind(Uuid::new_v4())
    .bind(auth.user.id)
    .bind(&body.endpoint)
    .bind(&body.keys.p256dh)
    .bind(&body.keys.auth)
    .bind(headers.get("user-agent").and_then(|v| v.to_str().ok()))
    .execute(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

async fn unsubscribe(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PushBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    sqlx::query("DELETE FROM push_subscriptions WHERE user_id = $1 AND endpoint = $2")
        .bind(auth.user.id)
        .bind(&body.endpoint)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

/// The tenant webpush provider's VAPID public key (subscribe applicationServerKey).
async fn public_key(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let Some(crypto) = crypto.as_ref() else {
        return Ok(Json(json!(null)));
    };
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        config_encrypted: Vec<u8>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT config_encrypted FROM notification_providers
         WHERE tenant_id = $1 AND enabled AND kind = 'webpush' LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    let Some(row) = row else {
        return Ok(Json(json!(null)));
    };
    let config: Value = serde_json::from_slice(
        &crypto
            .decrypt(&row.config_encrypted)
            .map_err(|e| AppError::internal(e.to_string()))?,
    )
    .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!(config.get("vapid_public"))))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/push/subscriptions",
            post(subscribe).delete(unsubscribe),
        )
        .route("/api/push/public-key", get(public_key))
}
