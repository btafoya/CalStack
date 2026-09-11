//! Durable job worker (docs/PRD.md section 17): polls the PostgreSQL queue,
//! leases jobs with SKIP LOCKED, executes idempotently, retries with backoff.
//!
//! `alarm_scan` reschedules itself each pass; reminders expand recurrence in
//! Rust (ADR-002) and create notifications keyed by dedupe keys, so a
//! re-scan never double-fires.

use calendar_core::DateOrDateTime;
use calendar_db::{self as db, alarms};
use chrono::{DateTime, Duration, Utc};
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
            alarm_scan(pool).await?;
            // Self-rescheduling tick.
            schedule_alarm_scan(pool, Utc::now() + Duration::minutes(1))
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
        "DELETE FROM webauthn_challenges WHERE expires_at < now()",
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
async fn alarm_scan(pool: &sqlx::PgPool) -> Result<(), String> {
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
    let mut principals: std::collections::HashMap<Uuid, Vec<Uuid>> =
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
                        let users: Vec<Uuid> = sqlx::query_scalar(
                            "SELECT principal_user_id FROM calendar_acl WHERE calendar_id = $1",
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
                for user in users {
                    // Idempotent via the dedupe key; restarts and re-scans
                    // never duplicate a fired reminder.
                    alarms::create_notification_deduped(
                        pool,
                        user,
                        "in_app",
                        Some(&title),
                        Some(&body),
                        Some(serde_json::json!({
                            "event_id": event_id,
                            "alarm_id": scan.alarm.id,
                            "trigger_at": trigger,
                            "action": scan.alarm.action,
                        })),
                        &format!("{dedupe}:{user}"),
                    )
                    .await
                    .ok();
                }
            }
        }
    }
    Ok(())
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
