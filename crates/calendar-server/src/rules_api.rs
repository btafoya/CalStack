//! Rules (ADR-007) and notification providers (docs/PRD.md section 16):
//! simple trigger → optional conditions → actions engine, executed inline on
//! event mutations, with executions recorded for the audit trail.

use crate::{
    AppError, AppState, require_admin, require_capability, require_csrf, require_session,
    resolve_auth,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{patch, post},
};
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use calendar_notify::SmsProvider;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

// ============ rules CRUD ============

/// Only the triggers the engine actually fires on event mutations.
const TRIGGER_TYPES: [&str; 11] = [
    "event_created",
    "event_updated",
    "event_deleted",
    "task_created",
    "task_updated",
    "task_deleted",
    "task_completed",
    "task_due",
    "journal_created",
    "journal_updated",
    "journal_deleted",
];

fn validate_trigger_type(trigger_type: &str) -> Result<(), AppError> {
    if TRIGGER_TYPES.contains(&trigger_type) {
        Ok(())
    } else {
        Err(AppError::bad_request(format!(
            "trigger_type must be one of: {}",
            TRIGGER_TYPES.join(", ")
        )))
    }
}

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
pub(crate) async fn load_sms_provider(
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
                    "create_notification" => {
                        // Address the event calendar's principals so the
                        // in-app rows are actually visible (user_id = NULL
                        // rows never appear in /api/notifications); a
                        // calendar without ACL rows falls back to tenant
                        // members.
                        let mut users: Vec<Uuid> = sqlx::query_scalar(
                            "SELECT u.id FROM calendar_acl acl
                             JOIN users u ON u.id = acl.principal_user_id
                             WHERE acl.calendar_id = $1",
                        )
                        .bind(calendar_id)
                        .fetch_all(pool)
                        .await
                        .unwrap_or_default();
                        if users.is_empty() {
                            users = sqlx::query_scalar(
                                "SELECT user_id FROM tenant_members WHERE tenant_id = $1",
                            )
                            .bind(tenant_id)
                            .fetch_all(pool)
                            .await
                            .unwrap_or_default();
                        }
                        if users.is_empty() {
                            false
                        } else {
                            let title = action.get("title").and_then(Value::as_str);
                            let body = action.get("body").and_then(Value::as_str);
                            let mut ok = true;
                            for user_id in users {
                                ok &= db::alarms::create_notification_deduped(
                                    pool,
                                    Some(user_id),
                                    "in_app",
                                    title,
                                    body,
                                    Some(context.clone()),
                                    &format!("rule:{}:{}", rule.id, subject_id),
                                )
                                .await
                                .is_ok();
                            }
                            ok
                        }
                    }
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
                    "webhook" => {
                        // Deliver to every tenant webhook subscribed to rule
                        // actions; the delivery payload carries the rule and
                        // subject, not the rule's action config.
                        crate::webhooks_api::fire(pool, tenant_id, subject_id, "rule_action").await;
                        true
                    }
                    _ => false, // email actions ride the notify providers later.
                };
                if !ok {
                    status = "failed";
                }
            }
        }
        let subject_type = if trigger_type.starts_with("task") {
            "task"
        } else if trigger_type.starts_with("journal") {
            "journal"
        } else {
            "event"
        };
        let _ = sqlx::query(
            "INSERT INTO rule_executions (id, rule_id, subject_type, subject_id, status)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(rule.id)
        .bind(subject_id)
        .bind(subject_type)
        .bind(status)
        .execute(pool)
        .await;
    }
}

/// One condition is `{"field": "dot.path", "op": "eq|ne|contains|in|exists", "value": ...}`;
/// all stored conditions must hold. A field that is missing from the context
/// fails every op except `exists` (whose `value` true/false asserts presence).
fn conditions_match(conditions: &[Value], context: &Value) -> bool {
    conditions.iter().all(|c| {
        let field = c.get("field").and_then(Value::as_str).unwrap_or("");
        let op = c.get("op").and_then(Value::as_str).unwrap_or("");
        let value = c.get("value").unwrap_or(&Value::Null);
        let actual = match lookup(context, field) {
            Some(actual) => actual,
            // Absent field: only `exists` with value false (asserting absence) passes.
            None => return op == "exists" && value.as_bool() == Some(false),
        };
        match op {
            "eq" => actual == value,
            "ne" => actual != value,
            "contains" => match actual {
                Value::Array(items) => items.contains(value),
                Value::String(s) => value.as_str().is_some_and(|v| s.contains(v)),
                _ => false,
            },
            "in" => value
                .as_array()
                .is_some_and(|options| options.contains(actual)),
            "exists" => value.as_bool().unwrap_or(true),
            _ => false, // unknown op: condition fails
        }
    })
}

fn lookup<'a>(context: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(context, |value, segment| value.get(segment))
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
    let kind = calendar_notify::Provider::from_db_str(&body.kind)
        .ok_or_else(|| AppError::BadRequest("unknown provider kind".into()))?;
    // Webpush providers get a server-generated VAPID key pair on first save;
    // the public key is later served to subscribing browsers.
    let mut config = body.config.clone();
    if kind == calendar_notify::Provider::WebPush && config.get("vapid_private").is_none() {
        let (private, public) = calendar_notify::generate_vapid_keys()
            .map_err(|e| AppError::internal(e.to_string()))?;
        config["vapid_private"] = json!(private);
        config["vapid_public"] = json!(public);
    }
    let crypto = crypto
        .as_ref()
        .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let encrypted = crypto
        .encrypt(config.to_string().as_bytes())
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

/// One provider with its decrypted config (admin-only; the edit modal
/// pre-fills credentials from this).
async fn get_provider(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    // Decrypted provider secrets (incl. the VAPID private key) must not be
    // readable by scoped bearer tokens: session + admin only.
    require_session(&auth)?;
    let crypto = crypto
        .as_ref()
        .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        name: String,
        enabled: bool,
        config_encrypted: Vec<u8>,
    }
    let row: Row = sqlx::query_as(
        "SELECT kind, name, enabled, config_encrypted FROM notification_providers
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(provider_id)
    .bind(tenant_id)
    .fetch_optional(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?
    .ok_or(AppError::NotFound)?;
    let config: Value = serde_json::from_slice(
        &crypto
            .decrypt(&row.config_encrypted)
            .map_err(|e| AppError::internal(e.to_string()))?,
    )
    .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!({
        "id": provider_id, "kind": row.kind, "name": row.name,
        "enabled": row.enabled, "config": config,
    })))
}

#[derive(serde::Deserialize)]
struct ProviderPatch {
    name: Option<String>,
    enabled: Option<bool>,
    config: Option<Value>,
}

async fn patch_provider(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<Uuid>,
    Json(body): Json<ProviderPatch>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let encrypted = match &body.config {
        Some(config) => {
            let crypto = crypto
                .as_ref()
                .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
            Some(
                crypto
                    .encrypt(config.to_string().as_bytes())
                    .map_err(|e| AppError::internal(e.to_string()))?,
            )
        }
        None => None,
    };
    sqlx::query(
        "UPDATE notification_providers SET
            name = COALESCE($3, name),
            enabled = COALESCE($4, enabled),
            config_encrypted = COALESCE($5, config_encrypted),
            updated_at = now()
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(provider_id)
    .bind(tenant_id)
    .bind(body.name)
    .bind(body.enabled)
    .bind(encrypted)
    .execute(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

/// Sends a test message through the provider's real channel. Always returns
/// 200 with ok true/false so the UI can show the outcome inline in the test
/// modal instead of the shared api() alert.
#[derive(serde::Deserialize)]
struct ProviderTestBody {
    to: String,
    subject: Option<String>,
    body: Option<String>,
}

async fn test_provider(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<Uuid>,
    Json(body): Json<ProviderTestBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    if body.to.trim().is_empty() {
        return Err(AppError::BadRequest("recipient required".into()));
    }
    let crypto = crypto
        .as_ref()
        .ok_or_else(|| AppError::Internal("APP_ENCRYPTION_KEY is not set".into()))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        config_encrypted: Vec<u8>,
    }
    let row: Row = sqlx::query_as(
        "SELECT kind, config_encrypted FROM notification_providers
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(provider_id)
    .bind(tenant_id)
    .fetch_optional(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?
    .ok_or(AppError::NotFound)?;
    let config: Value = serde_json::from_slice(
        &crypto
            .decrypt(&row.config_encrypted)
            .map_err(|e| AppError::Internal(e.to_string()))?,
    )
    .map_err(|e| AppError::Internal(e.to_string()))?;
    let kind = calendar_notify::Provider::from_db_str(&row.kind)
        .ok_or_else(|| AppError::BadRequest("unknown provider kind".into()))?;
    let subject = body
        .subject
        .unwrap_or_else(|| "CalStack test message".into());
    let text = body
        .body
        .clone()
        .unwrap_or_else(|| format!("Test message sent from CalStack at {}.", Utc::now()));
    let result: Result<(), calendar_notify::NotifyError> = async {
        match kind {
            calendar_notify::Provider::Postmark | calendar_notify::Provider::Smtp => {
                let provider = calendar_notify::EmailProvider::from_config(kind, &config)?;
                provider.send(&body.to, &subject, &text).await
            }
            calendar_notify::Provider::Twilio => {
                let sms = calendar_notify::SmsProvider::from_config(&config)?;
                // SMS has no subject; only the body goes on the wire.
                sms.send(&body.to, &body.body.unwrap_or(text)).await
            }
            calendar_notify::Provider::WebPush => Err(calendar_notify::NotifyError::Config(
                "webpush has no sender implemented".into(),
            )),
        }
    }
    .await;
    Ok(Json(match result {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }))
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
    validate_trigger_type(&body.trigger_type)?;
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
    /// Only present when the caller changes the trigger; validated like create.
    trigger_type: Option<String>,
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
    if let Some(trigger_type) = &body.trigger_type {
        validate_trigger_type(trigger_type)?;
    }
    sqlx::query(
        "UPDATE rules SET enabled = $1, trigger_type = COALESCE($2, trigger_type)
         WHERE id = $3 AND tenant_id = $4",
    )
    .bind(body.enabled)
    .bind(body.trigger_type)
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
        .route(
            "/api/notification-providers/{id}",
            axum::routing::get(get_provider)
                .patch(patch_provider)
                .delete(delete_provider),
        )
        .route("/api/notification-providers/{id}/test", post(test_provider))
        .route("/api/rules", post(create_rule).get(list_rules))
        .route("/api/rules/{id}", patch(update_rule).delete(delete_rule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn matches(conditions: Value, context: Value) -> bool {
        conditions_match(conditions.as_array().unwrap(), &context)
    }

    #[test]
    fn eq_ne_on_top_level_field() {
        let ctx = json!({"summary": "Standup", "starts_at": "2026-09-19T10:00:00Z"});
        assert!(matches(
            json!([{"field": "summary", "op": "eq", "value": "Standup"}]),
            ctx.clone()
        ));
        assert!(!matches(
            json!([{"field": "summary", "op": "eq", "value": "Other"}]),
            ctx.clone()
        ));
        assert!(matches(
            json!([{"field": "summary", "op": "ne", "value": "Other"}]),
            ctx.clone()
        ));
        assert!(!matches(
            json!([{"field": "summary", "op": "ne", "value": "Standup"}]),
            ctx
        ));
    }

    #[test]
    fn dot_path_walks_nested_fields() {
        let ctx = json!({"location": {"display_name": "Library"}});
        assert!(matches(
            json!([{"field": "location.display_name", "op": "eq", "value": "Library"}]),
            ctx
        ));
    }

    #[test]
    fn contains_array_and_string() {
        assert!(matches(
            json!([{"field": "categories", "op": "contains", "value": "work"}]),
            json!({"categories": ["work", "focus"]})
        ));
        assert!(matches(
            json!([{"field": "summary", "op": "contains", "value": "standup"}]),
            json!({"summary": "Team standup"})
        ));
        assert!(!matches(
            json!([{"field": "categories", "op": "contains", "value": "home"}]),
            json!({"categories": ["work"]})
        ));
    }

    #[test]
    fn in_matches_any_option() {
        let ctx = json!({"summary": "Standup"});
        assert!(matches(
            json!([{"field": "summary", "op": "in", "value": ["Standup", "Retro"]}]),
            ctx.clone()
        ));
        assert!(!matches(
            json!([{"field": "summary", "op": "in", "value": ["Retro", "Planning"]}]),
            ctx
        ));
    }

    #[test]
    fn exists_checks_presence() {
        assert!(matches(
            json!([{"field": "summary", "op": "exists", "value": true}]),
            json!({"summary": "x"})
        ));
        assert!(!matches(
            json!([{"field": "url", "op": "exists", "value": true}]),
            json!({"summary": "x"})
        ));
        assert!(matches(
            json!([{"field": "url", "op": "exists", "value": false}]),
            json!({"summary": "x"})
        ));
        assert!(matches(
            json!([{"field": "location.display_name", "op": "exists", "value": true}]),
            json!({"location": {"display_name": "x"}})
        ));
        // A null intermediate segment is still an absent field.
        assert!(!matches(
            json!([{"field": "location.display_name", "op": "exists", "value": true}]),
            json!({"location": null})
        ));
    }

    #[test]
    fn unknown_op_or_field_fails() {
        let ctx = json!({"summary": "Standup"});
        assert!(!matches(
            json!([{"field": "summary", "op": "regex", "value": "."}]),
            ctx.clone()
        ));
        // Absent field fails every op except exists.
        assert!(!matches(
            json!([{"field": "url", "op": "eq", "value": null}]),
            ctx.clone()
        ));
        // Malformed condition (no op) fails rather than passing silently.
        assert!(!matches(
            json!([{"field": "summary", "value": "Standup"}]),
            ctx
        ));
    }
}
