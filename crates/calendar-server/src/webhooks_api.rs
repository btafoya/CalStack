//! Webhook endpoint CRUD (docs/DEFERRED_REQUIREMENTS.md item 7): per-tenant
//! signed delivery targets, admin-gated like the notification providers.
//! The sign key is stored encrypted and never returned or logged.

use crate::{AppError, AppState, require_admin, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use calendar_db::{self as db, webhooks::TRIGGERS};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

/// Validates the subscribed trigger set; empty means every trigger.
fn validate_triggers(events: &[String]) -> Result<(), AppError> {
    if events.iter().all(|e| TRIGGERS.contains(&e.as_str())) {
        Ok(())
    } else {
        Err(AppError::bad_request(format!(
            "events must be a subset of: {}",
            TRIGGERS.join(", ")
        )))
    }
}

fn validate_url(url: &str) -> Result<(), AppError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(AppError::BadRequest("url must be http(s)".into()))
    }
}

/// Encrypts a caller-supplied sign key; empty string clears it.
fn encrypt_sign_key(
    crypto: Option<&std::sync::Arc<calendar_auth::Crypto>>,
    sign_key: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    if sign_key.trim().is_empty() {
        return Ok(None);
    }
    let crypto = crypto
        .as_ref()
        .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    crypto
        .encrypt(sign_key.as_bytes())
        .map(Some)
        .map_err(|e| AppError::internal(e.to_string()))
}

/// One webhook without its secret; `has_sign_key` is all callers ever learn.
fn webhook_json(w: &db::webhooks::WebhookRow) -> Value {
    json!({
        "id": w.id,
        "url": w.url,
        "name": w.name,
        "enabled": w.enabled,
        "events": w.events,
        "has_sign_key": w.secret_encrypted.is_some(),
        "created_at": w.created_at.to_rfc3339(),
    })
}

#[derive(serde::Deserialize)]
struct WebhookBody {
    url: String,
    name: String,
    enabled: Option<bool>,
    /// Subscribed triggers; empty/omitted = all of them.
    events: Option<Vec<String>>,
    /// Optional shared secret for X-CalStack-Signature; stored encrypted.
    sign_key: Option<String>,
}

async fn create_webhook(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WebhookBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    validate_url(&body.url)?;
    let events = body.events.unwrap_or_default();
    validate_triggers(&events)?;
    let secret_encrypted = match &body.sign_key {
        Some(key) => encrypt_sign_key(crypto.as_ref(), key)?,
        None => None,
    };
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::create_webhook(
        &pool,
        tenant_id,
        &db::webhooks::NewWebhook {
            url: body.url,
            name: body.name,
            events,
            secret_encrypted,
            enabled: body.enabled.unwrap_or(true),
        },
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(webhook_json(&webhook)),
    ))
}

async fn list_webhooks(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let rows = db::webhooks::list_webhooks(&pool, tenant_id).await?;
    Ok(Json(json!(
        rows.iter().map(webhook_json).collect::<Vec<_>>()
    )))
}

async fn get_webhook(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(webhook_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::get_webhook(&pool, webhook_id).await?;
    // Tenant scoping is enforced here, not in the query, so cross-tenant ids
    // 404 identically.
    if webhook.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    Ok(Json(webhook_json(&webhook)))
}

#[derive(serde::Deserialize)]
struct WebhookPatch {
    url: Option<String>,
    name: Option<String>,
    enabled: Option<bool>,
    events: Option<Vec<String>>,
    /// Replace (or, with "", clear) the sign key; omitted leaves it untouched.
    sign_key: Option<String>,
}

async fn patch_webhook(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(webhook_id): Path<Uuid>,
    Json(body): Json<WebhookPatch>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    if let Some(url) = &body.url {
        validate_url(url)?;
    }
    if let Some(events) = &body.events {
        validate_triggers(events)?;
    }
    let secret_encrypted = match &body.sign_key {
        Some(key) => Some(encrypt_sign_key(crypto.as_ref(), key)?),
        None => None,
    };
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::get_webhook(&pool, webhook_id).await?;
    if webhook.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    let updated = db::webhooks::update_webhook(
        &pool,
        webhook_id,
        &db::webhooks::WebhookUpdate {
            url: body.url,
            name: body.name,
            events: body.events,
            secret_encrypted,
            enabled: body.enabled,
        },
    )
    .await?;
    Ok(Json(webhook_json(&updated)))
}

async fn delete_webhook(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(webhook_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::get_webhook(&pool, webhook_id).await?;
    if webhook.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    db::webhooks::delete_webhook(&pool, webhook_id).await?;
    Ok(Json(json!({"ok": true})))
}

/// Sends a real signed delivery with a fake event to the configured URL,
/// synchronously, and records the attempt — mirrors the notification-provider
/// test pattern (always 200 with ok true/false).
async fn test_webhook(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(webhook_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::get_webhook(&pool, webhook_id).await?;
    if webhook.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    let crypto = crypto
        .as_deref()
        .ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    // Fake event: never touches the events table, so nothing to clean up.
    let event = test_event_defaults();
    let view = db::webhooks::event_view(&event);
    let delivery_id = db::webhooks::create_delivery(&pool, webhook.id, event.id, "test").await?;
    match crate::jobs::send_delivery_once(
        &pool,
        &webhook,
        view,
        "test",
        delivery_id,
        1,
        Some(crypto),
    )
    .await
    {
        Ok(()) => Ok(Json(json!({"ok": true, "delivery_id": delivery_id}))),
        Err(e) => Ok(Json(
            json!({"ok": false, "error": e, "delivery_id": delivery_id}),
        )),
    }
}

async fn list_deliveries(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(webhook_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let webhook = db::webhooks::get_webhook(&pool, webhook_id).await?;
    if webhook.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    let rows = db::webhooks::list_deliveries(&pool, webhook_id, 50).await?;
    Ok(Json(json!(
        rows.iter()
            .map(|d| json!({
                "id": d.id,
                "status": d.status,
                "attempts": d.attempts,
                "response_code": d.response_code,
                "error": d.error,
                "delivered_at": d.delivered_at.map(|t| t.to_rfc3339()),
                "created_at": d.created_at.to_rfc3339(),
            }))
            .collect::<Vec<_>>()
    )))
}

/// Minimal EventRow for the synthetic test event (no DB row behind it).
fn test_event_defaults() -> db::EventRow {
    db::EventRow {
        id: Uuid::new_v4(),
        calendar_id: Uuid::nil(),
        uid: format!("webhook-test-{}", Uuid::new_v4()),
        summary: "CalStack webhook test".into(),
        starts_at: Some(Utc::now()),
        ends_at: Some(Utc::now() + chrono::Duration::hours(1)),
        start_date: None,
        end_date: None,
        duration: None,
        tzid: None,
        all_day: false,
        floating: false,
        rrule: None,
        rdate: Value::Null,
        exdate: Value::Null,
        description_html: None,
        description_text: None,
        url: None,
        status: None,
        priority: None,
        class: None,
        transp: None,
        categories: Vec::new(),
        location_id: None,
        organizer_user_id: None,
        organizer_email: String::new(),
        organizer_name: None,
        created_by: None,
        sequence: 0,
        etag: String::new(),
        deleted_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        master_event_id: None,
        recurrence_id: None,
        recurrence_id_date: None,
        is_exception: false,
        href: None,
    }
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/webhooks", post(create_webhook).get(list_webhooks))
        .route(
            "/api/webhooks/{id}",
            get(get_webhook).patch(patch_webhook).delete(delete_webhook),
        )
        .route("/api/webhooks/{id}/test", post(test_webhook))
        .route("/api/webhooks/{id}/deliveries", get(list_deliveries))
}

/// Fires the tenant's matching webhooks for one event trigger. Fire-and-forget:
/// enqueue failures are logged and skipped, never breaking the mutation.
pub(crate) async fn fire(pool: &sqlx::PgPool, tenant_id: Uuid, event_id: Uuid, trigger: &str) {
    let webhooks = db::webhooks::matching_webhooks(pool, tenant_id, trigger)
        .await
        .unwrap_or_default();
    for webhook in webhooks {
        if let Err(e) = db::webhooks::enqueue_delivery(pool, webhook.id, event_id, trigger).await {
            tracing::warn!(error = %e, "webhook delivery not enqueued");
        }
    }
}
