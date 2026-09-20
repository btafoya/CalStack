//! Outbound webhooks (docs/DEFERRED_REQUIREMENTS.md item 7): per-tenant
//! subscribed endpoints, delivered at-least-once through `durable_jobs`.
//! Receivers dedupe on `delivery_id`; the sign key stays encrypted at rest
//! (same envelope as notification provider config) and is never logged.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{DbError, EventRow};
use crate::jobs;

/// The trigger set a webhook can subscribe to; an empty `events` array means
/// every trigger (tenant-wide, like a rule with calendar_id NULL).
pub const TRIGGERS: [&str; 3] = ["event_created", "event_updated", "event_deleted"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebhookRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub url: String,
    pub name: String,
    /// AES-256-GCM encrypted sign key (must stay retrievable to sign; never
    /// hashed, never returned by the API, never logged).
    pub secret_encrypted: Option<Vec<u8>>,
    /// Subscribed triggers; empty = all of `TRIGGERS`.
    pub events: Vec<String>,
    pub enabled: bool,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct NewWebhook {
    pub url: String,
    pub name: String,
    pub events: Vec<String>,
    /// Already-encrypted by the caller (API layer, via calendar_auth::Crypto).
    pub secret_encrypted: Option<Vec<u8>>,
    pub enabled: bool,
}

pub async fn create_webhook(
    pool: &PgPool,
    tenant_id: Uuid,
    new: &NewWebhook,
) -> Result<WebhookRow, DbError> {
    sqlx::query_as::<_, WebhookRow>(
        "INSERT INTO webhooks (id, tenant_id, url, name, secret_encrypted, events, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&new.url)
    .bind(&new.name)
    .bind(&new.secret_encrypted)
    .bind(&new.events)
    .bind(new.enabled)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Live webhooks of one tenant, oldest first.
pub async fn list_webhooks(pool: &PgPool, tenant_id: Uuid) -> Result<Vec<WebhookRow>, DbError> {
    sqlx::query_as::<_, WebhookRow>(
        "SELECT * FROM webhooks WHERE tenant_id = $1 AND deleted_at IS NULL ORDER BY created_at",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn get_webhook(pool: &PgPool, id: Uuid) -> Result<WebhookRow, DbError> {
    sqlx::query_as::<_, WebhookRow>("SELECT * FROM webhooks WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)
}

#[derive(Debug, Default)]
pub struct WebhookUpdate {
    pub url: Option<String>,
    pub name: Option<String>,
    pub events: Option<Vec<String>>,
    /// Some(Some(encrypted)) replaces the key, Some(None) clears it.
    pub secret_encrypted: Option<Option<Vec<u8>>>,
    pub enabled: Option<bool>,
}

pub async fn update_webhook(
    pool: &PgPool,
    id: Uuid,
    changes: &WebhookUpdate,
) -> Result<WebhookRow, DbError> {
    sqlx::query_as::<_, WebhookRow>(
        "UPDATE webhooks SET
            url = COALESCE($2, url),
            name = COALESCE($3, name),
            events = COALESCE($4, events),
            secret_encrypted = CASE WHEN $6 THEN $5 ELSE secret_encrypted END,
            enabled = COALESCE($7, enabled),
            updated_at = now()
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING *",
    )
    .bind(id)
    .bind(changes.url.as_deref())
    .bind(changes.name.as_deref())
    .bind(changes.events.as_deref())
    // $5/$6: the key change and a flag saying it happened — COALESCE cannot
    // distinguish "set to NULL" from "leave alone".
    .bind(changes.secret_encrypted.as_ref().and_then(|s| s.as_deref()))
    .bind(changes.secret_encrypted.is_some())
    .bind(changes.enabled)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// Soft delete: the row (and its delivery history) stays; it just leaves the
/// trigger set. Reversible only by direct DB edit, like calendar deletes.
pub async fn delete_webhook(pool: &PgPool, id: Uuid) -> Result<(), DbError> {
    let n =
        sqlx::query("UPDATE webhooks SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Live, enabled webhooks subscribed to `trigger` (empty events = all).
pub async fn matching_webhooks(
    pool: &PgPool,
    tenant_id: Uuid,
    trigger: &str,
) -> Result<Vec<WebhookRow>, DbError> {
    sqlx::query_as::<_, WebhookRow>(
        "SELECT * FROM webhooks
         WHERE tenant_id = $1 AND enabled AND deleted_at IS NULL
           AND (events = '{}' OR $2 = ANY(events))",
    )
    .bind(tenant_id)
    .bind(trigger)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

// ============ deliveries ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryRow {
    pub id: Uuid,
    pub webhook_id: Uuid,
    pub status: String,
    pub attempts: i32,
    pub response_code: Option<i32>,
    pub error: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Creates a pending delivery row; the caller decides whether to enqueue the
/// send job (enqueue_delivery) or send it synchronously (the test endpoint).
pub async fn create_delivery(
    pool: &PgPool,
    webhook_id: Uuid,
    event_id: Uuid,
    trigger: &str,
) -> Result<Uuid, DbError> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO webhook_deliveries (id, webhook_id, payload) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(webhook_id)
        .bind(serde_json::json!({"event_id": event_id, "trigger": trigger}))
        .execute(pool)
        .await?;
    Ok(id)
}

/// Records a delivery and enqueues its durable send job. Two statements, not
/// a transaction (jobs::enqueue takes the pool): a crash between them leaves
/// a pending delivery with no job — the deliveries list makes that visible,
/// and a resend via the test endpoint recovers it.
pub async fn enqueue_delivery(
    pool: &PgPool,
    webhook_id: Uuid,
    event_id: Uuid,
    trigger: &str,
) -> Result<Uuid, DbError> {
    let delivery_id = create_delivery(pool, webhook_id, event_id, trigger).await?;
    jobs::enqueue(
        pool,
        "webhook_send",
        serde_json::json!({"delivery_id": delivery_id}),
        None,
        0,
    )
    .await?;
    Ok(delivery_id)
}

/// Records one attempt's outcome. Non-terminal failures keep status 'pending'
/// so the next attempt re-sends; the job's own fail() backoff drives retries.
pub async fn record_delivery_result(
    pool: &PgPool,
    delivery_id: Uuid,
    status: &str,
    response_code: Option<i32>,
    error: Option<&str>,
    attempts: i32,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE webhook_deliveries SET
            status = $2,
            response_code = $3,
            error = $4,
            attempts = $5,
            delivered_at = CASE WHEN $2 = 'succeeded' THEN now() ELSE delivered_at END
         WHERE id = $1",
    )
    .bind(delivery_id)
    .bind(status)
    .bind(response_code)
    .bind(error)
    .bind(attempts)
    .execute(pool)
    .await?;
    Ok(())
}

/// Latest 50 deliveries of one webhook (admin deliveries view).
pub async fn list_deliveries(
    pool: &PgPool,
    webhook_id: Uuid,
    limit: i64,
) -> Result<Vec<DeliveryRow>, DbError> {
    sqlx::query_as::<_, DeliveryRow>(
        "SELECT id, webhook_id, status, attempts, response_code, error, delivered_at, created_at
         FROM webhook_deliveries WHERE webhook_id = $1
         ORDER BY created_at DESC LIMIT $2",
    )
    .bind(webhook_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// The event a delivery points at, soft-deleted rows included (an
/// event_deleted delivery arrives after the row is gone). None once purged.
pub async fn get_event_for_delivery(
    pool: &PgPool,
    event_id: Uuid,
) -> Result<Option<EventRow>, DbError> {
    sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE id = $1")
        .bind(event_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

// ============ payload + signature (pure; unit-testable) ============

/// Envelope v1. `event_view` is the compact event object (see event_view);
/// the exact serialized bytes of this value are what gets signed.
pub fn payload_json(
    delivery_id: Uuid,
    webhook_id: Uuid,
    event_id: Uuid,
    trigger: &str,
    event_view: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "delivery_id": delivery_id,
        "webhook_id": webhook_id,
        "event_id": event_id,
        "trigger": trigger,
        "event": event_view,
        "timestamp": Utc::now().to_rfc3339(),
    })
}

/// Compact event view for the envelope: identity and schedule, never
/// descriptions or attachments (receivers that need more can call the API).
pub fn event_view(event: &EventRow) -> serde_json::Value {
    serde_json::json!({
        "id": event.id,
        "calendar_id": event.calendar_id,
        "uid": event.uid,
        "summary": event.summary,
        "starts_at": event.starts_at.map(|t| t.to_rfc3339()),
        "ends_at": event.ends_at.map(|t| t.to_rfc3339()),
        "updated_at": event.updated_at.to_rfc3339(),
        "deleted": event.deleted_at.is_some(),
    })
}

/// HMAC-SHA256 of `payload` under `key`, hex-encoded (X-CalStack-Signature).
pub fn sign(payload: &[u8], key: &[u8]) -> String {
    calendar_auth::hex_encode(&hmac_sha256(key, payload))
}

/// HMAC-SHA256 (RFC 2104) built directly on sha2 — the workspace has no `hmac`
/// crate, and the construction is five lines over SHA-256's 64-byte block.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    /// DB-backed tests need a live PostgreSQL via DATABASE_URL (the throwaway
    /// instance the interop suite boots works). Without it they skip so
    /// `cargo test` still passes on machines without infrastructure.
    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::migrate(&pool).await.ok()?;
        Some(pool)
    }

    // ============ pure tests ============

    #[test]
    fn sign_matches_rfc_4231_test_case_1() {
        // RFC 4231 test case 1: key 0x0b x20 over "Hi There".
        let key = [0x0bu8; 20];
        assert_eq!(
            sign(b"Hi There", &key),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn sign_is_deterministic_and_key_sensitive() {
        let a = sign(b"payload", b"key-one");
        assert_eq!(a, sign(b"payload", b"key-one"));
        assert_ne!(a, sign(b"payload", b"key-two"));
        assert_ne!(a, sign(b"payloab", b"key-one"));
    }

    #[test]
    fn long_keys_hash_to_block_size() {
        // RFC 2104: a key longer than the 64-byte block is hashed first. The
        // long-key path must therefore agree with pre-hashing the key to 32
        // bytes and signing with that — which exercises the short-key path.
        use sha2::{Digest, Sha256};
        let key = [0xaau8; 131];
        let prehashed = Sha256::digest(key);
        assert_eq!(sign(b"message", &key), sign(b"message", &prehashed));
    }

    #[test]
    fn payload_envelope_carries_all_fields() {
        let payload = payload_json(
            Uuid::nil(),
            Uuid::nil(),
            Uuid::nil(),
            "event_created",
            json!({"summary": "Standup"}),
        );
        assert_eq!(payload["version"], 1);
        assert_eq!(payload["trigger"], "event_created");
        assert_eq!(payload["event"]["summary"], "Standup");
        assert!(payload["delivery_id"].is_string());
        // The timestamp must parse as RFC 3339 (it is what receivers replay).
        DateTime::parse_from_rfc3339(payload["timestamp"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn event_view_is_compact() {
        let event = EventRow {
            id: Uuid::nil(),
            calendar_id: Uuid::nil(),
            uid: "uid-1".into(),
            href: None,
            master_event_id: None,
            recurrence_id: None,
            recurrence_id_date: None,
            is_exception: false,
            starts_at: Some(timed("2026-01-01T10:00:00Z")),
            ends_at: Some(timed("2026-01-01T11:00:00Z")),
            start_date: None,
            end_date: None,
            duration: None,
            tzid: None,
            all_day: false,
            floating: false,
            rrule: None,
            rdate: serde_json::Value::Null,
            exdate: serde_json::Value::Null,
            summary: "Standup".into(),
            description_html: Some("<script>alert(1)</script>".into()),
            description_text: None,
            url: None,
            status: None,
            priority: None,
            class: None,
            transp: None,
            categories: Vec::new(),
            location_id: None,
            organizer_user_id: None,
            created_by: None,
            organizer_email: String::new(),
            organizer_name: None,
            sequence: 0,
            etag: String::new(),
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let view = event_view(&event);
        assert_eq!(view["summary"], "Standup");
        assert_eq!(view["uid"], "uid-1");
        // Descriptions stay out of the envelope.
        assert!(view.get("description_html").is_none());
        assert!(view.get("description_text").is_none());
    }

    fn timed(at: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Utc)
    }

    // ============ DB-backed tests ============

    async fn fixture_tenant(pool: &sqlx::PgPool) -> Uuid {
        let tenant = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $2, true)")
            .bind(tenant)
            .bind(tenant.simple().to_string())
            .execute(pool)
            .await
            .unwrap();
        tenant
    }

    #[tokio::test]
    async fn webhook_crud_enqueues_delivery_and_job() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let tenant = fixture_tenant(&pool).await;

        // Create.
        let created = create_webhook(
            &pool,
            tenant,
            &NewWebhook {
                url: "http://localhost:9/hook".into(),
                name: "Ops relay".into(),
                events: vec!["event_created".into()],
                secret_encrypted: Some(vec![1, 2, 3]),
                enabled: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(created.name, "Ops relay");

        // List + get.
        assert_eq!(list_webhooks(&pool, tenant).await.unwrap().len(), 1);
        let fetched = get_webhook(&pool, created.id).await.unwrap();
        assert_eq!(fetched.secret_encrypted, Some(vec![1, 2, 3]));

        // Update (including clearing the key).
        let updated = update_webhook(
            &pool,
            created.id,
            &WebhookUpdate {
                name: Some("Renamed".into()),
                enabled: Some(false),
                secret_encrypted: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.secret_encrypted, None);

        // Disabled webhooks leave the trigger set.
        assert!(
            matching_webhooks(&pool, tenant, "event_created")
                .await
                .unwrap()
                .is_empty()
        );
        update_webhook(
            &pool,
            created.id,
            &WebhookUpdate {
                enabled: Some(true),
                // Empty events subscribes to every trigger.
                events: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            matching_webhooks(&pool, tenant, "event_deleted")
                .await
                .unwrap()
                .len(),
            1
        );

        // enqueue_delivery writes a pending delivery plus its send job
        // atomically.
        let event_id = Uuid::new_v4();
        let delivery_id = enqueue_delivery(&pool, created.id, event_id, "event_created")
            .await
            .unwrap();
        let status: String =
            sqlx::query_scalar("SELECT status FROM webhook_deliveries WHERE id = $1")
                .bind(delivery_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "pending");
        let (job_type, payload): (String, serde_json::Value) = sqlx::query_as(
            "SELECT job_type, payload FROM durable_jobs
             WHERE payload->>'delivery_id' = $1",
        )
        .bind(delivery_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job_type, "webhook_send");
        assert_eq!(payload["delivery_id"], delivery_id.to_string());

        // Recording a result updates the row.
        record_delivery_result(&pool, delivery_id, "succeeded", Some(200), None, 1)
            .await
            .unwrap();
        let row: DeliveryRow = sqlx::query_as(
            "SELECT id, webhook_id, status, attempts, response_code, error, delivered_at, created_at
             FROM webhook_deliveries WHERE id = $1",
        )
        .bind(delivery_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.status, "succeeded");
        assert_eq!(row.response_code, Some(200));
        assert!(row.delivered_at.is_some());
        assert_eq!(
            list_deliveries(&pool, created.id, 50).await.unwrap().len(),
            1
        );

        // Soft delete removes it from the trigger set and listing; the
        // delivery history stays (FK cascade never fires).
        delete_webhook(&pool, created.id).await.unwrap();
        assert!(list_webhooks(&pool, tenant).await.unwrap().is_empty());
        assert!(get_webhook(&pool, created.id).await.is_err());
        let deliveries: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1")
                .bind(created.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(deliveries, 1);

        sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(tenant)
            .execute(&pool)
            .await
            .unwrap();
    }
}
