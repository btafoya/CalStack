//! Single production executable: HTTP server, CLI commands.
#![recursion_limit = "256"] // the event JSON literal in `event_json` is large

mod admin_api;
mod audit;
mod auth;
mod calendars_api;
mod capture;
mod categories_api;
mod contacts_api;
mod dav;
mod events_api;
mod extras;
mod jobs;
mod journals_api;
mod mfa;
mod places;
mod push_api;
mod rules_api;
mod scheduling;
mod sharing_api;
mod tasks_api;
mod webhooks_api;
mod xml;

pub(crate) use auth::{
    require_admin, require_csrf, require_session, require_session_mutation, resolve_auth,
    set_session_cookie,
};
pub(crate) use calendars_api::require_capability;

use anyhow::{Context, Result};
use axum::{Json, Router, response::IntoResponse, routing::get};
use calendar_core::validate_username;
use calendar_db::{self as db};
use chrono::Duration;
use clap::{Parser, Subcommand};
use utoipa::OpenApi;
use uuid::Uuid;

/// Session cookie + bearer token, the API's two authentication schemes
/// (alternatives; every /api path accepts either unless the handler refines).
struct SecuritySchemes;

impl utoipa::Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, SecurityScheme};
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "sessionCookie",
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                    "session",
                    "session cookie set by POST /api/auth/login or the passkey ceremony",
                ))),
            );
            components.add_security_scheme(
                "bearerToken",
                SecurityScheme::Http(
                    utoipa::openapi::security::HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some("scoped API token from POST /api/auth/tokens"))
                        .build(),
                ),
            );
        }
    }
}

/// The single served OpenAPI document, assembled from the per-module
/// `#[derive(OpenApi)]` structs next to each router.
#[derive(OpenApi)]
#[openapi(
    info(title = "CalStack API"),
    security(("sessionCookie" = []), ("bearerToken" = [])),
    modifiers(&SecuritySchemes)
)]
struct ApiDoc;

/// The `{"ok": true}` acknowledgement shared by several endpoints.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct OkView {
    pub(crate) ok: bool,
}

/// Attendee as surfaced in event and task responses (identical shape in both;
/// calendar-db's AttendeeRow and tasks::TaskAttendeeRow are distinct structs,
/// so callers construct this from whichever they hold).
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AttendeeView {
    pub(crate) email: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) telephone: Option<String>,
    pub(crate) role: String,
    pub(crate) partstat: String,
    pub(crate) rsvp: Option<bool>,
    pub(crate) contact_id: Option<Uuid>,
    pub(crate) user_id: Option<Uuid>,
}

/// The served document: utoipa-generated from handler annotations, merged
/// per module in `openapi_json()`.
fn openapi_json() -> serde_json::Value {
    let mut doc = ApiDoc::openapi();
    doc.merge(auth::AuthApi::openapi());
    doc.merge(mfa::MfaApi::openapi());
    doc.merge(calendars_api::CalendarsApi::openapi());
    doc.merge(events_api::EventsApi::openapi());
    doc.merge(tasks_api::TasksApi::openapi());
    doc.merge(journals_api::JournalsApi::openapi());
    doc.merge(sharing_api::SharingApi::openapi());
    doc.merge(webhooks_api::WebhooksApi::openapi());
    doc.merge(push_api::PushApi::openapi());
    doc.merge(rules_api::RulesApi::openapi());
    doc.merge(categories_api::CategoriesApi::openapi());
    doc.merge(contacts_api::ContactsApi::openapi());
    doc.merge(admin_api::AdminApi::openapi());
    doc.merge(extras::ExtrasApi::openapi());
    doc.merge(places::PlacesApi::openapi());
    serde_json::to_value(doc).expect("generated OpenAPI serializes")
}

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
    /// Create a user with is_admin=true.
    CreateAdmin {
        username: String,
        email: String,
        password: String,
    },
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
    /// Google Places API key; enables place autocomplete in the web UI.
    pub places_api_key: Option<String>,
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
            places_api_key: env("GOOGLE_MAPS_API_KEY").filter(|k| !k.is_empty()),
        })
    }
}

// ============ app state, errors, router ============

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub session_ttl: Duration,
    /// Envelope-encryption key for stored secrets (TOTP seeds, provider creds).
    pub crypto: Option<std::sync::Arc<calendar_auth::Crypto>>,
    pub passkeys: Option<std::sync::Arc<mfa::PasskeyStore>>,
    pub dav: Option<std::sync::Arc<dav_server::DavHandler<calendar_caldav::DavAuth>>>,
    pub dav_carddav: Option<std::sync::Arc<dav_server::DavHandler<calendar_carddav::DavAuth>>>,
    pub config: Config,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppError {
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
            AppError::BadRequest(msg) => (axum::http::StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized => (axum::http::StatusCode::UNAUTHORIZED, "unauthorized".into()),
            AppError::NotFound => (axum::http::StatusCode::NOT_FOUND, "not found".into()),
            AppError::Forbidden => (axum::http::StatusCode::FORBIDDEN, "forbidden".into()),
            AppError::Conflict(msg) => (axum::http::StatusCode::CONFLICT, msg.clone()),
            AppError::Internal(msg) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
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

impl From<auth::AuthExtractError> for AppError {
    fn from(e: auth::AuthExtractError) -> Self {
        match e {
            auth::AuthExtractError::Unauthorized => AppError::Unauthorized,
            other => AppError::Internal(other.to_string()),
        }
    }
}

fn security_headers() -> tower_http::set_header::SetResponseHeaderLayer<axum::http::HeaderValue> {
    tower_http::set_header::SetResponseHeaderLayer::overriding(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    )
}

fn frame_deny() -> tower_http::set_header::SetResponseHeaderLayer<axum::http::HeaderValue> {
    tower_http::set_header::SetResponseHeaderLayer::overriding(
        axum::http::header::X_FRAME_OPTIONS,
        axum::http::HeaderValue::from_static("DENY"),
    )
}

fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .merge(calendar_web::router())
        .merge(mfa::router())
        .merge(extras::router())
        .merge(push_api::router())
        .merge(sharing_api::router())
        .merge(rules_api::router())
        .merge(categories_api::router())
        .merge(contacts_api::router())
        .merge(scheduling::router())
        .merge(admin_api::router())
        .merge(auth::router())
        .merge(places::router())
        .merge(calendars_api::router())
        .merge(events_api::router())
        .merge(tasks_api::router())
        .merge(journals_api::router())
        .merge(webhooks_api::router())
        .route("/api/openapi.json", get(|| async { Json(openapi_json()) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/calendars", axum::routing::any(dav::entry))
        .route("/calendars/", axum::routing::any(dav::entry))
        .route("/calendars/{*rest}", axum::routing::any(dav::entry))
        .route(
            "/.well-known/caldav",
            axum::routing::any(|| async { axum::response::Redirect::permanent("/calendars/") }),
        )
        .route("/contacts", axum::routing::any(dav::entry_carddav))
        .route("/contacts/", axum::routing::any(dav::entry_carddav))
        .route("/contacts/{*rest}", axum::routing::any(dav::entry_carddav))
        .route(
            "/.well-known/carddav",
            axum::routing::any(|| async { axum::response::Redirect::permanent("/contacts/") }),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::admin_page_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::token_scope_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            audit::middleware,
        ))
        .layer(security_headers())
        .layer(frame_deny())
        .with_state(state);
    // Outermost, so requests the guards reject are captured too.
    if capture::dir().is_some() {
        router.layer(axum::middleware::from_fn(capture::middleware))
    } else {
        router
    }
}

// ============ commands ============

async fn serve(cfg: &Config) -> Result<()> {
    if let Some(dir) = capture::dir() {
        tracing::warn!(
            dir = %dir.display(),
            "DAV_CAPTURE_DIR set: DAV requests, including calendar content, are written to disk (dev only)"
        );
    }
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
    let dav_carddav = Some(std::sync::Arc::new(
        dav_server::DavHandler::builder()
            .filesystem(Box::new(calendar_carddav::PgAddressBookFs {
                pool: pool.clone(),
            }))
            .principal("/contacts/")
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
        dav_carddav,
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

async fn run_create_admin(cfg: &Config, username: &str, email: &str, password: &str) -> Result<()> {
    validate_username(username)?;
    calendar_core::validate_email(email)?;
    if password.len() < 8 {
        anyhow::bail!("password must be at least 8 characters");
    }
    let hash = calendar_auth::hash_password(password)?;
    let pool = db::connect(&cfg.database_url, cfg.database_max_connections).await?;
    let user = db::create_user(&pool, username, email, None, &hash).await?;
    sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
        .bind(user.id)
        .execute(&pool)
        .await?;
    tracing::info!(username, "admin user created");
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
        Command::CreateAdmin {
            username,
            email,
            password,
        } => run_create_admin(&cfg, &username, &email, &password).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The served document is 3.1.0 with the full /api inventory pinned (the
    /// pin lists below ARE the completeness check — dropping an annotation or
    /// its registration fails here, not in a codegen consumer).
    #[test]
    fn openapi_is_31_and_covers_the_api_inventory() {
        let doc = openapi_json();
        assert_eq!(doc["openapi"], "3.1.0");
        assert!(doc["components"]["securitySchemes"]["sessionCookie"].is_object());
        assert!(doc["components"]["securitySchemes"]["bearerToken"].is_object());
        assert!(doc["security"].is_array());
        // Auth surface (stage 2):
        for (path, method) in [
            ("/api/auth/register", "post"),
            ("/api/auth/login", "post"),
            ("/api/auth/logout", "post"),
            ("/api/auth/me", "get"),
            ("/api/auth/password", "post"),
            ("/api/auth/notify-prefs", "post"),
            ("/api/auth/tokens", "post"),
            ("/api/auth/tokens", "get"),
            ("/api/auth/tokens/{id}", "delete"),
            ("/api/auth/app-passwords", "post"),
            ("/api/auth/app-passwords", "get"),
            ("/api/auth/app-passwords/{id}", "delete"),
            ("/api/auth/totp/setup", "post"),
            ("/api/auth/totp/verify", "post"),
            ("/api/auth/totp", "get"),
            ("/api/auth/totp", "delete"),
            ("/api/auth/webauthn/register/start", "post"),
            ("/api/auth/webauthn/register/finish", "post"),
            ("/api/auth/webauthn/login/start", "post"),
            ("/api/auth/webauthn/login/finish", "post"),
            ("/api/auth/webauthn", "get"),
            ("/api/auth/webauthn/{id}", "delete"),
        ] {
            assert!(
                doc["paths"][path][method].is_object(),
                "{method} {path} missing from the generated document"
            );
        }
        assert!(doc["components"]["schemas"]["UserView"].is_object());
        // Calendaring core is utoipa-generated since stage 3 (legacy fragment
        // no longer lists these either):
        for (path, method) in [
            ("/api/calendars", "post"),
            ("/api/calendars", "get"),
            ("/api/calendars/{id}", "get"),
            ("/api/calendars/{id}", "patch"),
            ("/api/calendars/{id}", "delete"),
            ("/api/calendars/{id}/acl", "get"),
            ("/api/calendars/{id}/acl", "put"),
            ("/api/calendars/{id}/events", "post"),
            ("/api/calendars/{id}/events", "get"),
            ("/api/calendars/{id}/occurrences", "get"),
            ("/api/calendars/{id}/tasks", "post"),
            ("/api/calendars/{id}/tasks", "get"),
            ("/api/tasks/{id}", "get"),
            ("/api/tasks/{id}", "patch"),
            ("/api/tasks/{id}", "delete"),
            ("/api/tasks/{id}/complete", "post"),
            ("/api/tasks/{id}/reopen", "post"),
            ("/api/calendars/{id}/journals", "post"),
            ("/api/calendars/{id}/journals", "get"),
            ("/api/journals/{id}", "get"),
            ("/api/journals/{id}", "patch"),
            ("/api/journals/{id}", "delete"),
            ("/api/events/{id}", "get"),
            ("/api/events/{id}", "patch"),
            ("/api/events/{id}", "delete"),
            ("/api/events/{id}/attendees/self", "patch"),
            ("/api/subscriptions/{id}/occurrences", "get"),
        ] {
            assert!(
                doc["paths"][path][method].is_object(),
                "{method} {path} missing from the generated document"
            );
        }
        assert!(doc["components"]["schemas"]["CalendarView"].is_object());
        assert!(doc["components"]["schemas"]["EventView"].is_object());
        assert!(doc["components"]["schemas"]["TaskView"].is_object());
        assert!(doc["components"]["schemas"]["JournalView"].is_object());
        // Stage 4 modules; the legacy fragment now lists no paths at all, so
        // this pin list is the complete /api inventory:
        for (path, method) in [
            ("/api/calendars/{id}/shares", "post"),
            ("/api/calendars/{id}/shares", "get"),
            ("/api/calendars/{id}/shares/{share_id}", "delete"),
            ("/api/subscriptions", "post"),
            ("/api/subscriptions", "get"),
            ("/api/subscriptions/{id}", "delete"),
            ("/api/webhooks", "post"),
            ("/api/webhooks", "get"),
            ("/api/webhooks/{id}", "get"),
            ("/api/webhooks/{id}", "patch"),
            ("/api/webhooks/{id}", "delete"),
            ("/api/webhooks/{id}/test", "post"),
            ("/api/webhooks/{id}/deliveries", "get"),
            ("/api/push/subscriptions", "post"),
            ("/api/push/subscriptions", "delete"),
            ("/api/push/public-key", "get"),
            ("/api/notification-providers", "post"),
            ("/api/notification-providers", "get"),
            ("/api/notification-providers/{id}", "get"),
            ("/api/notification-providers/{id}", "patch"),
            ("/api/notification-providers/{id}", "delete"),
            ("/api/notification-providers/{id}/test", "post"),
            ("/api/rules", "post"),
            ("/api/rules", "get"),
            ("/api/rules/{id}", "patch"),
            ("/api/rules/{id}", "delete"),
            ("/api/categories", "post"),
            ("/api/categories", "get"),
            ("/api/categories/{id}", "patch"),
            ("/api/categories/{id}", "delete"),
            ("/api/addressbooks", "post"),
            ("/api/addressbooks", "get"),
            ("/api/addressbooks/{id}", "patch"),
            ("/api/addressbooks/{id}", "delete"),
            ("/api/addressbooks/{id}/contacts", "post"),
            ("/api/addressbooks/{id}/contacts", "get"),
            ("/api/contacts/autocomplete", "get"),
            ("/api/contacts/{id}", "get"),
            ("/api/contacts/{id}", "patch"),
            ("/api/contacts/{id}", "delete"),
            ("/api/contacts/{id}/photo", "get"),
            ("/api/contacts/{id}/photo", "put"),
            ("/api/admin/users", "get"),
            ("/api/admin/users", "post"),
            ("/api/admin/users/{id}", "patch"),
            ("/api/calendars/{id}/events/{event_id}/attachments", "post"),
            ("/api/calendars/{id}/events/{event_id}/attachments", "get"),
            ("/api/attachments/{id}", "get"),
            ("/api/attachments/{id}", "delete"),
            ("/api/attachments/{id}/meta", "get"),
            ("/api/search", "get"),
            ("/api/changes", "get"),
            ("/api/changes/stream", "get"),
            ("/api/notifications", "get"),
            ("/api/notifications/{id}/read", "post"),
            ("/api/audit", "get"),
            ("/api/places/autocomplete", "get"),
            ("/api/places/{place_id}", "get"),
        ] {
            assert!(
                doc["paths"][path][method].is_object(),
                "{method} {path} missing from the generated document"
            );
        }
        assert!(doc["components"]["schemas"]["ContactView"].is_object());
        assert!(doc["components"]["schemas"]["WebhookView"].is_object());
        assert!(doc["components"]["schemas"]["ProviderView"].is_object());
        assert!(doc["components"]["schemas"]["RuleView"].is_object());
    }
}
