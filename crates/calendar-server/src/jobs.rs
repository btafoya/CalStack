//! Durable job worker (docs/PRD.md section 17): polls the PostgreSQL queue,
//! leases jobs with SKIP LOCKED, executes idempotently, retries with backoff.
//!
//! `alarm_scan` reschedules itself each pass; reminders expand recurrence in
//! Rust (ADR-002) and create notifications keyed by dedupe keys, so a
//! re-scan never double-fires.

use calendar_core::DateOrDateTime;
use calendar_db::{self as db, alarms};
use chrono::{DateTime, Duration, Utc};
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

/// Soft-deleted resources, expired auth rows and stale journal entries go
/// once retention passes (docs/PRD.md section 21).
async fn retention_purge(pool: &sqlx::PgPool, days: i64) -> Result<(), String> {
    sqlx::query(&format!(
        "DELETE FROM events WHERE deleted_at < now() - interval '{days} days'"
    ))
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    for query in [
        "DELETE FROM sessions WHERE expires_at < now() - interval '7 days'",
        "DELETE FROM notifications WHERE read_at IS NOT NULL AND created_at < now() - interval '30 days'",
    ] {
        sqlx::query(query)
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

/// Finds alarms whose trigger falls in the window, creates deduped
/// notifications for the calendar's principals (DISPLAY) — EMAIL dispatch
/// joins when notification providers are configured (stage 16).
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

    // Group alarm rows by event so a recurring master expands once.
    let mut by_event: std::collections::HashMap<Uuid, Vec<&alarms::AlarmScanRow>> =
        std::collections::HashMap::new();
    for row in &rows {
        by_event.entry(row.event.id).or_default().push(row);
    }

    // Calendar principals for notification addressing.
    let mut principals: std::collections::HashMap<Uuid, Vec<Principal>> =
        std::collections::HashMap::new();
    let mut tenants: std::collections::HashMap<Uuid, Option<Uuid>> =
        std::collections::HashMap::new();
    let mut providers: std::collections::HashMap<Uuid, TenantProviders> =
        std::collections::HashMap::new();

    for (event_id, group) in &by_event {
        for scan in group {
            let triggers: Vec<DateTime<Utc>> = match scan.alarm.trigger_at {
                Some(at) => vec![at],
                None => {
                    let Some(offset) = scan.alarm.offset_secs() else {
                        continue;
                    };
                    let base_at = match scan.alarm.related.as_deref() {
                        Some("END") => scan.event.ends_at,
                        _ => scan.event.starts_at,
                    };
                    let Some(base_at) = base_at else { continue };
                    if scan.event.rrule.is_some() {
                        let expanded = calendar_core::recurrence::expand_occurrences(
                            DateOrDateTime::Timed(base_at),
                            scan.event.tzid.as_deref(),
                            Some(scan.event.rrule.as_deref().unwrap_or_default()),
                            &parse_points(&scan.event.rdate),
                            &parse_points(&scan.event.exdate),
                            lookback,
                            horizon,
                        )
                        .unwrap_or_default();
                        expanded
                            .into_iter()
                            .filter_map(|p| match p {
                                DateOrDateTime::Timed(at) => Some(at),
                                DateOrDateTime::AllDay(_) => None,
                            })
                            .map(|at| at + Duration::seconds(offset))
                            .collect()
                    } else {
                        vec![base_at + Duration::seconds(offset)]
                    }
                }
            };

            for trigger in triggers {
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
                let dedupe = format!("alarm:{}:{}", scan.alarm.id, trigger.to_rfc3339());
                let title = scan
                    .alarm
                    .summary
                    .clone()
                    .unwrap_or_else(|| scan.event.summary.clone());
                let body = scan
                    .alarm
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Reminder: {}", scan.event.summary));
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
                        Some(serde_json::json!({
                            "event_id": event_id,
                            "alarm_id": scan.alarm.id,
                            "trigger_at": trigger,
                            "action": scan.alarm.action,
                        })),
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
                let time_text = format_event_time(&scan.event);
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
                    for email in attendee_emails(pool, *event_id).await {
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
                        alarms::create_notification_deduped(
                            pool,
                            user_id,
                            "email",
                            Some(&title),
                            Some(&text),
                            Some(serde_json::json!({
                                "event_id": event_id,
                                "tenant_id": tenant_id,
                                "recipient": email,
                            })),
                            &format!("{dedupe}:email:{key}"),
                        )
                        .await
                        .ok();
                    }
                }

                // SMS: this event's attendees whose linked contact has a
                // mobile number, plus SMS-only attendees.
                if want_sms && p.sms {
                    for phone in sms_recipients(pool, *event_id).await {
                        let text =
                            format!("{} - {}", title, time_text.as_deref().unwrap_or_default());
                        alarms::create_notification_deduped(
                            pool,
                            None,
                            "sms",
                            Some(&title),
                            Some(&text),
                            Some(serde_json::json!({
                                "event_id": event_id,
                                "tenant_id": tenant_id,
                                "recipient": phone,
                            })),
                            &format!("{dedupe}:sms:{phone}"),
                        )
                        .await
                        .ok();
                    }
                }

                // Push: principals with active subscriptions (opt-out respected).
                if want_push && p.webpush {
                    for user in users.iter().filter(|u| u.notify_push) {
                        alarms::create_notification_deduped(
                            pool,
                            Some(user.id),
                            "push",
                            Some(&title),
                            Some(&body),
                            Some(serde_json::json!({
                                "event_id": event_id,
                                "tenant_id": tenant_id,
                            })),
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
                let Some(provider) =
                    crate::scheduling::load_email_provider(pool, Some(tenant_id), crypto).await
                else {
                    mark_unsent(pool, &row.id, "no email provider").await;
                    continue;
                };
                provider
                    .send(
                        &recipient,
                        row.title.as_deref().unwrap_or("Reminder"),
                        row.body.as_deref().unwrap_or_default(),
                    )
                    .await
            }
            "sms" => {
                let Some(provider) =
                    crate::rules_api::load_sms_provider(pool, tenant_id, crypto).await
                else {
                    mark_unsent(pool, &row.id, "no sms provider").await;
                    continue;
                };
                provider
                    .send(&recipient, row.body.as_deref().unwrap_or_default())
                    .await
            }
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
                    "UPDATE notifications SET sent_at = now(), send_error = NULL WHERE id = $1",
                )
                .bind(row.id)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                let attempts: i32 = sqlx::query_scalar(
                    "UPDATE notifications
                     SET send_attempts = send_attempts + 1, send_error = $2
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

async fn mark_unsent(pool: &sqlx::PgPool, id: &Uuid, error: &str) {
    sqlx::query("UPDATE notifications SET send_error = $2 WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(pool)
        .await
        .ok();
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
    use base64::Engine;
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
    let raw = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
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

/// Event start in its own timezone, for message bodies.
fn format_event_time(event: &db::EventRow) -> Option<String> {
    let at = event.starts_at?;
    Some(
        match event
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

/// Distinct attendee emails for one event.
async fn attendee_emails(pool: &sqlx::PgPool, event_id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT DISTINCT email::text FROM event_attendees
         WHERE event_id = $1 AND email IS NOT NULL",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// SMS recipients: attendees whose linked contact has a mobile tel, plus
/// SMS-only attendees (telephone set).
async fn sms_recipients(pool: &sqlx::PgPool, event_id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT DISTINCT COALESCE(ct.number, ea.telephone::text)
         FROM event_attendees ea
         LEFT JOIN contacts c ON c.id = ea.contact_id AND c.deleted_at IS NULL
         LEFT JOIN contact_tels ct ON ct.contact_id = c.id AND ct.is_mobile
         WHERE ea.event_id = $1 AND (ct.number IS NOT NULL OR ea.telephone IS NOT NULL)",
    )
    .bind(event_id)
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
