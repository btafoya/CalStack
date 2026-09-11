//! Rules (ADR-007) and notification providers (docs/PRD.md section 16):
//! simple trigger → optional conditions → actions engine, executed inline on
//! event mutations, with executions recorded for the audit trail.

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{delete, post},
};
use calendar_db::{self as db};
use serde_json::{Value, json};
use uuid::Uuid;

// ============ rules CRUD ============

#[derive(serde::Deserialize)]
struct RuleBody {
    name: String,
    enabled: Option<bool>,
    trigger_type: String,
    conditions: Option<Value>,
    actions: Option<Value>,
}

/// Evaluates rules for one trigger. Called from event mutations; failures are
/// logged, never propagated (rules must not break the mutation).
pub(crate) async fn run_rules(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    trigger_type: &str,
    subject_id: Uuid,
    context: Value,
) {
    #[derive(sqlx::FromRow)]
    struct RuleRow {
        id: Uuid,
        conditions: Value,
        actions: Value,
    }
    let Ok(rules) = sqlx::query_as::<_, RuleRow>(
        "SELECT id, conditions, actions FROM rules
         WHERE tenant_id = $1 AND enabled AND trigger_type = $2
         ORDER BY position",
    )
    .bind(tenant_id)
    .bind(trigger_type)
    .fetch_all(pool)
    .await
    else {
        return;
    };
    for rule in &rules {
        let passes = match &rule.conditions {
            Value::Array(conditions) => conditions_match(conditions, &context),
            _ => true,
        };
        if !passes {
            continue;
        }
        // actions: [{"type": "create_notification", "title", "body"}, ...]
        let mut status = "succeeded";
        if let Value::Array(actions) = &rule.actions {
            for action in actions {
                let kind = action.get("type").and_then(Value::as_str).unwrap_or("");
                if kind != "create_notification" {
                    // email/sms/webhook actions ride the notify providers later.
                    status = "failed";
                    continue;
                }
                let created = db::alarms::create_notification_deduped(
                    pool,
                    Uuid::nil(),
                    "in_app",
                    action.get("title").and_then(Value::as_str),
                    action.get("body").and_then(Value::as_str),
                    Some(context.clone()),
                    &format!("rule:{}:{}", rule.id, subject_id),
                )
                .await;
                if created.is_err() {
                    status = "failed";
                }
            }
        }
        let _ = sqlx::query(
            "INSERT INTO rule_executions (id, rule_id, subject_type, subject_id, status)
             VALUES ($1, $2, 'event', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(rule.id)
        .bind(subject_id)
        .bind(status)
        .execute(pool)
        .await;
    }
}

fn conditions_match(conditions: &[Value], _context: &Value) -> bool {
    conditions.iter().all(|_| true) // ponytail: condition DSL (field/op/value) lands with the rules UI
}

// ============ provider CRUD ============

#[derive(serde::Deserialize)]
struct ProviderBody {
    kind: String,
    name: String,
    /// Credentials envelope-encrypted with the environment key before storage.
    config: Value,
}

async fn create_provider(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProviderBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    if calendar_notify::Provider::from_db_str(&body.kind).is_none() {
        return Err(AppError::bad_request("unknown provider kind"));
    }
    let crypto = crypto
        .as_ref()
        .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let encrypted = crypto
        .encrypt(body.config.to_string().as_bytes())
        .map_err(|e| AppError::internal(e.to_string()))?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_providers (id, tenant_id, kind, name, config_encrypted)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, kind, name) DO UPDATE SET
            config_encrypted = $5, enabled = true, updated_at = now()
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&body.kind)
    .bind(&body.name)
    .bind(&encrypted)
    .fetch_one(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({"id": id}))))
}

async fn list_providers(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        kind: String,
        name: String,
        enabled: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, kind, name, enabled FROM notification_providers
         WHERE tenant_id = $1 ORDER BY created_at",
    )
    .bind(tenant_id)
    .fetch_all(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter()
            .map(|r| json!({"id": r.id, "kind": r.kind, "name": r.name, "enabled": r.enabled}))
            .collect::<Vec<_>>()
    )))
}

async fn delete_provider(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    sqlx::query("DELETE FROM notification_providers WHERE id = $1 AND tenant_id = $2")
        .bind(provider_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(sqlx::FromRow)]
struct RuleListRow {
    id: Uuid,
    name: String,
    enabled: bool,
    trigger_type: String,
    conditions: Value,
    actions: Value,
}

async fn create_rule(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RuleBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO rules (id, tenant_id, name, enabled, trigger_type, conditions, actions, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&body.name)
    .bind(body.enabled.unwrap_or(true))
    .bind(&body.trigger_type)
    .bind(body.conditions.clone().unwrap_or(json!([])))
    .bind(body.actions.clone().unwrap_or(json!([])))
    .bind(auth.user.id)
    .fetch_one(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({"id": id}))))
}

async fn list_rules(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let rows: Vec<RuleListRow> = sqlx::query_as(
        "SELECT id, name, enabled, trigger_type, conditions, actions
         FROM rules WHERE tenant_id = $1 ORDER BY position",
    )
    .bind(tenant_id)
    .fetch_all(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter()
            .map(|r| json!({
                "id": r.id, "name": r.name, "enabled": r.enabled,
                "trigger_type": r.trigger_type, "conditions": r.conditions, "actions": r.actions,
            }))
            .collect::<Vec<_>>()
    )))
}

async fn delete_rule(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(rule_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    sqlx::query("DELETE FROM rules WHERE id = $1 AND tenant_id = $2")
        .bind(rule_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/notification-providers",
            post(create_provider).get(list_providers),
        )
        .route("/api/notification-providers/{id}", delete(delete_provider))
        .route("/api/rules", post(create_rule).get(list_rules))
        .route("/api/rules/{id}", delete(delete_rule))
}
