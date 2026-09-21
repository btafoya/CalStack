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

#[derive(serde::Serialize, utoipa::ToSchema)]
struct TotpSetupView {
    otpauth_url: String,
    secret_base32: String,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct TotpCodeBody {
    code: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct RecoveryCodesView {
    /// Eight one-time codes, shown exactly once; stored hashed.
    recovery_codes: Vec<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct TotpStatusView {
    enabled: bool,
}

#[utoipa::path(
    post,
    path = "/api/auth/totp/setup",
    responses(
        (status = 201, description = "unconfirmed secret + otpauth URL", body = TotpSetupView),
    )
)]
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
        Json(TotpSetupView {
            otpauth_url: totp.otpauth_url,
            secret_base32: totp.base32_secret,
        }),
    ))
}

#[utoipa::path(
    post,
    path = "/api/auth/totp/verify",
    request_body = TotpCodeBody,
    responses(
        (status = 200, description = "recovery codes", body = RecoveryCodesView),
        (status = 401, description = "bad code"),
    )
)]
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
    Ok(Json(RecoveryCodesView {
        recovery_codes: codes,
    }))
}

#[utoipa::path(
    get,
    path = "/api/auth/totp",
    responses(
        (status = 200, description = "status", body = TotpStatusView),
    )
)]
async fn totp_status(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let row = db::auth_ext::get_totp_secret(&pool, auth.user.id).await?;
    Ok(Json(TotpStatusView {
        enabled: row.is_some_and(|r| r.confirmed_at.is_some()),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/auth/totp",
    responses((status = 200, description = "disabled", body = crate::OkView))
)]
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

#[derive(serde::Serialize, utoipa::ToSchema)]
struct PasskeyChallengeView {
    challenge_id: Uuid,
    /// Opaque WebAuthn ceremony challenge; hand it to the authenticator as-is.
    #[schema(value_type = Object)]
    challenge: serde_json::Value,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct PasskeyRegisteredView {
    id: Uuid,
    name: Option<String>,
}

/// Narrower than auth::UserView on purpose: the passkey-login response has
/// always omitted the notify_* flags (shape preserved, see
/// IMPLEMENTATION_PLAN.md's compatibility constraint).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct PasskeyUserView {
    id: Uuid,
    username: String,
    email: String,
    display_name: Option<String>,
    is_admin: bool,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct PasskeySessionView {
    csrf_token: String,
    user: PasskeyUserView,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct PasskeyView {
    id: Uuid,
    name: Option<String>,
    created_at: chrono::DateTime<Utc>,
    last_used_at: Option<chrono::DateTime<Utc>>,
}

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

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct RegisterFinishBody {
    challenge_id: Uuid,
    name: Option<String>,
    #[schema(value_type = Object)]
    credential: RegisterPublicKeyCredential,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct LoginStartBody {
    username_or_email: String,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct LoginFinishBody {
    challenge_id: Uuid,
    #[schema(value_type = Object)]
    credential: PublicKeyCredential,
}

#[utoipa::path(
    post,
    path = "/api/auth/webauthn/register/start",
    responses(
        (status = 201, description = "ceremony challenge", body = PasskeyChallengeView),
    )
)]
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
        Json(PasskeyChallengeView {
            challenge_id: id,
            challenge: serde_json::to_value(&ccr).map_err(|e| AppError::internal(e.to_string()))?,
        }),
    ))
}

#[utoipa::path(
    post,
    path = "/api/auth/webauthn/register/finish",
    request_body = RegisterFinishBody,
    responses(
        (status = 200, description = "passkey stored", body = PasskeyRegisteredView),
        (status = 400, description = "bad or expired challenge"),
    )
)]
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
    Ok(Json(PasskeyRegisteredView {
        id: row.id,
        name: row.name,
    }))
}

#[utoipa::path(
    post,
    path = "/api/auth/webauthn/login/start",
    request_body = LoginStartBody,
    security(()),
    responses(
        (status = 200, description = "ceremony challenge (unknown users get an unusable one)", body = PasskeyChallengeView),
    )
)]
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
    Ok(Json(PasskeyChallengeView {
        challenge_id: id,
        challenge: serde_json::to_value(&rcr).map_err(|e| AppError::internal(e.to_string()))?,
    }))
}

#[utoipa::path(
    post,
    path = "/api/auth/webauthn/login/finish",
    request_body = LoginFinishBody,
    security(()),
    responses(
        (status = 200, description = "session established (cookie set)", body = PasskeySessionView),
        (status = 400, description = "bad or expired challenge"),
        (status = 401, description = "unauthorized"),
    )
)]
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
    crate::audit::write(
        &pool,
        "session",
        Some(user.id),
        "passkey_login",
        "user",
        Some(user.id),
        None,
    )
    .await;
    let mut response = Json(PasskeySessionView {
        csrf_token: csrf,
        user: PasskeyUserView {
            id: user.id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            is_admin: user.is_admin,
        },
    })
    .into_response();
    set_session_cookie(&mut response, &secret);
    Ok(response)
}

#[utoipa::path(
    get,
    path = "/api/auth/webauthn",
    responses(
        (status = 200, description = "list", body = Vec<PasskeyView>),
    )
)]
async fn list_passkeys(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows = db::auth_ext::list_webauthn_credentials(&pool, auth.user.id).await?;
    Ok(Json(
        rows.iter()
            .map(|r| PasskeyView {
                id: r.id,
                name: r.name.clone(),
                created_at: r.created_at,
                last_used_at: r.last_used_at,
            })
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/auth/webauthn/{id}",
    params(("id" = Uuid, Path, description = "passkey id")),
    responses((status = 200, description = "removed", body = crate::OkView))
)]
async fn delete_passkey(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(credential_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_session_mutation(&auth, &headers)?;
    db::auth_ext::delete_webauthn_credential(&pool, auth.user.id, credential_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// OpenAPI for the MFA module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        totp_setup,
        totp_verify,
        totp_status,
        totp_disable,
        passkey_register_start,
        passkey_register_finish,
        passkey_login_start,
        passkey_login_finish,
        list_passkeys,
        delete_passkey,
    ),
    components(schemas(
        TotpSetupView,
        TotpCodeBody,
        RecoveryCodesView,
        TotpStatusView,
        PasskeyChallengeView,
        PasskeyRegisteredView,
        PasskeyUserView,
        PasskeySessionView,
        PasskeyView,
        RegisterFinishBody,
        LoginStartBody,
        LoginFinishBody,
    ))
)]
pub(crate) struct MfaApi;

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
