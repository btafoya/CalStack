//! TOTP 2FA and WebAuthn passkey endpoints (docs/PRD.md section 14).

use crate::{AppError, AppState, require_session_mutation, resolve_auth, set_session_cookie};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use calendar_auth::webauthn::{PasskeyManager, deserialize_passkeys};
use calendar_db::{self as db};
use chrono::Utc;
use serde_json::json;
use std::sync::Mutex;
use std::sync::MutexGuard;
use uuid::Uuid;
use webauthn_rs::prelude::{
    PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
};

// ============ TOTP ============

async fn totp_setup(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    let crypto = crypto.ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    let totp = calendar_auth::totp::generate("calendar-server", &auth.user.username);
    let encrypted = crypto
        .encrypt(&totp.secret)
        .map_err(|e| AppError::internal(e.to_string()))?;
    db::auth_ext::upsert_totp_secret(&pool, auth.user.id, &encrypted).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"otpauth_url": totp.otpauth_url, "secret_base32": totp.base32_secret})),
    ))
}

async fn totp_verify(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpCodeBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    let crypto = crypto.ok_or_else(|| AppError::internal("APP_ENCRYPTION_KEY is not set"))?;
    let row = db::auth_ext::get_totp_secret(&pool, auth.user.id)
        .await?
        .ok_or_else(|| AppError::bad_request("TOTP is not set up"))?;
    let secret = crypto
        .decrypt(&row.secret_encrypted)
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !calendar_auth::totp::verify(&secret, &body.code) {
        return Err(AppError::unauthorized());
    }
    // Eight one-time recovery codes, shown once, stored hashed.
    let codes: Vec<String> = (0..8)
        .map(|_| calendar_auth::generate_secret()[..10].to_string())
        .collect();
    let hashes: Vec<String> = codes
        .iter()
        .map(|c| calendar_auth::crypto::recovery_code_hash(c))
        .collect();
    db::auth_ext::confirm_totp(&pool, auth.user.id, &hashes).await?;
    Ok(Json(json!({"recovery_codes": codes})))
}

async fn totp_status(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let row = db::auth_ext::get_totp_secret(&pool, auth.user.id).await?;
    Ok(Json(json!({
        "enabled": row.is_some_and(|r| r.confirmed_at.is_some()),
    })))
}

async fn totp_disable(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    db::auth_ext::delete_totp_secret(&pool, auth.user.id).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ WebAuthn passkeys ============

/// Pending ceremony state, in-memory on purpose: challenges are single-use,
/// 10-minute TTL, and a process restart just means the user retries.
pub(crate) enum Ceremony {
    Register(PasskeyRegistration),
    Login {
        state: PasskeyAuthentication,
        stored: Vec<webauthn_rs::prelude::Passkey>,
    },
}

pub struct PasskeyStore {
    pub webauthn: PasskeyManager,
    state: Mutex<std::collections::HashMap<Uuid, (Ceremony, chrono::DateTime<Utc>)>>,
}

impl PasskeyStore {
    pub fn new(webauthn: PasskeyManager) -> Self {
        Self {
            webauthn,
            state: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn insert(&self, ceremony: Ceremony) -> Uuid {
        let id = Uuid::new_v4();
        let mut map = self.lock();
        let cutoff = Utc::now() - chrono::Duration::minutes(10);
        map.retain(|_, (_, at)| *at > cutoff);
        map.insert(id, (ceremony, Utc::now()));
        id
    }

    fn take(&self, id: Uuid) -> Option<Ceremony> {
        self.lock().remove(&id).map(|(c, _)| c)
    }

    fn lock(
        &self,
    ) -> MutexGuard<'_, std::collections::HashMap<Uuid, (Ceremony, chrono::DateTime<Utc>)>> {
        self.state.lock().unwrap() // poisoning only loses a 10-minute challenge
    }
}

#[derive(serde::Deserialize)]
struct TotpCodeBody {
    code: String,
}

#[derive(serde::Deserialize)]
struct RegisterFinishBody {
    challenge_id: Uuid,
    name: Option<String>,
    credential: RegisterPublicKeyCredential,
}

#[derive(serde::Deserialize)]
struct LoginStartBody {
    username_or_email: String,
}

#[derive(serde::Deserialize)]
struct LoginFinishBody {
    challenge_id: Uuid,
    credential: PublicKeyCredential,
}

async fn passkey_register_start(
    State(AppState { pool, passkeys, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    let store = passkeys.as_ref().ok_or_else(|| {
        AppError::internal("WebAuthn is not configured (set WEBAUTHN_RP_ID/WEBAUTHN_ORIGIN)")
    })?;
    let existing = db::auth_ext::list_webauthn_credentials(&pool, auth.user.id).await?;
    let (ccr, state) = store
        .webauthn
        .start_registration(
            auth.user.id,
            &auth.user.username,
            auth.user
                .display_name
                .as_deref()
                .unwrap_or(&auth.user.username),
            existing.iter().map(|c| c.credential_id.clone()).collect(),
        )
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let id = store.insert(Ceremony::Register(state));
    Ok((
        StatusCode::CREATED,
        Json(json!({"challenge_id": id, "challenge": ccr})),
    ))
}

async fn passkey_register_finish(
    State(AppState { pool, passkeys, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    let store = passkeys
        .as_ref()
        .ok_or_else(|| AppError::internal("WebAuthn is not configured"))?;
    let Ceremony::Register(state) = store
        .take(body.challenge_id)
        .ok_or_else(|| AppError::bad_request("unknown or expired challenge"))?
    else {
        return Err(AppError::bad_request("challenge is not a registration"));
    };
    let registered = store
        .webauthn
        .finish_registration(&body.credential, &state)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let row = db::auth_ext::create_webauthn_credential(
        &pool,
        auth.user.id,
        &registered.credential_id,
        &registered.passkey_json,
        body.name.as_deref(),
    )
    .await?;
    Ok(Json(json!({"id": row.id, "name": row.name})))
}

async fn passkey_login_start(
    State(AppState { pool, passkeys, .. }): State<AppState>,
    Json(body): Json<LoginStartBody>,
) -> Result<impl IntoResponse, AppError> {
    let store = passkeys
        .as_ref()
        .ok_or_else(|| AppError::internal("WebAuthn is not configured"))?;
    // Existence leaks are acceptable here only in the negative: an unknown
    // user gets an unusable challenge (no credentials) rather than a 404.
    let user = match db::find_user_by_username(&pool, &body.username_or_email).await {
        Ok(u) => Some(u),
        Err(_) => db::find_user_by_email(&pool, &body.username_or_email)
            .await
            .ok(),
    };
    let stored_rows = match user {
        Some(u) => db::auth_ext::list_webauthn_credentials(&pool, u.id).await?,
        None => vec![],
    };
    let passkeys_stored = deserialize_passkeys(
        &stored_rows
            .iter()
            .map(|r| r.public_key.clone())
            .collect::<Vec<_>>(),
    );
    let (rcr, state) = store
        .webauthn
        .start_authentication(&passkeys_stored)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let id = store.insert(Ceremony::Login {
        state,
        stored: passkeys_stored,
    });
    Ok(Json(json!({"challenge_id": id, "challenge": rcr})))
}

async fn passkey_login_finish(
    State(AppState {
        pool,
        passkeys,
        session_ttl,
        ..
    }): State<AppState>,
    Json(body): Json<LoginFinishBody>,
) -> Result<impl IntoResponse, AppError> {
    let store = passkeys
        .as_ref()
        .ok_or_else(|| AppError::internal("WebAuthn is not configured"))?;
    let Ceremony::Login { state, stored } = store
        .take(body.challenge_id)
        .ok_or_else(|| AppError::bad_request("unknown or expired challenge"))?
    else {
        return Err(AppError::bad_request("challenge is not a login"));
    };
    let (cred_id, updated) = store
        .webauthn
        .finish_authentication(&body.credential, &state, &stored)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let row = db::auth_ext::find_webauthn_credential(&pool, &cred_id)
        .await?
        .ok_or(AppError::unauthorized())?;
    if !updated.is_empty() {
        // Counter advanced (or flags changed): refresh the stored serialization.
        sqlx::query(
            "UPDATE webauthn_credentials SET public_key = $2, last_used_at = now() WHERE id = $1",
        )
        .bind(row.id)
        .bind(&updated)
        .execute(&pool)
        .await
        .map_err(db::DbError::Sql)?;
    } else {
        db::auth_ext::touch_webauthn_credential(&pool, row.id, 0).await?;
    }
    let user = db::find_user_by_id(&pool, row.user_id).await?;
    if user.disabled_at.is_some() {
        return Err(AppError::unauthorized());
    }
    let secret = calendar_auth::generate_session_token();
    let csrf = calendar_auth::generate_secret();
    db::create_session(
        &pool,
        user.id,
        &calendar_auth::sha256(secret.as_bytes()),
        &csrf,
        session_ttl,
    )
    .await?;
    let mut response =
        Json(json!({"csrf_token": csrf, "user": user_view_json(&user)})).into_response();
    set_session_cookie(&mut response, &secret);
    Ok(response)
}

async fn list_passkeys(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows = db::auth_ext::list_webauthn_credentials(&pool, auth.user.id).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|r| json!({"id": r.id, "name": r.name, "created_at": r.created_at, "last_used_at": r.last_used_at}))
        .collect::<Vec<_>>())))
}

async fn delete_passkey(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(credential_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    db::auth_ext::delete_webauthn_credential(&pool, auth.user.id, credential_id).await?;
    Ok(Json(json!({"ok": true})))
}

fn user_view_json(user: &db::UserRow) -> serde_json::Value {
    json!({
        "id": user.id,
        "username": user.username,
        "email": user.email,
        "display_name": user.display_name,
        "is_admin": user.is_admin,
    })
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/auth/totp/setup", post(totp_setup))
        .route("/api/auth/totp/verify", post(totp_verify))
        .route("/api/auth/totp", get(totp_status).delete(totp_disable))
        .route(
            "/api/auth/webauthn/register/start",
            post(passkey_register_start),
        )
        .route(
            "/api/auth/webauthn/register/finish",
            post(passkey_register_finish),
        )
        .route("/api/auth/webauthn/login/start", post(passkey_login_start))
        .route(
            "/api/auth/webauthn/login/finish",
            post(passkey_login_finish),
        )
        .route("/api/auth/webauthn", get(list_passkeys))
        .route(
            "/api/auth/webauthn/{id}",
            axum::routing::delete(delete_passkey),
        )
}
