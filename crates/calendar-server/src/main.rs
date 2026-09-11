//! Single production executable: HTTP server, CLI commands.

mod dav;
mod extras;
mod jobs;
mod mfa;
mod rules_api;
mod scheduling;
mod sharing_api;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use base64::Engine;
use calendar_core::validate_username;
use calendar_db::{self as db, UserRow};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    name = "calendar-server",
    version,
    about = "Lightweight PostgreSQL-backed CalDAV calendar server"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the HTTP server (default).
    Serve,
    /// Run pending migrations and exit.
    Migrate,
    /// Verify configuration and database connectivity.
    Check,
    /// Export a portable JSON backup to stdout (attachments included).
    Backup,
    /// Import a portable JSON backup from a file (empty database only).
    Restore { path: String },
}

/// Environment-only configuration (docs/PRD.md section 23).
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub database_max_connections: u32,
    pub bind_addr: String,
    pub session_ttl: Duration,
    pub encryption_key: Option<String>,
    pub webauthn_rp_id: Option<String>,
    pub webauthn_origin: Option<String>,
    /// Per-attachment cap in bytes (ADR-010; PRD default 50 MB).
    pub attachment_max_bytes: i64,
    /// Soft-deleted calendar resources live this long before purge.
    pub retention_days: i64,
}

impl Config {
    fn from_env() -> Result<Self> {
        let env = |name: &str| std::env::var(name).ok();
        Ok(Self {
            database_url: env("DATABASE_URL").context("DATABASE_URL is required")?,
            database_max_connections: env("DATABASE_MAX_CONNECTIONS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(16),
            bind_addr: env("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            session_ttl: Duration::hours(
                env("SESSION_TTL_HOURS")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(24 * 7),
            ),
            encryption_key: env("APP_ENCRYPTION_KEY"),
            webauthn_rp_id: env("WEBAUTHN_RP_ID"),
            webauthn_origin: env("WEBAUTHN_ORIGIN"),
            attachment_max_bytes: env("ATTACHMENT_MAX_BYTES")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50 * 1024 * 1024),
            retention_days: env("RETENTION_DAYS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        })
    }
}

// ============ authentication plumbing ============

/// Authenticated request identity: a live session (cookie) or an API token
/// (Authorization: Bearer). CalDAV app-password Basic auth joins later.
#[derive(Debug, Clone)]
struct Auth {
    user: UserRow,
    session: Option<db::SessionRow>,
}

#[derive(Debug, thiserror::Error)]
enum AuthExtractError {
    #[error("unauthorized")]
    Unauthorized,
    #[error(transparent)]
    Db(#[from] db::DbError),
}

impl IntoResponse for AuthExtractError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            AuthExtractError::Unauthorized => StatusCode::UNAUTHORIZED,
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
async fn resolve_auth(pool: &PgPool, headers: &HeaderMap) -> Result<Auth, AuthExtractError> {
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
fn require_csrf(auth: &Auth, headers: &HeaderMap) -> Result<(), AuthExtractError> {
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ============ request/response payloads ============

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
}

fn user_view(user: &UserRow) -> UserView {
    UserView {
        id: user.id,
        username: user.username.clone(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        is_admin: user.is_admin,
    }
}

fn set_session_cookie(response: &mut axum::response::Response, token: &str) {
    // ponytail: no Secure flag — TLS termination is deployment's concern; flip when behind TLS.
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        format!("session={token}; Path=/; HttpOnly; SameSite=Lax")
            .parse()
            .unwrap(),
    );
}

// ============ handlers ============

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
    require_csrf(&auth, &headers)?;
    let secret = calendar_auth::generate_secret();
    let token = db::create_api_token(
        &pool,
        auth.user.id,
        &body.name,
        &calendar_auth::sha256(secret.as_bytes()),
        body.scopes.as_deref().unwrap_or(&[]),
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
    require_csrf(&auth, &headers)?;
    db::revoke_app_password(&pool, auth.user.id, password_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ============ calendars ============

#[derive(serde::Deserialize)]
struct CalendarBody {
    slug: String,
    name: String,
    description: Option<String>,
    color: Option<String>,
    timezone: Option<String>,
}

#[derive(serde::Deserialize)]
struct CalendarPatchBody {
    name: Option<String>,
    description: Option<String>,
    color: Option<String>,
    timezone: Option<String>,
    order_index: Option<i32>,
}

#[derive(serde::Deserialize)]
struct AclEntryBody {
    user_id: Uuid,
    capability: String,
    can_manage_acl: bool,
}

#[derive(serde::Deserialize)]
struct AclBody {
    entries: Vec<AclEntryBody>,
}

fn calendar_view(
    calendar: &db::CalendarRow,
    capability: calendar_core::CalendarCapability,
) -> serde_json::Value {
    serde_json::json!({
        "id": calendar.id,
        "slug": calendar.slug,
        "name": calendar.name,
        "description": calendar.description,
        "color": calendar.color,
        "timezone": calendar.timezone,
        "order_index": calendar.order_index,
        "created_at": calendar.created_at,
        "updated_at": calendar.updated_at,
        "my_capability": capability.as_db_str(),
    })
}

fn parse_acl(
    body: &AclBody,
) -> Result<Vec<(Uuid, calendar_core::CalendarCapability, bool)>, AppError> {
    let entries: Vec<calendar_core::AclEntry> = body
        .entries
        .iter()
        .map(|e| {
            calendar_core::CalendarCapability::from_db_str(&e.capability)
                .map(|cap| calendar_core::AclEntry {
                    principal_user_id: e.user_id,
                    capability: cap,
                    can_manage_acl: e.can_manage_acl,
                })
                .ok_or_else(|| {
                    AppError::bad_request(format!("unknown capability: {}", e.capability))
                })
        })
        .collect::<Result<_, _>>()?;
    let set = calendar_core::AclSet::validate(&entries)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    Ok(set
        .entries
        .iter()
        .map(|e| (e.principal_user_id, e.capability, e.can_manage_acl))
        .collect())
}

/// ACL guard: fetch capability, require it, 404 when absent (no existence leak).
async fn require_capability(
    pool: &PgPool,
    calendar_id: Uuid,
    user_id: Uuid,
    required: calendar_core::CalendarCapability,
) -> Result<db::CalendarRow, AppError> {
    let cap = db::calendar_capability(pool, calendar_id, user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !cap.satisfies(required) {
        return Err(AppError::Forbidden);
    }
    Ok(db::get_calendar(pool, calendar_id).await?)
}

async fn create_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CalendarBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    calendar_core::validate_slug(&body.slug).map_err(|e| AppError::bad_request(e.to_string()))?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let new_calendar = db::NewCalendar {
        slug: body.slug,
        name: body.name,
        description: body.description,
        color: body.color,
        timezone: body.timezone,
    };
    let calendar = db::create_calendar(
        &pool,
        tenant_id,
        &new_calendar,
        auth.user.id,
        &[(auth.user.id, calendar_core::CalendarCapability::Owner, true)],
    )
    .await
    .map_err(|e| match e {
        db::DbError::Conflict(msg) => AppError::bad_request(msg),
        other => other.into(),
    })?;
    Ok((
        StatusCode::CREATED,
        Json(calendar_view(
            &calendar,
            calendar_core::CalendarCapability::Owner,
        )),
    ))
}

async fn list_calendars(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows = db::list_calendars_for_user(&pool, auth.user.id).await?;
    Ok(Json(serde_json::json!(
        rows.iter()
            .map(|(cal, cap)| calendar_view(cal, *cap))
            .collect::<Vec<_>>()
    )))
}

async fn get_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let calendar = require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let cap = db::calendar_capability(&pool, calendar_id, auth.user.id)
        .await?
        .unwrap_or(calendar_core::CalendarCapability::FreeBusy);
    Ok(Json(calendar_view(&calendar, cap)))
}

async fn patch_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<CalendarPatchBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    let changes = db::CalendarUpdate {
        name: body.name,
        description: body.description,
        color: body.color,
        timezone: body.timezone,
        order_index: body.order_index,
    };
    let calendar = db::update_calendar(&pool, calendar_id, &changes).await?;
    let cap = db::calendar_capability(&pool, calendar_id, auth.user.id)
        .await?
        .unwrap_or(calendar_core::CalendarCapability::FreeBusy);
    Ok(Json(calendar_view(&calendar, cap)))
}

async fn delete_calendar(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    db::soft_delete_calendar(&pool, calendar_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn get_calendar_acl(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    let acl = db::list_calendar_acl(&pool, calendar_id).await?;
    Ok(Json(serde_json::json!(
        acl.iter()
            .map(|(user, cap, manage)| serde_json::json!({
                "user_id": user, "capability": cap.as_db_str(), "can_manage_acl": manage,
            }))
            .collect::<Vec<_>>()
    )))
}

async fn put_calendar_acl(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<AclBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::Owner,
    )
    .await?;
    let acl = parse_acl(&body)?;
    db::replace_calendar_acl(&pool, calendar_id, &acl).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ============ events ============

#[derive(serde::Deserialize)]
struct EventBody {
    uid: Option<String>,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    starts_at: Option<chrono::DateTime<Utc>>,
    ends_at: Option<chrono::DateTime<Utc>>,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    tzid: Option<String>,
    all_day: Option<bool>,
    rrule: Option<String>,
    rdate: Option<serde_json::Value>,
    exdate: Option<serde_json::Value>,
    status: Option<String>,
    priority: Option<i16>,
    class: Option<String>,
    transp: Option<String>,
    categories: Option<Vec<String>>,
    attendees: Option<Vec<db::NewAttendee>>,
    // exception support
    master_event_id: Option<Uuid>,
    recurrence_id: Option<chrono::NaiveDateTime>,
    recurrence_id_date: Option<chrono::NaiveDate>,
}

fn event_view(
    event: &db::EventRow,
    etag: &str,
    attendees: &[db::AttendeeRow],
) -> serde_json::Value {
    serde_json::json!({
        "id": event.id,
        "calendar_id": event.calendar_id,
        "uid": event.uid,
        "master_event_id": event.master_event_id,
        "recurrence_id": event.recurrence_id,
        "recurrence_id_date": event.recurrence_id_date,
        "summary": event.summary,
        "description_html": event.description_html,
        "description_text": event.description_text,
        "url": event.url,
        "starts_at": event.starts_at,
        "ends_at": event.ends_at,
        "start_date": event.start_date,
        "end_date": event.end_date,
        "tzid": event.tzid,
        "all_day": event.all_day,
        "rrule": event.rrule,
        "rdate": event.rdate,
        "exdate": event.exdate,
        "status": event.status,
        "priority": event.priority,
        "class": event.class,
        "transp": event.transp,
        "categories": event.categories,
        "location_id": event.location_id,
        "organizer_email": event.organizer_email,
        "sequence": event.sequence,
        "etag": etag,
        "created_at": event.created_at,
        "updated_at": event.updated_at,
        "attendees": attendees.iter().map(|a| serde_json::json!({
            "email": a.email, "display_name": a.display_name, "telephone": a.telephone,
            "role": a.role, "partstat": a.partstat, "rsvp": a.rsvp,
        })).collect::<Vec<_>>(),
    })
}

/// Mirrors the events-table CHECK constraints; the full structural model lives
/// in calendar-core and the iCalendar stage reuses it.
fn validate_event_body(body: &EventBody) -> Result<(), AppError> {
    let timed = body.starts_at.is_some();
    let all_day = body.start_date.is_some();
    if timed == all_day {
        return Err(AppError::bad_request(
            "exactly one of starts_at or start_date",
        ));
    }
    if timed && body.ends_at.is_none() {
        return Err(AppError::bad_request("timed event needs ends_at"));
    }
    if body.all_day.unwrap_or(false) != all_day && !timed && body.all_day.is_some() {
        return Err(AppError::bad_request(
            "all_day must match start_date presence",
        ));
    }
    if let Some(p) = body.priority
        && !(0..=9).contains(&p)
    {
        return Err(AppError::bad_request("priority must be 0..=9"));
    }
    if body.master_event_id.is_some()
        && body.recurrence_id.is_none()
        && body.recurrence_id_date.is_none()
    {
        return Err(AppError::bad_request("exception needs recurrence_id"));
    }
    for a in body.attendees.iter().flatten() {
        calendar_core::validate_email(&a.email)
            .map_err(|e| AppError::bad_request(e.to_string()))?;
    }
    Ok(())
}

async fn create_event(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<EventBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    validate_event_body(&body)?;
    let uid = body
        .uid
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let data = db::NewEventData {
        uid,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        start_date: body.start_date,
        end_date: body.end_date,
        tzid: body.tzid.clone(),
        all_day: body.all_day.unwrap_or(false),
        rrule: body.rrule.clone(),
        rdate: body.rdate.clone(),
        exdate: body.exdate.clone(),
        summary: body.summary.clone(),
        description_html: body.description_html.clone(),
        description_text: body.description_text.clone(),
        url: body.url.clone(),
        status: body.status.clone(),
        priority: body.priority,
        class: body.class.clone(),
        transp: body.transp.clone(),
        categories: body.categories.clone().unwrap_or_default(),
        location_id: None,
        organizer_user_id: Some(auth.user.id),
        organizer_email: auth.user.email.clone(),
        organizer_name: auth.user.display_name.clone(),
        master_id: body.master_event_id,
        recurrence_id: body.recurrence_id,
        recurrence_id_date: body.recurrence_id_date,
    };
    let attendees = body.attendees.clone().unwrap_or_default();
    let (event, etag) =
        db::create_event(&pool, calendar_id, auth.user.id, &attendees, &data).await?;
    if !attendees.is_empty() {
        db::scheduling::schedule_requests(&pool, event.id).await;
    }
    if let Ok(cal) = db::get_calendar(&pool, calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            "event_created",
            event.id,
            serde_json::json!({"summary": event.summary, "starts_at": event.starts_at}),
        )
        .await;
    }
    let rows = db::list_attendees(&pool, event.id).await?;
    Ok((StatusCode::CREATED, Json(event_view(&event, &etag, &rows))))
}

#[derive(serde::Deserialize)]
struct RangeQuery {
    from: Option<chrono::DateTime<Utc>>,
    to: Option<chrono::DateTime<Utc>>,
    // ponytail: no pagination yet; per-calendar windows stay small.
}

async fn list_events(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let from = query.from.unwrap_or(Utc::now() - Duration::days(30));
    let to = query.to.unwrap_or(Utc::now() + Duration::days(90));
    let events = db::list_events_in_range(&pool, calendar_id, from, to).await?;
    let out: Vec<serde_json::Value> = events
        .iter()
        .map(|e| event_view(e, &db::event_etag(e), &[]))
        .collect();
    Ok(Json(serde_json::json!(out)))
}

async fn get_event(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let (event, etag) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        event.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let rows = db::list_attendees(&pool, event.id).await?;
    Ok(Json(event_view(&event, &etag, &rows)))
}

async fn patch_event(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
    if_match: IfMatch,
    Json(body): Json<EventBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    let patch = db::EventPatch {
        summary: Some(body.summary),
        description_html: body.description_html,
        description_text: body.description_text,
        url: body.url,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        tzid: body.tzid,
        status: body.status,
        priority: body.priority,
        class: body.class,
        transp: body.transp,
    };
    let (event, etag) = db::update_event(&pool, event_id, if_match.0.as_deref(), &patch).await?;
    let rows = db::list_attendees(&pool, event.id).await?;
    Ok(Json(event_view(&event, &etag, &rows)))
}

async fn delete_event(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(event_id): Path<Uuid>,
    if_match: IfMatch,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::get_event(&pool, event_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    db::delete_event(&pool, event_id, if_match.0.as_deref()).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// If-Match header extractor; None when absent.
struct IfMatch(Option<String>);

impl axum::extract::FromRequestParts<AppState> for IfMatch {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get("if-match")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string()),
        ))
    }
}

/// Expanded occurrences with exceptions overlaid: each master occurrence
/// matching an exception's RECURRENCE-ID is replaced by that exception row.
async fn list_occurrences(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<RangeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let from = query.from.unwrap_or(Utc::now() - Duration::days(30));
    let to = query.to.unwrap_or(Utc::now() + Duration::days(90));
    let rows = db::list_events_in_range(&pool, calendar_id, from, to).await?;
    let master_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.master_event_id.is_none())
        .map(|r| r.id)
        .collect();
    let exceptions = db::list_exceptions(&pool, &master_ids).await?;

    let mut out: Vec<serde_json::Value> = Vec::new();
    for event in &rows {
        // Exception rows surface only through their master's overlay.
        if event.master_event_id.is_some() {
            continue;
        }
        if event.rrule.is_none() {
            out.push(serde_json::json!({"event": event_view(event, &db::event_etag(event), &[])}));
            continue;
        }
        let dtstart = match (event.starts_at, event.start_date) {
            (Some(at), _) => calendar_core::DateOrDateTime::Timed(at),
            (None, Some(date)) => calendar_core::DateOrDateTime::AllDay(date),
            _ => continue,
        };
        let rdate = json_to_points(&event.rdate);
        let exdate = json_to_points(&event.exdate);
        let expanded = calendar_core::recurrence::expand_occurrences(
            dtstart,
            event.tzid.as_deref(),
            Some(&event.rrule.clone().unwrap_or_default()),
            &rdate,
            &exdate,
            from,
            to,
        )
        .map_err(|e| AppError::bad_request(e.to_string()))?;
        for point in expanded {
            // Exceptions are keyed on the original wall-clock occurrence.
            let tz = calendar_core::recurrence::resolve_tz(event.tzid.as_deref());
            let wall: chrono::NaiveDateTime = match point {
                calendar_core::DateOrDateTime::Timed(at) => at.with_timezone(&tz).naive_local(),
                calendar_core::DateOrDateTime::AllDay(date) => date.and_hms_opt(0, 0, 0).unwrap(),
            };
            let matched = exceptions
                .iter()
                .find(|ex| ex.master_event_id == Some(event.id) && ex.recurrence_id == Some(wall));
            let (source, is_exception) = matched.map_or((event, false), |ex| (ex, true));
            let mut view = event_view(source, &db::event_etag(source), &[]);
            view["occurrence"] = match point {
                calendar_core::DateOrDateTime::Timed(at) => {
                    serde_json::json!({"kind": "timed", "at": at})
                }
                calendar_core::DateOrDateTime::AllDay(date) => {
                    serde_json::json!({"kind": "all_day", "date": date.to_string()})
                }
            };
            view["is_exception"] = serde_json::json!(is_exception);
            out.push(view);
        }
    }
    Ok(Json(serde_json::json!(out)))
}

fn json_to_points(value: &serde_json::Value) -> Vec<calendar_core::DateOrDateTime> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    if let Some(s) = v.as_str() {
                        if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                            return Some(calendar_core::DateOrDateTime::Timed(
                                at.with_timezone(&Utc),
                            ));
                        }
                        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                            return Some(calendar_core::DateOrDateTime::AllDay(d));
                        }
                    }
                    None
                })
                .collect()
        })
        .unwrap_or_default()
}

// ============ app state, errors, router ============

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub session_ttl: Duration,
    /// Envelope-encryption key for stored secrets (TOTP seeds, provider creds).
    pub crypto: Option<std::sync::Arc<calendar_auth::Crypto>>,
    pub passkeys: Option<std::sync::Arc<mfa::PasskeyStore>>,
    pub dav: Option<std::sync::Arc<dav_server::DavHandler<calendar_caldav::DavAuth>>>,
    pub config: Config,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    fn unauthorized() -> Self {
        Self::Unauthorized
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

impl From<db::DbError> for AppError {
    fn from(e: db::DbError) -> Self {
        match e {
            db::DbError::NotFound => AppError::Unauthorized,
            db::DbError::Conflict(msg) => AppError::Conflict(msg),
            other => AppError::Internal(other.to_string()),
        }
    }
}

impl From<AuthExtractError> for AppError {
    fn from(e: AuthExtractError) -> Self {
        match e {
            AuthExtractError::Unauthorized => AppError::Unauthorized,
            other => AppError::Internal(other.to_string()),
        }
    }
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(mfa::router())
        .merge(extras::router())
        .merge(sharing_api::router())
        .merge(rules_api::router())
        .merge(scheduling::router())
        .route(
            "/api/openapi.json",
            get(|| async { axum::Json(calendar_api::openapi_document()) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/tokens", post(create_token).get(list_tokens))
        .route("/api/auth/tokens/{id}", delete(revoke_token))
        .route(
            "/api/auth/app-passwords",
            post(create_app_password).get(list_app_passwords),
        )
        .route("/api/auth/app-passwords/{id}", delete(revoke_app_password))
        .route("/api/calendars", post(create_calendar).get(list_calendars))
        .route(
            "/api/calendars/{id}",
            get(get_calendar)
                .patch(patch_calendar)
                .delete(delete_calendar),
        )
        .route(
            "/api/calendars/{id}/acl",
            get(get_calendar_acl).put(put_calendar_acl),
        )
        .route(
            "/api/calendars/{id}/events",
            post(create_event).get(list_events),
        )
        .route("/api/calendars/{id}/occurrences", get(list_occurrences))
        .route(
            "/events/{id}",
            get(get_event).patch(patch_event).delete(delete_event),
        )
        .route("/calendars", axum::routing::any(dav::entry))
        .route("/calendars/", axum::routing::any(dav::entry))
        .route("/calendars/{*rest}", axum::routing::any(dav::entry))
        .route(
            "/.well-known/caldav",
            get(|| async { axum::response::Redirect::permanent("/calendars/") }),
        )
        .with_state(state)
}

// ============ commands ============

async fn serve(cfg: &Config) -> Result<()> {
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections)
        .await
        .context("connecting to PostgreSQL")?;
    db::migrate(&pool).await.context("running migrations")?;
    let crypto = calendar_auth::Crypto::from_hex_or_base64(cfg.encryption_key.as_deref())
        .map(std::sync::Arc::new)
        .inspect_err(|e| tracing::warn!("disabling encrypted secrets: {e}"))
        .ok();
    let passkeys = match (&cfg.webauthn_rp_id, &cfg.webauthn_origin) {
        (Some(rp_id), Some(origin)) => calendar_auth::webauthn::PasskeyManager::new(rp_id, origin)
            .map(|w| std::sync::Arc::new(mfa::PasskeyStore::new(w)))
            .inspect_err(|e| tracing::warn!("disabling WebAuthn: {e}"))
            .ok(),
        _ => {
            tracing::info!("WebAuthn disabled: WEBAUTHN_RP_ID/WEBAUTHN_ORIGIN not set");
            None
        }
    };
    let dav = Some(std::sync::Arc::new(
        dav_server::DavHandler::builder()
            .filesystem(Box::new(calendar_caldav::PgDavFs { pool: pool.clone() }))
            .principal("/calendars/")
            .build_handler(),
    ));
    // Background job worker: reminders and scheduled scans.
    tokio::spawn(jobs::run_worker(
        pool.clone(),
        format!("worker-{}", std::process::id()),
        crypto.clone(),
    ));
    let app = build_router(AppState {
        pool,
        session_ttl: cfg.session_ttl,
        crypto,
        passkeys,
        dav,
        config: cfg.clone(),
    });

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;
    tracing::info!("listening on {}", cfg.bind_addr);
    axum::serve(listener, app).await.context("serving")
}

async fn run_migrate(cfg: &Config) -> Result<()> {
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections)
        .await
        .context("connecting to PostgreSQL")?;
    db::migrate(&pool).await.context("running migrations")?;
    tracing::info!("migrations applied");
    Ok(())
}

async fn run_check(cfg: &Config) -> Result<()> {
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections).await?;
    sqlx::query("SELECT 1").execute(&pool).await?;
    let has_users: bool = sqlx::query_scalar("SELECT to_regclass('public.users') IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    tracing::info!(users_table = has_users, "database reachable");
    Ok(())
}

async fn run_backup(cfg: &Config) -> Result<()> {
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections).await?;
    let document = db::backup::export(&pool).await?;
    serde_json::to_writer(std::io::stdout(), &document)?;
    println!();
    Ok(())
}

async fn run_restore(cfg: &Config, path: &str) -> Result<()> {
    let document: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(path)?).context("reading backup file")?;
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections).await?;
    db::migrate(&pool).await.context("running migrations")?;
    db::backup::import(&pool, &document)
        .await
        .context("importing backup")?;
    tracing::info!("restore complete");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(&cfg).await,
        Command::Migrate => run_migrate(&cfg).await,
        Command::Check => run_check(&cfg).await,
        Command::Backup => run_backup(&cfg).await,
        Command::Restore { path } => run_restore(&cfg, &path).await,
    }
}
