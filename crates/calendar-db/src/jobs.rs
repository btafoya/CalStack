//! Durable PostgreSQL job queue (docs/PRD.md section 17): leases, retries
//! with exponential backoff, and a worker identity for one-owner execution.

use chrono::Duration;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::DbError;

/// Enqueues a job to run at `run_at` (now by default). Idempotent callers use
/// their own uniqueness keys in the payload; the queue itself does not dedupe.
pub async fn enqueue(
    pool: &PgPool,
    job_type: &str,
    payload: Value,
    run_at: Option<DateTime<Utc>>,
    priority: i32,
) -> Result<Uuid, DbError> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO durable_jobs (id, job_type, payload, run_at, priority) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(job_type)
        .bind(payload)
        .bind(run_at.unwrap_or_else(Utc::now))
        .bind(priority)
        .execute(pool)
        .await?;
    Ok(id)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub job_type: String,
    pub payload: Value,
    pub run_at: chrono::DateTime<Utc>,
    pub attempts: i32,
    pub max_attempts: i32,
}

/// Claims the next runnable job for `worker_id` (SKIP LOCKED so many workers
/// can poll concurrently). Stuck leases (crashed worker) become claimable once
/// `locked_until` passes.
pub async fn lease_next(
    pool: &PgPool,
    worker_id: &str,
    lease_secs: i64,
) -> Result<Option<JobRow>, DbError> {
    let row = sqlx::query_as::<_, JobRow>(
        "UPDATE durable_jobs SET locked_until = $2, locked_by = $3, updated_at = now()
         WHERE id = (
            SELECT id FROM durable_jobs
            WHERE completed_at IS NULL AND failed_at IS NULL
              AND run_at <= now()
              AND (locked_until IS NULL OR locked_until < now())
            ORDER BY priority DESC, run_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
         )
         RETURNING id, job_type, payload, run_at, attempts, max_attempts",
    )
    .bind(chrono::Duration::seconds(lease_secs))
    .bind(Utc::now() + Duration::seconds(lease_secs))
    .bind(worker_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn complete(pool: &PgPool, job_id: Uuid) -> Result<(), DbError> {
    sqlx::query("UPDATE durable_jobs SET completed_at = now(), locked_until = NULL WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records a failure; re-runs with exponential backoff until max_attempts.
pub async fn fail(pool: &PgPool, job_id: Uuid, error: &str) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE durable_jobs SET
            attempts = attempts + 1,
            last_error = $2,
            locked_until = NULL,
            updated_at = now(),
            failed_at = CASE WHEN attempts + 1 >= max_attempts THEN now() ELSE failed_at END,
            run_at = now() + (LEAST(attempts + 1, 10) * interval '1 minute' * 2)
         WHERE id = $1",
    )
    .bind(job_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}
