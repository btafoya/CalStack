//! TOTP and WebAuthn repositories (schema in 0001/0002).

use super::DbError;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

// ============ TOTP ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TotpRow {
    pub user_id: Uuid,
    pub secret_encrypted: Vec<u8>,
    pub confirmed_at: Option<chrono::DateTime<Utc>>,
    pub recovery_codes: Vec<String>,
    pub created_at: chrono::DateTime<Utc>,
}

pub async fn upsert_totp_secret(
    pool: &PgPool,
    user_id: Uuid,
    secret_encrypted: &[u8],
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO totp_secrets (user_id, secret_encrypted) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET secret_encrypted = $2, confirmed_at = NULL",
    )
    .bind(user_id)
    .bind(secret_encrypted)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_totp_secret(pool: &PgPool, user_id: Uuid) -> Result<Option<TotpRow>, DbError> {
    sqlx::query_as::<_, TotpRow>("SELECT * FROM totp_secrets WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

pub async fn confirm_totp(
    pool: &PgPool,
    user_id: Uuid,
    recovery_hashes: &[String],
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE totp_secrets SET confirmed_at = now(), recovery_codes = $2 WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(recovery_hashes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_totp_secret(pool: &PgPool, user_id: Uuid) -> Result<(), DbError> {
    sqlx::query("DELETE FROM totp_secrets WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Consumes one recovery code hash (removes it on success).
pub async fn consume_totp_recovery_code(
    pool: &PgPool,
    user_id: Uuid,
    code_hash: &str,
) -> Result<bool, DbError> {
    let n = sqlx::query(
        "UPDATE totp_secrets
         SET recovery_codes = array_remove(recovery_codes, $2)
         WHERE user_id = $1 AND $2 = ANY(recovery_codes)",
    )
    .bind(user_id)
    .bind(code_hash)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

// ============ WebAuthn ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebauthnCredentialRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub sign_count: i64,
    pub transports: Vec<String>,
    pub name: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub last_used_at: Option<chrono::DateTime<Utc>>,
}

pub async fn list_webauthn_credentials(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<WebauthnCredentialRow>, DbError> {
    sqlx::query_as::<_, WebauthnCredentialRow>(
        "SELECT * FROM webauthn_credentials WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// All credentials for the user who owns `credential_id`, plus the matched
/// owner. Passkey login resolves the user by credential id alone.
pub async fn find_webauthn_credential(
    pool: &PgPool,
    credential_id: &[u8],
) -> Result<Option<WebauthnCredentialRow>, DbError> {
    sqlx::query_as::<_, WebauthnCredentialRow>(
        "SELECT * FROM webauthn_credentials WHERE credential_id = $1",
    )
    .bind(credential_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

pub async fn create_webauthn_credential(
    pool: &PgPool,
    user_id: Uuid,
    credential_id: &[u8],
    public_key: &[u8],
    name: Option<&str>,
) -> Result<WebauthnCredentialRow, DbError> {
    sqlx::query_as::<_, WebauthnCredentialRow>(
        "INSERT INTO webauthn_credentials (id, user_id, credential_id, public_key, name)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(credential_id)
    .bind(public_key)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Updates the signature counter and last-used stamp after a login.
pub async fn touch_webauthn_credential(
    pool: &PgPool,
    credential_db_id: Uuid,
    sign_count: i64,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE webauthn_credentials SET sign_count = $2, last_used_at = now() WHERE id = $1",
    )
    .bind(credential_db_id)
    .bind(sign_count)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_webauthn_credential(
    pool: &PgPool,
    user_id: Uuid,
    credential_id: Uuid,
) -> Result<(), DbError> {
    let n = sqlx::query("DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2")
        .bind(credential_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

// ============ challenge state ============

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChallengeRow {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub username: Option<String>,
    pub kind: String,
    pub state: serde_json::Value,
    pub expires_at: chrono::DateTime<Utc>,
}

pub async fn create_challenge(
    pool: &PgPool,
    user_id: Option<Uuid>,
    username: Option<&str>,
    kind: &str,
    state: serde_json::Value,
) -> Result<Uuid, DbError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO webauthn_challenges (id, user_id, username, kind, state, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(username)
    .bind(kind)
    .bind(state)
    .bind(Utc::now() + Duration::minutes(10))
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn get_challenge(pool: &PgPool, id: Uuid) -> Result<ChallengeRow, DbError> {
    let row = sqlx::query_as::<_, ChallengeRow>(
        "SELECT * FROM webauthn_challenges WHERE id = $1 AND expires_at > now()",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    // Challenges are single-use.
    sqlx::query("DELETE FROM webauthn_challenges WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(row)
}
