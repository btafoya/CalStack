//! Durable job worker (docs/PRD.md section 17): polls the PostgreSQL queue,
//! leases jobs with SKIP LOCKED, executes idempotently, retries with backoff.
//!
//! `alarm_scan` reschedules itself each pass; reminders expand recurrence in
//! Rust (ADR-002) and create notifications keyed by dedupe keys, so a
//! re-scan never double-fires.

use calendar_core::DateOrDateTime;
use calendar_db::{self as db, alarms};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Poll loop; one in-process worker (docs/ARCHITECTURE.md).
pub async fn run_worker(
    pool: PgPool,
    worker_id: String,
    crypto: Option<std::sync::Arc<calendar_auth::Crypto>>,
) {
    // Seed the alarm scan if no scan job is pending (first boot / after purge).
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM durable_jobs
            WHERE job_type = 'alarm_scan' AND completed_at IS NULL AND failed_at IS NULL
        )",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(false);
    if !pending {
        schedule_alarm_scan(&pool, Utc::now()).await.ok();
    }
    let purge_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM durable_jobs
            WHERE job_type = 'retention_purge' AND completed_at IS NULL AND failed_at IS NULL
        )",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(false);
    if !purge_pending {
        db::jobs::enqueue(
            &pool,
            "retention_purge",
            serde_json::json!({}),
            Some(Utc::now()),
            0,
        )
        .await
        .ok();
    }
    let sync_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM durable_jobs
            WHERE job_type = 'ics_sync' AND completed_at IS NULL AND failed_at IS NULL
        )",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(false);
    if !sync_pending {
        db::jobs::enqueue(
            &pool,
            "ics_sync",
            serde_json::json!({}),
            Some(Utc::now()),
            0,
        )
        .await
        .ok();
    }

    let retention_days = std::env::var("RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    loop {
        match db::jobs::lease_next(&pool, &worker_id, 60).await {
            Ok(Some(job)) => {
                let result = execute(&pool, &job, retention_days, crypto.as_deref()).await;
                match result {
                    Ok(()) => {
                        db::jobs::complete(&pool, job.id).await.ok();
                    }
                    Err(err) => {
                        tracing::warn!(job_type = %job.job_type, error = %err, "job failed");
                        db::jobs::fail(&pool, job.id, &err).await.ok();
                    }
                }
            }
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
            Err(e) => {
                tracing::warn!(error = %e, "job queue poll failed");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

async fn execute(
    pool: &sqlx::PgPool,
    job: &db::jobs::JobRow,
    retention_days: i64,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), String> {
    match job.job_type.as_str() {
        "alarm_scan" => {
            alarm_scan(pool, crypto).await?;
            // Reschedule the scan and kick the send job.
            schedule_alarm_scan(pool, Utc::now() + Duration::minutes(1))
                .await
                .map_err(|e| e.to_string())?;
            db::jobs::enqueue(
                pool,
                "notify_send",
                serde_json::json!({}),
                Some(Utc::now()),
                0,
            )
            .await
            .ok();
            Ok(())
        }
        "notify_send" => {
            notify_send(pool, crypto).await?;
            // Recurring 1-minute tick.
            db::jobs::enqueue(
                pool,
                "notify_send",
                serde_json::json!({}),
                Some(Utc::now() + Duration::minutes(1)),
                0,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        }
        "imip_send" => {
            crate::scheduling::send_pending(pool, crypto).await;
            Ok(())
        }
        "webhook_send" => webhook_send(pool, job, crypto).await,
        "ics_sync" => {
            ics_sync(pool).await?;
            // Self-rescheduling pass over subscribed calendars.
            let interval = ics_sync_interval();
            db::jobs::enqueue(
                pool,
                "ics_sync",
                serde_json::json!({}),
                Some(Utc::now() + Duration::seconds(interval)),
                0,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        }
        "retention_purge" => {
            retention_purge(pool, retention_days).await?;
            // Daily sweep.
            db::jobs::enqueue(
                pool,
                "retention_purge",
                serde_json::json!({}),
                Some(Utc::now() + Duration::days(1)),
                0,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        }
        other => Err(format!("unknown job type: {other}")),
    }
}

/// Delivers one pending webhook delivery. Disabled/revoked webhooks and
/// purged events stop the delivery (recorded, job completes); a failing
/// target returns Err so db::jobs::fail retries with backoff until the
/// 5-attempt terminal, with the last outcome recorded on the delivery row.
async fn webhook_send(
    pool: &sqlx::PgPool,
    job: &db::jobs::JobRow,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), String> {
    let Some(delivery_id) = job
        .payload
        .get("delivery_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Err("webhook_send job payload missing delivery_id".into());
    };
    #[derive(sqlx::FromRow)]
    struct Delivery {
        id: Uuid,
        webhook_id: Uuid,
        payload: Value,
    }
    // The delivery row is gone (webhook deleted cascades nothing — soft
    // delete keeps it — but a hard DB delete may): nothing to deliver.
    let Some(delivery) = sqlx::query_as::<_, Delivery>(
        "SELECT id, webhook_id, payload FROM webhook_deliveries WHERE id = $1",
    )
    .bind(delivery_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    // Send-time re-check: disabling or revoking mid-retry must stop further
    // attempts. NotFound here covers disabled, soft-deleted, and gone rows.
    let webhook = match db::webhooks::get_webhook(pool, delivery.webhook_id).await {
        Ok(webhook) if webhook.enabled => webhook,
        _ => {
            db::webhooks::record_delivery_result(
                pool,
                delivery.id,
                "failed",
                None,
                Some("webhook disabled or deleted before delivery"),
                job.attempts + 1,
            )
            .await
            .map_err(|e| e.to_string())?;
            return Ok(());
        }
    };
    let event_id = delivery
        .payload
        .get("event_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or("delivery payload missing event_id")?;
    let trigger = delivery
        .payload
        .get("trigger")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    // A deleted event still delivers (trigger event_deleted); a purged one
    // has nothing left to describe.
    let Some(event) = db::webhooks::get_event_for_delivery(pool, event_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        db::webhooks::record_delivery_result(
            pool,
            delivery.id,
            "failed",
            None,
            Some("event no longer exists"),
            job.attempts + 1,
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let attendees = db::list_attendees(pool, event_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut view = db::webhooks::event_view(&event);
    view["attendees"] = attendees_json(&attendees);
    send_delivery_once(
        pool,
        &webhook,
        view,
        &trigger,
        delivery.id,
        job.attempts + 1,
        crypto,
    )
    .await
}

/// Attendee summaries for the envelope.
fn attendees_json(attendees: &[db::AttendeeRow]) -> serde_json::Value {
    serde_json::json!(
        attendees
            .iter()
            .map(|a| serde_json::json!({
                "email": a.email,
                "display_name": a.display_name,
                "telephone": a.telephone,
                "role": a.role,
                "partstat": a.partstat,
            }))
            .collect::<Vec<_>>()
    )
}

/// Signs and POSTs one delivery (the pre-built compact event view), records
/// the outcome, and fails the job (via Err) on a non-2xx so the durable
/// queue's backoff retries — same 5-attempt terminal semantics as
/// notify_send. The sign key is decrypted only here and never logged; errors
/// carry the HTTP outcome, not the URL.
pub(crate) async fn send_delivery_once(
    pool: &sqlx::PgPool,
    webhook: &db::webhooks::WebhookRow,
    event_view: serde_json::Value,
    trigger: &str,
    delivery_id: Uuid,
    attempt: i32,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), String> {
    let event_id: Uuid = event_view["id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or("event view missing id")?;
    let payload =
        db::webhooks::payload_json(delivery_id, webhook.id, event_id, trigger, event_view);
    let body = payload.to_string();
    let started = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client
        .post(&webhook.url)
        .header("content-type", "application/json")
        .header(
            "user-agent",
            concat!("CalStack/", env!("CARGO_PKG_VERSION")),
        )
        .body(body.clone());
    if let (Some(secret), Some(crypto)) = (&webhook.secret_encrypted, crypto)
        && let Ok(key) = crypto.decrypt(secret)
    {
        request = request.header(
            "X-CalStack-Signature",
            db::webhooks::sign(body.as_bytes(), &key),
        );
    }
    let result = request.send().await;
    // HTTP status for 2xx judgment; transport errors strip the target URL
    // (it may carry credentials) before being recorded.
    let (outcome, transport_error) = match result {
        Ok(response) => {
            let code = response.status().as_u16() as i32;
            let _ = response.text().await; // drain the connection
            (Some(code), None)
        }
        Err(e) => (None, Some(e.without_url().to_string())),
    };
    let elapsed = started.elapsed().as_millis() as u64;
    let succeeded = outcome.is_some_and(|code| (200..300).contains(&code));
    // 5 attempts terminal, matching durable_jobs.max_attempts and notify_send.
    let status = if succeeded {
        "succeeded"
    } else if attempt >= 5 {
        "exhausted"
    } else {
        "pending"
    };
    let error = if succeeded {
        None
    } else {
        Some(match (outcome, transport_error) {
            (Some(code), _) => format!("{elapsed}ms: HTTP {code}"),
            (None, Some(detail)) => format!("{elapsed}ms: {detail}"),
            (None, None) => format!("{elapsed}ms: delivery failed"),
        })
    };
    db::webhooks::record_delivery_result(
        pool,
        delivery_id,
        status,
        outcome,
        error.as_deref(),
        attempt,
    )
    .await
    .map_err(|e| e.to_string())?;
    if succeeded {
        Ok(())
    } else {
        Err(error.unwrap_or_default())
    }
}

/// Soft-deleted resources, expired auth rows, stale journal entries and
/// finished job history go once retention passes (docs/PRD.md section 21).
async fn retention_purge(pool: &sqlx::PgPool, days: i64) -> Result<(), String> {
    sqlx::query(&format!(
        "DELETE FROM events WHERE deleted_at < now() - interval '{days} days'"
    ))
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    calendar_db::tasks::purge_deleted_tasks_journals(pool, days)
        .await
        .map_err(|e| e.to_string())?;
    // Audit rows keep their own retention (they outlive operational data).
    let audit_days: i64 = std::env::var("AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    // Old change_log rows expire sync tokens: clients that page in past the
    // purge point get a sync-token mismatch and must resync — RFC 6578
    // permits a 410 response for that.
    for query in [
        format!(
            "DELETE FROM change_log WHERE changed_at < now() - interval '{days} days'"
        ),
        format!(
            "DELETE FROM rule_executions WHERE created_at < now() - interval '{days} days'"
        ),
        format!(
            "DELETE FROM audit_log WHERE created_at < now() - interval '{audit_days} days'"
        ),
        // Completed and failed job rows are run history, not work; keep a
        // week for debugging.
        "DELETE FROM durable_jobs
         WHERE (completed_at IS NOT NULL OR failed_at IS NOT NULL)
           AND COALESCE(completed_at, failed_at) < now() - interval '7 days'"
            .to_string(),
        "DELETE FROM sessions WHERE expires_at < now() - interval '7 days'".to_string(),
        "DELETE FROM notifications WHERE read_at IS NOT NULL AND created_at < now() - interval '30 days'"
            .to_string(),
    ] {
        sqlx::query(&query)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn schedule_alarm_scan(
    pool: &sqlx::PgPool,
    run_at: DateTime<Utc>,
) -> Result<Uuid, db::DbError> {
    db::jobs::enqueue(pool, "alarm_scan", serde_json::json!({}), Some(run_at), 0).await
}

fn ics_sync_interval() -> i64 {
    std::env::var("ICS_SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

fn import_max_bytes() -> i64 {
    std::env::var("IMPORT_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024)
}

// ============ ics_sync: outbound .ics subscriptions (PRD section 5) ============
//
// One pass re-fetches every calendar with a source_url. The remote is
// authoritative: series replace by UID (put_series), masters absent from the
// refetch soft-delete (§21 retention purges later). A failed fetch keeps the
// last good data; the pass loop is the retry.

async fn ics_sync(pool: &PgPool) -> Result<(), String> {
    // Redirects are not followed: a 30x could aim the fetch at an internal
    // address the resolver check never saw.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let calendars = db::subscribed_calendars(pool)
        .await
        .map_err(|e| e.to_string())?;
    for cal in calendars {
        if let Err(e) = sync_calendar(pool, &client, &cal).await {
            tracing::warn!(calendar = %cal.id, error = %e, "ics_sync pass failed");
        }
    }
    Ok(())
}

/// True when an address must not be fetched as a subscription source:
/// loopback, link-local, private ranges, unspecified. ponytail: resolved-IP
/// check only (no TOCTOU DNS pinning); upgrade to a proxy or custom connector
/// if a deployment actually needs one.
fn ip_allowed(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast())
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

/// One calendar's fetch-and-replace pass.
async fn sync_calendar(
    pool: &PgPool,
    client: &reqwest::Client,
    cal: &db::CalendarRow,
) -> Result<(), String> {
    let source = cal.source_url.as_deref().unwrap_or_default();
    let url = reqwest::Url::parse(source).map_err(|_| format!("invalid source URL {source:?}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("source URL must be http or https".into());
    }
    // Resolve and vet the target before connecting (no redirects followed).
    let host = url.host_str().ok_or("source URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("resolving {host}: {e}"))?
        .collect();
    let Some(_) = addrs.first() else {
        return Err(format!("remote host {host} did not resolve"));
    };
    if let Some(blocked) = addrs.iter().map(|a| a.ip()).find(|ip| !ip_allowed(*ip)) {
        return Err(format!(
            "remote host resolves to a blocked address ({blocked})"
        ));
    }
    let mut request = client.get(url).header("accept", "text/calendar").header(
        "user-agent",
        concat!("CalStack/", env!("CARGO_PKG_VERSION")),
    );
    if let Some(etag) = &cal.source_etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        // Nothing changed; just mark the pass.
        return db::set_calendar_sync_state(pool, cal.id, None)
            .await
            .map_err(|e| e.to_string());
    }
    if !response.status().is_success() {
        return Err(format!("remote returned {}", response.status()));
    }
    if let Some(len) = response.content_length()
        && len as i64 > import_max_bytes()
    {
        return Err("remote body over the size cap".into());
    }
    let validator = response
        .headers()
        .get(reqwest::header::ETAG)
        .or_else(|| response.headers().get(reqwest::header::LAST_MODIFIED))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if bytes.len() as i64 > import_max_bytes() {
        return Err("remote body over the size cap".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    let parsed = calendar_caldav::parse_calendar(&text)
        .map_err(|e| format!("remote body is not valid iCalendar: {e}"))?;
    let zones: Vec<db::timezones::NewTimezone> = parsed
        .timezones
        .iter()
        .map(|tz| db::timezones::NewTimezone {
            tzid: tz.tzid.clone(),
            definition: tz.definition.clone(),
            rules: tz.rules.clone(),
        })
        .collect();
    // One master plus its overrides per UID; href stays stable per UID so
    // updates match instead of colliding. Organizer fallback is the calendar
    // owner (remote ICS usually carries its own ORGANIZER; upsert_for keeps it).
    let owner: db::UserRow = match cal.created_by {
        Some(uid) => db::find_user_by_id(pool, uid).await.unwrap_or_else(|_| {
            tracing::warn!(calendar = %cal.id, "subscribed calendar owner is gone; events without ORGANIZER get a placeholder");
            db::UserRow {
                id: Uuid::nil(),
                username: String::new(),
                email: "sync-unknown@calstack.invalid".into(),
                display_name: None,
                password_hash: None,
                is_admin: false,
                timezone: None,
                notify_email: false,
                notify_sms: false,
                notify_push: false,
                disabled_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        }),
        None => {
            tracing::warn!(calendar = %cal.id, "subscribed calendar has no owner; events without ORGANIZER get a placeholder");
            db::UserRow {
                id: Uuid::nil(),
                username: String::new(),
                email: "sync-unknown@calstack.invalid".into(),
                display_name: None,
                password_hash: None,
                is_admin: false,
                timezone: None,
                notify_email: false,
                notify_sms: false,
                notify_push: false,
                disabled_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        }
    };
    // Group by UID, in one pass, preserving first-seen order.
    struct Group<'a> {
        master: Option<&'a calendar_caldav::ParsedEvent>,
        overrides: Vec<&'a calendar_caldav::ParsedEvent>,
    }
    let mut groups: std::collections::HashMap<String, Group<'_>> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for event in &parsed.events {
        match groups.get_mut(&event.uid) {
            Some(group) => {
                if event.recurrence_id.is_none() && event.recurrence_id_date.is_none() {
                    group.master = Some(event);
                } else {
                    group.overrides.push(event);
                }
            }
            None => {
                order.push(event.uid.clone());
                let mut group = Group {
                    master: None,
                    overrides: Vec::new(),
                };
                if event.recurrence_id.is_none() && event.recurrence_id_date.is_none() {
                    group.master = Some(event);
                } else {
                    group.overrides.push(event);
                }
                groups.insert(event.uid.clone(), group);
            }
        }
    }
    let mut fetched_uids: Vec<String> = Vec::new();
    for uid in order {
        let group = &groups[&uid];
        let href: Option<String> = sqlx::query_scalar(
            "SELECT COALESCE(href, id::text || '.ics') FROM events
             WHERE calendar_id = $1 AND uid = $2 AND master_event_id IS NULL
             LIMIT 1",
        )
        .bind(cal.id)
        .bind(&uid)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
        let href = href.unwrap_or_else(|| format!("{}.ics", Uuid::new_v4()));
        let overrides: Vec<db::ics_upsert::IcsEventUpsert> = group
            .overrides
            .iter()
            .map(|e| calendar_caldav::upsert_for(&owner, e))
            .collect();
        let master_data =
            calendar_caldav::upsert_for(&owner, group.master.expect("master set above"));
        db::ics_upsert::put_series(
            pool,
            cal.id,
            cal.created_by.unwrap_or(cal.tenant_id),
            &href,
            &master_data,
            &overrides,
            &zones,
            &db::ics_upsert::PutPrecondition::None,
        )
        .await
        .map_err(|e| format!("storing UID {uid}: {e}"))?;
        fetched_uids.push(uid);
    }
    // Staleness sweep over the *fetched* set (not the written one): a UID
    // whose put failed keeps its stored rows and is not treated as stale.
    sqlx::query(
        "WITH stale AS (
            SELECT id FROM events
            WHERE calendar_id = $1 AND deleted_at IS NULL
              AND master_event_id IS NULL AND uid <> ALL($2)
         )
         UPDATE events e SET deleted_at = now()
         FROM stale s
         WHERE e.deleted_at IS NULL AND (e.id = s.id OR e.master_event_id = s.id)",
    )
    .bind(cal.id)
    .bind(&fetched_uids)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    db::set_calendar_sync_state(pool, cal.id, validator.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Finds alarms whose trigger falls in the window, creates deduped
/// notifications for the calendar's principals (DISPLAY) — EMAIL dispatch
/// joins when notification providers are configured (stage 16). Also prunes
/// pending reminder rows whose alarm no longer matches a live alarm
/// occurrence (deleted or edited).
async fn alarm_scan(
    pool: &sqlx::PgPool,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), String> {
    let now = Utc::now();
    let lookback = now - Duration::minutes(2); // cover a stalled worker
    let horizon = now + Duration::minutes(3);
    let rows = alarms::list_scannable_alarms(pool)
        .await
        .map_err(|e| e.to_string())?;
    prune_stale_pending(pool, &rows, now, horizon).await?;

    // Group alarm rows by subject so a recurring master expands once.
    let mut by_subject: std::collections::HashMap<(alarms::ScanKind, Uuid), Vec<&alarms::ScanRow>> =
        std::collections::HashMap::new();
    for row in &rows {
        by_subject
            .entry((row.kind, row.subject_id))
            .or_default()
            .push(row);
    }

    // Calendar principals for notification addressing.
    let mut principals: std::collections::HashMap<Uuid, Vec<Principal>> =
        std::collections::HashMap::new();
    let mut tenants: std::collections::HashMap<Uuid, Option<Uuid>> =
        std::collections::HashMap::new();
    let mut providers: std::collections::HashMap<Uuid, TenantProviders> =
        std::collections::HashMap::new();
    // Per-calendar stored VTIMEZONEs (ADR-012), cached like the other lookups.
    let mut resolvers: std::collections::HashMap<Uuid, calendar_core::recurrence::TzResolver> =
        std::collections::HashMap::new();

    for ((kind, subject_id), group) in &by_subject {
        for scan in group {
            let resolver = match resolvers.get(&scan.calendar_id) {
                Some(resolver) => resolver.clone(),
                None => {
                    let resolver = db::timezones::load_for_calendar(pool, scan.calendar_id)
                        .await
                        .unwrap_or_default();
                    resolvers.insert(scan.calendar_id, resolver.clone());
                    resolver
                }
            };
            for trigger in alarm_triggers(scan, &resolver, lookback, horizon) {
                if trigger < lookback || trigger > horizon {
                    continue;
                }
                // Address to every principal of the calendar (cached).
                let users = match principals.get(&scan.calendar_id) {
                    Some(users) => users.clone(),
                    None => {
                        let users: Vec<Principal> = sqlx::query_as(
                            "SELECT u.id, u.email, u.notify_email, u.notify_push
                             FROM calendar_acl acl JOIN users u ON u.id = acl.principal_user_id
                             WHERE acl.calendar_id = $1",
                        )
                        .bind(scan.calendar_id)
                        .fetch_all(pool)
                        .await
                        .unwrap_or_default();
                        principals.insert(scan.calendar_id, users.clone());
                        users
                    }
                };
                // task_due rules + webhooks fire exactly once per (task,
                // due instant): the deduped marker row is the tombstone.
                if scan.kind == alarms::ScanKind::Task {
                    let marker = format!("task_due:{}:{}", scan.subject_id, trigger.to_rfc3339());
                    let first = alarms::create_notification_deduped(
                        pool, None, "in_app", None, None, None, &marker,
                    )
                    .await
                    .unwrap_or(false);
                    if first && let Ok(cal) = db::get_calendar(pool, scan.calendar_id).await {
                        crate::rules_api::run_rules(
                            pool,
                            cal.tenant_id,
                            cal.id,
                            "task_due",
                            scan.subject_id,
                            serde_json::json!({
                                "summary": scan.summary,
                                "due": trigger.to_rfc3339(),
                                "kind": "task",
                            }),
                            crypto,
                        )
                        .await;
                        crate::webhooks_api::fire(pool, cal.tenant_id, scan.subject_id, "task_due")
                            .await;
                    }
                }
                let dedupe = format!("alarm:{}:{}", scan.alarm.id, trigger.to_rfc3339());
                let title = scan
                    .alarm
                    .summary
                    .clone()
                    .unwrap_or_else(|| scan.summary.clone());
                let body = scan
                    .alarm
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Reminder: {}", scan.summary));
                let channels = &scan.alarm.notify_channels;
                let want_email = channels.iter().any(|c| c == "email");
                let want_sms = channels.iter().any(|c| c == "sms");
                let want_push = channels.iter().any(|c| c == "push");
                let extra_channels = want_email || want_sms || want_push;

                for user in &users {
                    // Idempotent via the dedupe key; restarts and re-scans
                    // never duplicate a fired reminder.
                    alarms::create_notification_deduped(
                        pool,
                        Some(user.id),
                        "in_app",
                        Some(&title),
                        Some(&body),
                        Some(subject_data(scan, trigger)),
                        &format!("{dedupe}:{}", user.id),
                    )
                    .await
                    .ok();
                }

                if !extra_channels {
                    continue;
                }
                let tenant_id = match tenants.get(&scan.calendar_id) {
                    Some(id) => *id,
                    None => {
                        let id: Option<Uuid> =
                            sqlx::query_scalar("SELECT tenant_id FROM calendars WHERE id = $1")
                                .bind(scan.calendar_id)
                                .fetch_one(pool)
                                .await
                                .ok()
                                .flatten();
                        tenants.insert(scan.calendar_id, id);
                        id
                    }
                };
                let Some(tenant_id) = tenant_id else { continue };
                let p = providers.entry(tenant_id).or_default();
                if want_email && !p.email {
                    p.email = crate::scheduling::load_email_provider(pool, Some(tenant_id), crypto)
                        .await
                        .is_some();
                }
                if want_sms && !p.sms {
                    p.sms = crate::rules_api::load_sms_provider(pool, tenant_id, crypto)
                        .await
                        .is_some();
                }
                if want_push && !p.webpush {
                    p.webpush = load_webpush_provider(pool, tenant_id, crypto)
                        .await
                        .is_some();
                }
                let time_text = format_scan_time(scan);
                let url = std::env::var("APP_PUBLIC_URL")
                    .ok()
                    .filter(|u| !u.is_empty());

                // Email: principals' login addresses (opt-out respected) ∪
                // attendee emails ∪ the alarm's explicit recipients.
                if want_email && p.email {
                    let mut emails = Vec::new();
                    for user in &users {
                        if user.notify_email {
                            emails.push((Some(user.id), user.email.clone()));
                        }
                    }
                    for email in attendee_emails(pool, *kind, *subject_id).await {
                        emails.push((None, email));
                    }
                    for email in &scan.alarm.recipient_emails {
                        emails.push((None, email.clone()));
                    }
                    let mut seen = std::collections::HashSet::new();
                    for (user_id, email) in emails {
                        let key = email.to_lowercase();
                        if !seen.insert(key.clone()) {
                            continue;
                        }
                        let mut text = format!(
                            "{}\n{}\n{}",
                            title,
                            body,
                            time_text.as_deref().unwrap_or_default()
                        );
                        if let Some(u) = &url {
                            text.push('\n');
                            text.push_str(u);
                        }
                        let mut data = subject_data(scan, trigger);
                        data["tenant_id"] = serde_json::json!(tenant_id);
                        data["recipient"] = serde_json::json!(email);
                        alarms::create_notification_deduped(
                            pool,
                            user_id,
                            "email",
                            Some(&title),
                            Some(&text),
                            Some(data),
                            &format!("{dedupe}:email:{key}"),
                        )
                        .await
                        .ok();
                    }
                }

                // SMS: this subject's attendees whose linked contact has a
                // mobile number, plus SMS-only attendees.
                if want_sms && p.sms {
                    for phone in sms_recipients(pool, *kind, *subject_id).await {
                        let text =
                            format!("{} - {}", title, time_text.as_deref().unwrap_or_default());
                        let mut data = subject_data(scan, trigger);
                        data["tenant_id"] = serde_json::json!(tenant_id);
                        data["recipient"] = serde_json::json!(phone);
                        alarms::create_notification_deduped(
                            pool,
                            None,
                            "sms",
                            Some(&title),
                            Some(&text),
                            Some(data),
                            &format!("{dedupe}:sms:{phone}"),
                        )
                        .await
                        .ok();
                    }
                }

                // Push: principals with active subscriptions (opt-out respected).
                if want_push && p.webpush {
                    for user in users.iter().filter(|u| u.notify_push) {
                        let mut data = subject_data(scan, trigger);
                        data["tenant_id"] = serde_json::json!(tenant_id);
                        alarms::create_notification_deduped(
                            pool,
                            Some(user.id),
                            "push",
                            Some(&title),
                            Some(&body),
                            Some(data),
                            &format!("{dedupe}:push:{}", user.id),
                        )
                        .await
                        .ok();
                    }
                }
            }
        }
    }
    Ok(())
}

/// Alarm trigger instants for one scan row. Absolute triggers pass through;
/// relative triggers anchor on the subject's own start/end — exception/task
/// overrides use their own times, all-day subjects start at midnight in their
/// timezone, floating ones as if UTC — and recurring subjects expand within
/// [lookback, horizon), skipping completed occurrences of recurring tasks.
/// `resolver` carries the calendar's stored VTIMEZONEs (ADR-012); tzdb ids
/// resolve without it.
fn alarm_triggers(
    scan: &alarms::ScanRow,
    resolver: &calendar_core::recurrence::TzResolver,
    lookback: DateTime<Utc>,
    horizon: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    if let Some(at) = scan.alarm.trigger_at {
        return vec![at];
    }
    let Some(offset) = scan.alarm.offset_secs() else {
        return vec![];
    };
    let related_end = scan.alarm.related.as_deref() == Some("END");
    let base = if related_end { scan.end } else { scan.start };
    let Some(base) = base else {
        // Relative alarms on subjects with no anchor (a task without
        // DTSTART/DUE) cannot fire; absolute ones already returned above.
        return vec![];
    };
    let expanded = if scan.rrule.is_some() {
        calendar_core::recurrence::expand_occurrences(
            base,
            scan.tzid.as_deref(),
            Some(resolver),
            scan.rrule.as_deref(),
            &parse_points(&scan.rdate),
            &parse_points(&scan.exdate),
            lookback,
            horizon,
        )
        .unwrap_or_default()
    } else {
        vec![base]
    };
    expanded
        .into_iter()
        .filter(|p| !is_completed_occurrence(scan, *p, resolver))
        .filter_map(|p| match p {
            DateOrDateTime::Timed(at) => Some(at),
            DateOrDateTime::AllDay(date) => day_start_instant(date, scan.tzid.as_deref(), resolver),
        })
        .map(|at| at + Duration::seconds(offset))
        .collect()
}

/// True when `point` is a completed occurrence of a recurring task (design
/// §6). Compared in wall-clock space, like `next_open`: overrides store the
/// local time or the date.
fn is_completed_occurrence(
    scan: &alarms::ScanRow,
    point: DateOrDateTime,
    resolver: &calendar_core::recurrence::TzResolver,
) -> bool {
    if scan.completed_occurrences.is_empty() {
        return false;
    }
    let key = match point {
        DateOrDateTime::Timed(at) => {
            let Ok(zone) =
                calendar_core::recurrence::resolve_tz(scan.tzid.as_deref(), Some(resolver))
            else {
                return false;
            };
            db::tasks::Occurrence::Timed(zone.to_local(at))
        }
        DateOrDateTime::AllDay(date) => db::tasks::Occurrence::AllDay(date),
    };
    scan.completed_occurrences.contains(&key)
}

/// The notification `data` envelope: the subject id under the key the
/// dispatch path expects (`event_id` or `task_id`), plus the shared fields.
fn subject_data(scan: &alarms::ScanRow, trigger: DateTime<Utc>) -> serde_json::Value {
    let mut data = serde_json::json!({
        "alarm_id": scan.alarm.id,
        "trigger_at": trigger,
        "action": scan.alarm.action,
    });
    match scan.kind {
        alarms::ScanKind::Event => data["event_id"] = serde_json::json!(scan.subject_id),
        alarms::ScanKind::Task => data["task_id"] = serde_json::json!(scan.subject_id),
    }
    data
}

/// Midnight of an all-day occurrence as an instant: wall clock in the event's
/// timezone; floating events (no tzid) store wall clock as if UTC.
fn day_start_instant(
    date: NaiveDate,
    tzid: Option<&str>,
    resolver: &calendar_core::recurrence::TzResolver,
) -> Option<DateTime<Utc>> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    calendar_core::recurrence::resolve_tz(tzid, Some(resolver))
        .ok()?
        .from_local(naive)
}

/// Deletes pending (not yet dispatched) reminder rows whose alarm or trigger
/// no longer matches a live alarm occurrence: the event was deleted, or was
/// edited (ics_upsert regenerates alarm ids on edit; an API time edit moves
/// the trigger) — rows the next scan re-creates under the new dedupe key are
/// unaffected. Rows that already dispatched are history, not pending, and
/// stay.
async fn prune_stale_pending(
    pool: &sqlx::PgPool,
    live: &[alarms::ScanRow],
    now: DateTime<Utc>,
    horizon: DateTime<Utc>,
) -> Result<(), String> {
    #[derive(sqlx::FromRow)]
    struct Pending {
        id: Uuid,
        alarm_id: Option<Uuid>,
        trigger_at: Option<DateTime<Utc>>,
    }
    let pending: Vec<Pending> = sqlx::query_as(
        "SELECT id, (data->>'alarm_id')::uuid AS alarm_id,
                (data->>'trigger_at')::timestamptz AS trigger_at
         FROM notifications
         WHERE sent_at IS NULL AND channel IN ('email', 'sms', 'push')
           AND dedupe_key LIKE 'alarm:%'
           AND created_at > now() - interval '7 days'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(());
    }
    let live_by_id: std::collections::HashMap<Uuid, &alarms::ScanRow> =
        live.iter().map(|r| (r.alarm.id, r)).collect();
    // ponytail: 24h-wide comparison window; rows older than that have aged
    // out of the send backoff long before, so widening further buys nothing.
    let lookback = now - Duration::hours(24);
    let mut stale: Vec<Uuid> = Vec::new();
    for row in pending {
        let stale_row = match row.alarm_id.as_ref().and_then(|id| live_by_id.get(id)) {
            // No live alarm with this id: deleted event, or edited so the
            // alarm set was regenerated.
            None => true,
            Some(scan) => match row.trigger_at {
                // Trigger moved (e.g. an API edit of the start time keeps the
                // alarm rows): drop rows whose old trigger no longer matches.
                Some(at) => {
                    let resolver = db::timezones::load_for_calendar(pool, scan.calendar_id)
                        .await
                        .unwrap_or_default();
                    !alarm_triggers(scan, &resolver, lookback, horizon).contains(&at)
                }
                // Rows written before trigger_at existed age out via send
                // backoff; only the alarm-identity check applies.
                None => false,
            },
        };
        if stale_row {
            stale.push(row.id);
        }
    }
    for id in stale {
        sqlx::query("DELETE FROM notifications WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Sends pending reminder rows (email/sms/push), bounded exponential backoff
/// on retries, deduped in-app notice when retries are exhausted. Provider
/// config comes from the notification's tenant (carried in `data`).
async fn notify_send(
    pool: &sqlx::PgPool,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), String> {
    // Backoff = 2^attempts minutes since creation; 5 attempts then give up.
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        user_id: Option<Uuid>,
        channel: String,
        title: Option<String>,
        body: Option<String>,
        data: Value,
    }
    let pending: Vec<Row> = sqlx::query_as(
        "SELECT id, user_id, channel, title, body, data FROM notifications
         WHERE channel IN ('email', 'sms', 'push') AND sent_at IS NULL
           AND send_attempts < 5
           AND created_at < now() - (interval '1 minute' * pow(2, send_attempts::double precision))
         ORDER BY created_at LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for row in pending {
        // Crash-recovery claim: a row mid-send when the process died must not
        // re-send until the lease expires. Single worker per DB by design —
        // the lease closes the crash-between-send-and-sent_at window, not
        // multi-instance fan-out.
        if !alarms::claim_notification(pool, row.id)
            .await
            .map_err(|e| e.to_string())?
        {
            continue; // a live claim holds the row (crashed worker lease)
        }
        let recipient = row
            .data
            .get("recipient")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tenant_id = row
            .data
            .get("tenant_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or(Uuid::nil());
        let result = match row.channel.as_str() {
            "email" => {
                // A missing provider is an error like any other: it counts
                // against the retry budget instead of retrying forever.
                match crate::scheduling::load_email_provider(pool, Some(tenant_id), crypto).await {
                    Some(provider) => {
                        provider
                            .send(
                                &recipient,
                                row.title.as_deref().unwrap_or("Reminder"),
                                row.body.as_deref().unwrap_or_default(),
                            )
                            .await
                    }
                    None => Err(calendar_notify::NotifyError::Config(
                        "no email provider".into(),
                    )),
                }
            }
            "sms" => match crate::rules_api::load_sms_provider(pool, tenant_id, crypto).await {
                Some(provider) => {
                    provider
                        .send(&recipient, row.body.as_deref().unwrap_or_default())
                        .await
                }
                None => Err(calendar_notify::NotifyError::Config(
                    "no sms provider".into(),
                )),
            },
            "push" => {
                send_push(
                    pool,
                    tenant_id,
                    crypto,
                    row.user_id,
                    row.title.clone(),
                    row.body.clone(),
                    row.data.clone(),
                )
                .await
            }
            _ => continue,
        };
        match result {
            Ok(()) => {
                sqlx::query(
                    "UPDATE notifications
                     SET sent_at = now(), send_error = NULL, claimed_until = NULL WHERE id = $1",
                )
                .bind(row.id)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                let attempts: i32 = sqlx::query_scalar(
                    "UPDATE notifications
                     SET send_attempts = send_attempts + 1, send_error = $2, claimed_until = NULL
                     WHERE id = $1
                     RETURNING send_attempts",
                )
                .bind(row.id)
                .bind(e.to_string())
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string())?;
                if attempts >= 5 {
                    // Permanent failure: one deduped in-app notice.
                    alarms::create_notification_deduped(
                        pool,
                        row.user_id,
                        "in_app",
                        Some("Reminder delivery failed"),
                        Some(&format!(
                            "{} ({}): {}",
                            row.title.as_deref().unwrap_or_default(),
                            row.channel,
                            e
                        )),
                        None,
                        &format!("alarm-failed:{}", row.id),
                    )
                    .await
                    .ok();
                }
            }
        }
    }
    Ok(())
}

/// Sends a push row to the user's subscriptions; drops endpoints the push
/// service reports gone. Ok(()) when any subscription accepted the message.
async fn send_push(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    crypto: Option<&calendar_auth::Crypto>,
    user_id: Option<Uuid>,
    title: Option<String>,
    body: Option<String>,
    data: Value,
) -> Result<(), calendar_notify::NotifyError> {
    let Some(user_id) = user_id else {
        return Err(calendar_notify::NotifyError::Config(
            "push row without user".into(),
        ));
    };
    let Some(provider) = load_webpush_provider(pool, tenant_id, crypto).await else {
        return Err(calendar_notify::NotifyError::Config(
            "no webpush provider".into(),
        ));
    };
    let payload = serde_json::json!({
        "title": title.unwrap_or_default(),
        "body": body.unwrap_or_default(),
        "url": std::env::var("APP_PUBLIC_URL").unwrap_or_default(),
    });
    // The service worker does `event.data.json()`: send raw JSON bytes, not a
    // base64 string of them.
    let raw = payload.to_string();
    let _ = &data;
    let subs: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, endpoint, p256dh, auth FROM push_subscriptions WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if subs.is_empty() {
        return Err(calendar_notify::NotifyError::Config(
            "no push subscriptions".into(),
        ));
    }
    let mut delivered = false;
    for (id, endpoint, p256dh, auth) in subs {
        match provider
            .send(&endpoint, &p256dh, &auth, raw.as_bytes())
            .await
        {
            Ok(gone) => {
                if gone {
                    sqlx::query("DELETE FROM push_subscriptions WHERE id = $1")
                        .bind(id)
                        .execute(pool)
                        .await
                        .ok();
                } else {
                    delivered = true;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "push send failed");
            }
        }
    }
    if delivered {
        Ok(())
    } else {
        Err(calendar_notify::NotifyError::Send(
            "no push subscription accepted the message".into(),
        ))
    }
}

/// Calendar principal with the fields reminder dispatch needs.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Principal {
    id: Uuid,
    email: String,
    notify_email: bool,
    notify_push: bool,
}

/// Per-tenant provider existence, cached per scan pass (the send job
/// re-resolves and reloads providers itself).
#[derive(Debug, Default)]
struct TenantProviders {
    email: bool,
    sms: bool,
    webpush: bool,
}

/// Subject start in its own timezone, for message bodies. Tasks fall back to
/// their due time when they carry no DTSTART.
fn format_scan_time(scan: &alarms::ScanRow) -> Option<String> {
    let at = match scan.start {
        Some(DateOrDateTime::Timed(at)) => at,
        _ => return None,
    };
    Some(
        match scan
            .tzid
            .as_deref()
            .and_then(|t| t.parse::<chrono_tz::Tz>().ok())
        {
            Some(tz) => at
                .with_timezone(&tz)
                .format("%a %b %d %H:%M (%Z)")
                .to_string(),
            None => at.format("%a %b %d %H:%M UTC").to_string(),
        },
    )
}

/// Distinct attendee emails for one event or task.
async fn attendee_emails(
    pool: &sqlx::PgPool,
    kind: alarms::ScanKind,
    subject_id: Uuid,
) -> Vec<String> {
    match kind {
        alarms::ScanKind::Event => sqlx::query_scalar(
            "SELECT DISTINCT email::text FROM event_attendees
                 WHERE event_id = $1 AND email IS NOT NULL",
        )
        .bind(subject_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default(),
        alarms::ScanKind::Task => sqlx::query_scalar(
            "SELECT DISTINCT email::text FROM task_attendees
                 WHERE task_id = $1 AND email IS NOT NULL",
        )
        .bind(subject_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default(),
    }
}

/// SMS recipients: attendees whose linked contact has a mobile tel, plus
/// SMS-only attendees (telephone set).
async fn sms_recipients(
    pool: &sqlx::PgPool,
    kind: alarms::ScanKind,
    subject_id: Uuid,
) -> Vec<String> {
    let (table, column) = match kind {
        alarms::ScanKind::Event => ("event_attendees", "event_id"),
        alarms::ScanKind::Task => ("task_attendees", "task_id"),
    };
    sqlx::query_scalar(&format!(
        "SELECT DISTINCT COALESCE(ct.number, a.telephone::text)
         FROM {table} a
         LEFT JOIN contacts c ON c.id = a.contact_id AND c.deleted_at IS NULL
         LEFT JOIN contact_tels ct ON ct.contact_id = c.id AND ct.is_mobile
         WHERE a.{column} = $1 AND (ct.number IS NOT NULL OR a.telephone IS NOT NULL)"
    ))
    .bind(subject_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// The tenant's enabled webpush provider, config decrypted.
async fn load_webpush_provider(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    crypto: Option<&calendar_auth::Crypto>,
) -> Option<calendar_notify::WebPushProvider> {
    let crypto = crypto?;
    #[derive(sqlx::FromRow)]
    struct Row {
        config_encrypted: Vec<u8>,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT config_encrypted FROM notification_providers
         WHERE tenant_id = $1 AND enabled AND kind = 'webpush' LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let config: Value =
        serde_json::from_slice(&crypto.decrypt(&row.config_encrypted).ok()?).ok()?;
    calendar_notify::WebPushProvider::from_config(&config).ok()
}

fn parse_points(value: &serde_json::Value) -> Vec<DateOrDateTime> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| {
                    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                        return Some(DateOrDateTime::Timed(at.with_timezone(&Utc)));
                    }
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .ok()
                        .map(DateOrDateTime::AllDay)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use calendar_db::EventRow;

    #[test]
    fn subscription_fetch_blocks_private_targets() {
        let p = |s: &str| ip_allowed(s.parse().unwrap());
        assert!(p("93.184.216.34")); // public
        assert!(p("2606:2800:220:1:248:1893:25c8:1946")); // public v6
        assert!(!p("127.0.0.1"));
        assert!(!p("10.1.2.3"));
        assert!(!p("172.16.0.9"));
        assert!(!p("192.168.1.1"));
        assert!(!p("169.254.1.1"));
        assert!(!p("0.0.0.0"));
        assert!(!p("::1"));
        assert!(!p("fe80::1"));
        assert!(!p("fd00::1"));
        assert!(!p("::"));
    }

    fn resolver() -> calendar_core::recurrence::TzResolver {
        calendar_core::recurrence::TzResolver::default()
    }

    fn timed(at: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn event(
        starts_at: Option<DateTime<Utc>>,
        start_date: Option<NaiveDate>,
        tzid: Option<&str>,
        rrule: Option<&str>,
        master_event_id: Option<Uuid>,
    ) -> EventRow {
        EventRow {
            id: Uuid::nil(),
            calendar_id: Uuid::nil(),
            uid: String::new(),
            href: None,
            master_event_id,
            recurrence_id: None,
            recurrence_id_date: None,
            is_exception: master_event_id.is_some(),
            starts_at,
            ends_at: None,
            start_date,
            end_date: None,
            duration: None,
            tzid: tzid.map(str::to_string),
            all_day: start_date.is_some(),
            floating: tzid.is_none(),
            rrule: rrule.map(str::to_string),
            rdate: serde_json::Value::Null,
            exdate: serde_json::Value::Null,
            summary: String::new(),
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
            created_by: None,
            organizer_email: String::new(),
            organizer_name: None,
            sequence: 0,
            etag: String::new(),
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn scan_row(event: EventRow, offset_secs: i64) -> alarms::ScanRow {
        let start = match (event.starts_at, event.start_date) {
            (Some(at), _) => Some(DateOrDateTime::Timed(at)),
            (None, Some(date)) => Some(DateOrDateTime::AllDay(date)),
            _ => None,
        };
        alarms::ScanRow {
            kind: alarms::ScanKind::Event,
            subject_id: event.id,
            calendar_id: Uuid::nil(),
            summary: event.summary.clone(),
            alarm: alarms::ScanAlarm {
                id: Uuid::nil(),
                action: "DISPLAY".to_string(),
                related: Some("START".to_string()),
                offset_interval: Some(sqlx::postgres::types::PgInterval {
                    months: 0,
                    days: 0,
                    microseconds: offset_secs * 1_000_000,
                }),
                trigger_at: None,
                description: None,
                summary: None,
                recipient_emails: Vec::new(),
                notify_channels: Vec::new(),
                created_at: Utc::now(),
            },
            start,
            end: None,
            rrule: event.rrule,
            rdate: event.rdate,
            exdate: event.exdate,
            tzid: event.tzid,
            floating: event.floating,
            completed_occurrences: Vec::new(),
        }
    }

    /// A task scan row; `start`/`end` come pre-anchored the way the union
    /// builds them (start = DTSTART-else-DUE, end = DUE).
    fn task_scan_row(
        start: Option<DateOrDateTime>,
        end: Option<DateOrDateTime>,
        related: &str,
        tzid: Option<&str>,
        rrule: Option<&str>,
        offset_secs: i64,
    ) -> alarms::ScanRow {
        alarms::ScanRow {
            kind: alarms::ScanKind::Task,
            subject_id: Uuid::nil(),
            calendar_id: Uuid::nil(),
            summary: "task".to_string(),
            alarm: alarms::ScanAlarm {
                id: Uuid::nil(),
                action: "DISPLAY".to_string(),
                related: Some(related.to_string()),
                offset_interval: Some(sqlx::postgres::types::PgInterval {
                    months: 0,
                    days: 0,
                    microseconds: offset_secs * 1_000_000,
                }),
                trigger_at: None,
                description: None,
                summary: None,
                recipient_emails: Vec::new(),
                notify_channels: Vec::new(),
                created_at: Utc::now(),
            },
            start,
            end,
            rrule: rrule.map(str::to_string),
            rdate: serde_json::Value::Null,
            exdate: serde_json::Value::Null,
            tzid: tzid.map(str::to_string),
            floating: tzid.is_none(),
            completed_occurrences: Vec::new(),
        }
    }

    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        (timed("2026-01-01T00:00:00Z"), timed("2026-03-01T00:00:00Z"))
    }

    #[test]
    fn exception_alarm_fires_at_the_override_time() {
        // A moved occurrence's exception row carries its own starts_at; the
        // master's original time must not be used.
        let scan = scan_row(
            event(
                Some(timed("2026-01-20T15:00:00Z")),
                None,
                None,
                None,
                Some(Uuid::nil()),
            ),
            -900,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-20T14:45:00Z")]
        );
    }

    #[test]
    fn all_day_alarm_anchors_at_local_midnight() {
        // 2026-01-15 midnight in New York (EST) is 05:00 UTC.
        let scan = scan_row(
            event(
                None,
                Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
                Some("America/New_York"),
                None,
                None,
            ),
            -900,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T04:45:00Z")]
        );
    }

    #[test]
    fn all_day_floating_alarm_anchors_at_utc_midnight() {
        // Floating wall clock is stored as if UTC.
        let scan = scan_row(
            event(
                None,
                Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
                None,
                None,
                None,
            ),
            0,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T00:00:00Z")]
        );
    }

    #[test]
    fn all_day_recurring_alarm_expands_to_midnights() {
        let scan = scan_row(
            event(
                None,
                Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
                None,
                Some("FREQ=DAILY;COUNT=3"),
                None,
            ),
            0,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![
                timed("2026-01-15T00:00:00Z"),
                timed("2026-01-16T00:00:00Z"),
                timed("2026-01-17T00:00:00Z"),
            ]
        );
    }

    #[test]
    fn master_recurring_alarm_still_expands_instant_occurrences() {
        let scan = scan_row(
            event(
                Some(timed("2026-01-15T10:00:00Z")),
                None,
                None,
                Some("FREQ=DAILY;COUNT=2"),
                None,
            ),
            -900,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T09:45:00Z"), timed("2026-01-16T09:45:00Z"),]
        );
    }

    #[test]
    fn absolute_trigger_passes_through() {
        let mut scan = scan_row(
            event(Some(timed("2026-01-15T10:00:00Z")), None, None, None, None),
            0,
        );
        scan.alarm.trigger_at = Some(timed("2026-02-01T08:00:00Z"));
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-02-01T08:00:00Z")]
        );
    }

    #[test]
    fn task_related_end_alarm_anchors_on_due() {
        let scan = task_scan_row(
            None,
            Some(DateOrDateTime::Timed(timed("2026-01-20T17:00:00Z"))),
            "END",
            None,
            None,
            -600,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-20T16:50:00Z")]
        );
    }

    #[test]
    fn task_with_no_anchor_skips_relative_alarm() {
        let scan = task_scan_row(None, None, "END", None, None, -600);
        let (lookback, horizon) = window();
        assert!(alarm_triggers(&scan, &resolver(), lookback, horizon).is_empty());
    }

    #[test]
    fn task_absolute_trigger_fires_without_anchor() {
        let mut scan = task_scan_row(None, None, "END", None, None, -600);
        scan.alarm.trigger_at = Some(timed("2026-02-01T08:00:00Z"));
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-02-01T08:00:00Z")]
        );
    }

    #[test]
    fn all_day_task_due_anchors_at_local_midnight() {
        // 2026-01-15 midnight in New York (EST) is 05:00 UTC.
        let scan = task_scan_row(
            None,
            Some(DateOrDateTime::AllDay(
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            )),
            "END",
            Some("America/New_York"),
            None,
            0,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T05:00:00Z")]
        );
    }

    #[test]
    fn floating_task_due_anchors_at_utc_midnight() {
        let scan = task_scan_row(
            None,
            Some(DateOrDateTime::AllDay(
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            )),
            "END",
            None,
            None,
            0,
        );
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T00:00:00Z")]
        );
    }

    #[test]
    fn recurring_task_alarm_skips_completed_occurrences() {
        let mut scan = task_scan_row(
            Some(DateOrDateTime::Timed(timed("2026-01-15T10:00:00Z"))),
            None,
            "START",
            None,
            Some("FREQ=DAILY;COUNT=3"),
            -900,
        );
        scan.completed_occurrences = vec![db::tasks::Occurrence::Timed(
            timed("2026-01-16T10:00:00Z").naive_utc(),
        )];
        let (lookback, horizon) = window();
        assert_eq!(
            alarm_triggers(&scan, &resolver(), lookback, horizon),
            vec![timed("2026-01-15T09:45:00Z"), timed("2026-01-17T09:45:00Z"),]
        );
    }
}
