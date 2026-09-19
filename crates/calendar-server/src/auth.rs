//! Authentication plumbing: sessions, API tokens, app passwords, CSRF, scopes.

use crate::{AppError, AppState};
use axum::{
    Json,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::Engine;
use calendar_core::validate_username;
use calendar_db::{self as db, UserRow};
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

/// Authenticated request identity: a live session (cookie) or an API token
/// (Authorization: Bearer). CalDAV app-password Basic auth joins later.
#[derive(Debug, Clone)]
pub(crate) struct Auth {
    pub(crate) user: UserRow,
    pub(crate) session: Option<db::SessionRow>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthExtractError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("token scope does not permit this operation")]
    Forbidden,
    #[error(transparent)]
    Db(#[from] db::DbError),
}

impl IntoResponse for AuthExtractError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            AuthExtractError::Unauthorized => StatusCode::UNAUTHORIZED,
            AuthExtractError::Forbidden => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({"error": self.to_string()}))).into_response()
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

/// Resolves the caller: session cookie first, then Bearer token, then
/// CalDAV-style Basic auth over app passwords.
pub(crate) async fn resolve_auth(
    pool: &PgPool,
    headers: &HeaderMap,
) -> Result<Auth, AuthExtractError> {
    if let Some(secret) = cookie_value(headers, "session") {
        let hash = calendar_auth::sha256(secret.as_bytes());
        let session = db::find_live_session(pool, &hash)
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        let user = db::find_user_by_id(pool, session.user_id)
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        return Ok(Auth {
            user,
            session: Some(session),
        });
    }
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Some(bearer) = auth_header.and_then(|v| v.strip_prefix("Bearer ")) {
        let token = db::find_live_api_token(pool, &calendar_auth::sha256(bearer.as_bytes()))
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        let user = db::find_user_by_id(pool, token.user_id)
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        return Ok(Auth {
            user,
            session: None,
        });
    }
    if let Some(basic) = auth_header.and_then(|v| v.strip_prefix("Basic ")) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or(AuthExtractError::Unauthorized)?;
        let (username, password) = decoded
            .split_once(':')
            .ok_or(AuthExtractError::Unauthorized)?;
        // sha256 lookup-hash fast path; argon2 verifies the real password.
        let app_password =
            db::find_live_app_password(pool, &calendar_auth::sha256(password.as_bytes()))
                .await
                .map_err(|_| AuthExtractError::Unauthorized)?;
        let user = db::find_user_by_id(pool, app_password.user_id)
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        if user.username != username
            || calendar_auth::verify_password(password, &app_password.password_hash).is_err()
        {
            return Err(AuthExtractError::Unauthorized);
        }
        return Ok(Auth {
            user,
            session: None,
        });
    }
    Err(AuthExtractError::Unauthorized)
}

/// Session-authenticated requests must present their CSRF token on mutations.
pub(crate) fn require_csrf(auth: &Auth, headers: &HeaderMap) -> Result<(), AuthExtractError> {
    if let Some(session) = &auth.session {
        let supplied = headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !constant_time_eq(supplied.as_bytes(), session.csrf_token.as_bytes()) {
            return Err(AuthExtractError::Unauthorized);
        }
    }
    Ok(())
}

/// The request must be session-authenticated: bearer tokens (of any scope)
/// and Basic auth may not touch secrets-bearing endpoints.
pub(crate) fn require_session(auth: &Auth) -> Result<(), AppError> {
    if auth.session.is_some() {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Token scope model: empty (legacy) or "full" = everything; "write" implies
/// "read"; "read" grants GET/HEAD only. Enforced for Bearer tokens by
/// [`token_scope_guard`]; session-cookie requests are not scoped.
fn scope_allows(scopes: &[String], required: &str) -> bool {
    scopes.is_empty()
        || scopes
            .iter()
            .any(|s| s == "full" || s == required || (required == "read" && s == "write"))
}

/// Bearer tokens are scoped by HTTP verb: reads need "read", everything else
/// "write". Sessions and Basic auth are untouched. Runs before the handlers,
/// which resolve auth again — one extra indexed token lookup per Bearer
/// request (ponytail: central check beats 62 per-handler edits; dedupe by
/// stashing the token row in request extensions if lookup ever shows up).
pub(crate) async fn token_scope_guard(
    State(AppState { pool, .. }): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AuthExtractError> {
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(bearer) = bearer {
        let token = db::find_live_api_token(&pool, &calendar_auth::sha256(bearer.as_bytes()))
            .await
            .map_err(|_| AuthExtractError::Unauthorized)?;
        let required = match *req.method() {
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS => {
                "read"
            }
            _ => "write",
        };
        if !scope_allows(&token.scopes, required) {
            return Err(AuthExtractError::Forbidden);
        }
    }
    Ok(next.run(req).await)
}

/// The /admin, /rules, /providers and /credentials pages are admin-only.
/// Signed-out visitors go to /login, signed-in non-admins to /. Their APIs
/// are gated per handler; this only guards the HTML pages.
pub(crate) async fn admin_page_guard(
    State(AppState { pool, .. }): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if matches!(
        req.uri().path(),
        "/admin" | "/rules" | "/providers" | "/credentials"
    ) {
        let verdict = match resolve_auth(&pool, req.headers()).await {
            Ok(auth) if auth.user.is_admin => None,
            Ok(_) => Some("/"),
            Err(_) => Some("/login"),
        };
        if let Some(dest) = verdict {
            return axum::response::Redirect::to(dest).into_response();
        }
    }
    next.run(req).await
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Admin gate: 403 for signed-in non-admins.
pub(crate) fn require_admin(auth: &Auth) -> Result<(), AppError> {
    if auth.user.is_admin {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub(crate) fn set_session_cookie(response: &mut axum::response::Response, token: &str) {
    // ponytail: no Secure flag — TLS termination is deployment's concern; flip when behind TLS.
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        format!("session={token}; Path=/; HttpOnly; SameSite=Lax")
            .parse()
            .unwrap(),
    );
}

#[derive(serde::Deserialize)]
struct RegisterBody {
    username: String,
    email: String,
    password: String,
    display_name: Option<String>,
}

#[derive(serde::Deserialize)]
struct LoginBody {
    username_or_email: String,
    password: String,
    totp_code: Option<String>,
    recovery_code: Option<String>,
}

/// If the user confirmed TOTP, a code (or one-time recovery code) is required.
async fn verify_totp_gate(
    pool: &PgPool,
    user: &db::UserRow,
    totp_code: &Option<String>,
    recovery_code: &Option<String>,
    crypto: Option<&calendar_auth::Crypto>,
) -> Result<(), AppError> {
    let Some(row) = db::auth_ext::get_totp_secret(pool, user.id)
        .await?
        .filter(|r| r.confirmed_at.is_some())
    else {
        return Ok(());
    };
    if let Some(recovery) = recovery_code.as_deref() {
        let hash = calendar_auth::crypto::recovery_code_hash(recovery);
        if db::auth_ext::consume_totp_recovery_code(pool, user.id, &hash).await? {
            return Ok(());
        }
    }
    let Some(crypto) = crypto else {
        // Secret is encrypted with a key we do not have: fail closed.
        return Err(AppError::unauthorized());
    };
    let secret = crypto
        .decrypt(&row.secret_encrypted)
        .map_err(|e| AppError::internal(e.to_string()))?;
    match totp_code
        .as_deref()
        .map(|code| calendar_auth::totp::verify(&secret, code))
    {
        Some(true) => Ok(()),
        _ => Err(AppError::unauthorized()),
    }
}

#[derive(serde::Deserialize)]
struct TokenBody {
    name: String,
    scopes: Option<Vec<String>>,
    expires_at: Option<chrono::DateTime<Utc>>,
}

#[derive(serde::Serialize)]
struct UserView {
    id: Uuid,
    username: String,
    email: String,
    display_name: Option<String>,
    is_admin: bool,
    notify_email: bool,
    notify_sms: bool,
    notify_push: bool,
}

fn user_view(user: &UserRow) -> UserView {
    UserView {
        id: user.id,
        username: user.username.clone(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        is_admin: user.is_admin,
        notify_email: user.notify_email,
        notify_sms: user.notify_sms,
        notify_push: user.notify_push,
    }
}

/// Per-user reminder opt-outs (in-app is never optional).
#[derive(serde::Deserialize)]
struct NotifyPrefsBody {
    notify_email: bool,
    notify_sms: bool,
    notify_push: bool,
}

async fn set_notify_prefs(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifyPrefsBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    sqlx::query(
        "UPDATE users SET notify_email = $2, notify_sms = $3, notify_push = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(auth.user.id)
    .bind(body.notify_email)
    .bind(body.notify_sms)
    .bind(body.notify_push)
    .execute(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn register(
    State(AppState { pool, .. }): State<AppState>,
    Json(body): Json<RegisterBody>,
) -> Result<impl IntoResponse, AppError> {
    validate_username(&body.username).map_err(|e| AppError::bad_request(e.to_string()))?;
    calendar_core::validate_email(&body.email).map_err(|e| AppError::bad_request(e.to_string()))?;
    if body.password.len() < 8 {
        return Err(AppError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    let hash = calendar_auth::hash_password(&body.password)
        .map_err(|e| AppError::internal(e.to_string()))?;
    let user = db::create_user(
        &pool,
        &body.username,
        &body.email,
        body.display_name.as_deref(),
        &hash,
    )
    .await
    .map_err(|e| match e {
        db::DbError::Conflict(msg) => AppError::bad_request(msg),
        other => other.into(),
    })?;
    Ok((StatusCode::CREATED, Json(user_view(&user))))
}

async fn login(
    State(AppState {
        pool,
        session_ttl,
        crypto,
        ..
    }): State<AppState>,
    Json(body): Json<LoginBody>,
) -> Result<impl IntoResponse, AppError> {
    let user = match db::find_user_by_email(&pool, &body.username_or_email).await {
        Ok(user) => Ok(user),
        Err(db::DbError::NotFound) => db::find_user_by_username(&pool, &body.username_or_email)
            .await
            .map_err(|_| AppError::unauthorized()),
        Err(e) => Err(e.into()),
    }?;
    if user.disabled_at.is_some() || user.password_hash.is_none() {
        return Err(AppError::unauthorized());
    }
    if calendar_auth::verify_password(&body.password, user.password_hash.as_deref().unwrap())
        .is_err()
    {
        return Err(AppError::unauthorized());
    }
    verify_totp_gate(
        &pool,
        &user,
        &body.totp_code,
        &body.recovery_code,
        crypto.as_deref(),
    )
    .await?;
    calendar_db::delete_expired_sessions(&pool).await.ok(); // ponytail: lazy purge on login

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
    let mut response = Json(serde_json::json!({
        "csrf_token": csrf,
        "user": user_view(&user),
    }))
    .into_response();
    set_session_cookie(&mut response, &secret);
    Ok(response)
}

#[derive(serde::Deserialize)]
struct ChangePasswordBody {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    // Wrong current password is a validation failure, not a lost session — a
    // 401 here would bounce the (still logged-in) caller to /login.
    let current_hash = auth
        .user
        .password_hash
        .as_deref()
        .ok_or_else(|| AppError::bad_request("incorrect current password"))?;
    if calendar_auth::verify_password(&body.current_password, current_hash).is_err() {
        return Err(AppError::bad_request("incorrect current password"));
    }
    if body.new_password.len() < 8 {
        return Err(AppError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    let hash = calendar_auth::hash_password(&body.new_password)
        .map_err(|e| AppError::internal(e.to_string()))?;
    db::set_password(&pool, auth.user.id, &hash).await?;
    db::revoke_other_sessions(&pool, auth.user.id, auth.session.map(|s| s.id)).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn logout(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    if let Some(session) = auth.session {
        db::revoke_session(&pool, session.id).await?;
    }
    let mut response = Json(serde_json::json!({"ok": true})).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        "session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
            .parse()
            .unwrap(),
    );
    Ok(response)
}

async fn me(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    Ok(Json(user_view(&auth.user)))
}

async fn create_token(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TokenBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let scopes = body.scopes.unwrap_or_default();
    if scopes
        .iter()
        .any(|s| !matches!(s.as_str(), "read" | "write" | "full"))
    {
        return Err(AppError::bad_request(
            "unknown scope; allowed: read, write, full",
        ));
    }
    let secret = calendar_auth::generate_secret();
    let token = db::create_api_token(
        &pool,
        auth.user.id,
        &body.name,
        &calendar_auth::sha256(secret.as_bytes()),
        &scopes,
        body.expires_at,
    )
    .await?;
    // The secret is returned exactly once; only its hash is stored.
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": token.id, "name": token.name, "secret": secret,
            "scopes": token.scopes, "expires_at": token.expires_at,
        })),
    ))
}

async fn list_tokens(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let tokens = db::list_api_tokens(&pool, auth.user.id).await?;
    Ok(Json(serde_json::json!(tokens
        .iter()
        .map(|t| serde_json::json!({"id": t.id, "name": t.name, "scopes": t.scopes, "expires_at": t.expires_at, "created_at": t.created_at}))
        .collect::<Vec<_>>())))
}

async fn revoke_token(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(token_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    db::revoke_api_token(&pool, auth.user.id, token_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct AppPasswordBody {
    name: String,
}

async fn create_app_password(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AppPasswordBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    let password = calendar_auth::generate_secret();
    let hash =
        calendar_auth::hash_password(&password).map_err(|e| AppError::internal(e.to_string()))?;
    let row = db::create_app_password(
        &pool,
        auth.user.id,
        &body.name,
        &hash,
        &calendar_auth::sha256(password.as_bytes()),
    )
    .await?;
    // The password is returned exactly once; only its hash is stored.
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id": row.id, "name": row.name, "password": password})),
    ))
}

async fn list_app_passwords(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    let rows = db::list_app_passwords(&pool, auth.user.id).await?;
    Ok(Json(serde_json::json!(rows
        .iter()
        .map(|r| serde_json::json!({"id": r.id, "name": r.name, "created_at": r.created_at, "last_used_at": r.last_used_at, "expires_at": r.expires_at}))
        .collect::<Vec<_>>())))
}

async fn revoke_app_password(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(password_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_admin(&auth)?;
    require_csrf(&auth, &headers)?;
    db::revoke_app_password(&pool, auth.user.id, password_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/password", post(change_password))
        .route("/api/auth/notify-prefs", post(set_notify_prefs))
        .route("/api/auth/tokens", post(create_token).get(list_tokens))
        .route("/api/auth/tokens/{id}", delete(revoke_token))
        .route(
            "/api/auth/app-passwords",
            post(create_app_password).get(list_app_passwords),
        )
        .route("/api/auth/app-passwords/{id}", delete(revoke_app_password))
}

#[cfg(test)]
mod scope_tests {
    use super::scope_allows;

    fn scopes(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_scopes_are_full_access() {
        assert!(scope_allows(&[], "read"));
        assert!(scope_allows(&[], "write"));
    }

    #[test]
    fn full_scope_grants_everything() {
        assert!(scope_allows(&scopes(&["full"]), "read"));
        assert!(scope_allows(&scopes(&["full"]), "write"));
    }

    #[test]
    fn read_scope_denies_writes() {
        assert!(scope_allows(&scopes(&["read"]), "read"));
        assert!(!scope_allows(&scopes(&["read"]), "write"));
    }

    #[test]
    fn write_scope_implies_read() {
        assert!(scope_allows(&scopes(&["write"]), "write"));
        assert!(scope_allows(&scopes(&["write"]), "read"));
    }

    #[test]
    fn unknown_scopes_grant_nothing() {
        assert!(!scope_allows(&scopes(&["admin"]), "read"));
        assert!(!scope_allows(&scopes(&["admin"]), "write"));
    }
}
