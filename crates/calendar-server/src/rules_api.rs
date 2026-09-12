//! Rules (ADR-007) and notification providers (docs/PRD.md section 16):
//! simple trigger → optional conditions → actions engine, executed inline on
//! event mutations, with executions recorded for the audit trail.

use crate::{AppError, AppState, require_admin, require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{delete, patch, post},
};
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use calendar_notify::SmsProvider;
use serde_json::{Value, json};
use uuid::Uuid;

// ============ rules CRUD ============

#[derive(serde::Deserialize)]
struct RuleBody {
    name: String,
    enabled: Option<bool>,
    trigger_type: String,
    /// Calendar this rule applies to; omit/null for tenant-wide (all calendars).
    calendar_id: Option<Uuid>,
    conditions: Option<Value>,
    actions: Option<Value>,
}

/// The tenant's enabled Twilio provider, config decrypted.
async fn load_sms_provider(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    crypto: Option<&calendar_auth::Crypto>,
) -> Option<SmsProvider> {
    let crypto = crypto?;
    #[derive(sqlx::FromRow)]
    struct Row {
        config_encrypted: Vec<u8>,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT config_encrypted FROM notification_providers
         WHERE tenant_id = $1 AND enabled AND kind = 'twilio' LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let config: Value =
        serde_json::from_slice(&crypto.decrypt(&row.config_encrypted).ok()?).ok()?;
    SmsProvider::from_config(&config).ok()
}

/// Evaluates rules for one trigger. Called from event mutations; failures are
/// logged, never propagated (rules must not break the mutation).
pub(crate) async fn run_rules(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    calendar_id: Uuid,
    trigger_type: &str,
    subject_id: Uuid,
    context: Value,
    crypto: Option<&calendar_auth::Crypto>,
) {
    #[derive(sqlx::FromRow)]
    struct RuleRow {
        id: Uuid,
        conditions: Value,
        actions: Value,
    }
    // A rule with calendar_id NULL applies tenant-wide (all calendars).
    let Ok(rules) = sqlx::query_as::<_, RuleRow>(
        "SELECT id, conditions, actions FROM rules
         WHERE tenant_id = $1 AND enabled AND trigger_type = $2
         AND (calendar_id = $3 OR calendar_id IS NULL)
         ORDER BY position",
    )
    .bind(tenant_id)
    .bind(trigger_type)
    .bind(calendar_id)
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
        // or [{"type": "sms", "to": "+1...", "body": "..."}, ...] (needs a
        // tenant Twilio provider configured; the "to" number is explicit —
        // no per-user phone-number resolution yet).
        let mut status = "succeeded";
        if let Value::Array(actions) = &rule.actions {
            for action in actions {
                let kind = action.get("type").and_then(Value::as_str).unwrap_or("");
                let ok = match kind {
                    "create_notification" => db::alarms::create_notification_deduped(
                        pool,
                        Uuid::nil(),
                        "in_app",
                        action.get("title").and_then(Value::as_str),
                        action.get("body").and_then(Value::as_str),
                        Some(context.clone()),
                        &format!("rule:{}:{}", rule.id, subject_id),
                    )
                    .await
                    .is_ok(),
                    "sms" => match action.get("to").and_then(Value::as_str) {
                        Some(to) => match load_sms_provider(pool, tenant_id, crypto).await {
                            Some(sms) => {
                                let body = action.get("body").and_then(Value::as_str).unwrap_or("");
                                match sms.send(to, body).await {
                                    Ok(()) => true,
                                    Err(e) => {
                                        tracing::warn!(rule_id = %rule.id, error = %e, "rule sms action failed");
                                        false
                                    }
                                }
                            }
                            None => {
                                tracing::warn!(rule_id = %rule.id, "rule sms action skipped: no Twilio provider configured");
                                false
                            }
                        },
                        None => false,
                    },
                    _ => false, // email/webhook actions ride the notify providers later.
                };
                if !ok {
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
    require_admin(&auth)?;
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
    require_admin(&auth)?;
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
    require_admin(&auth)?;
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
    calendar_id: Option<Uuid>,
    conditions: Value,
    actions: Value,
}

async fn create_rule(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RuleBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    if let Some(calendar_id) = body.calendar_id {
        require_capability(&pool, calendar_id, auth.user.id, CalendarCapability::Owner).await?;
    }
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO rules (id, tenant_id, calendar_id, name, enabled, trigger_type, conditions, actions, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(body.calendar_id)
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

#[derive(serde::Deserialize)]
struct RulesQuery {
    calendar_id: Option<Uuid>,
}

async fn list_rules(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RulesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    // A rule with calendar_id NULL applies tenant-wide (all calendars).
    let rows: Vec<RuleListRow> = sqlx::query_as(
        "SELECT id, name, enabled, trigger_type, calendar_id, conditions, actions
         FROM rules WHERE tenant_id = $1 AND (calendar_id = $2 OR calendar_id IS NULL)
         ORDER BY position",
    )
    .bind(tenant_id)
    .bind(query.calendar_id)
    .fetch_all(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter()
            .map(|r| json!({
                "id": r.id, "name": r.name, "enabled": r.enabled, "trigger_type": r.trigger_type,
                "calendar_id": r.calendar_id, "conditions": r.conditions, "actions": r.actions,
            }))
            .collect::<Vec<_>>()
    )))
}

#[derive(serde::Deserialize)]
struct RuleUpdateBody {
    enabled: bool,
}

async fn update_rule(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(rule_id): Path<Uuid>,
    Json(body): Json<RuleUpdateBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    sqlx::query("UPDATE rules SET enabled = $1 WHERE id = $2 AND tenant_id = $3")
        .bind(body.enabled)
        .bind(rule_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_rule(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(rule_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
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
        .route("/api/rules/{id}", patch(update_rule).delete(delete_rule))
}
