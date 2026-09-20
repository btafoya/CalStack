//! audit_log write side (PRD §22): one row per authenticated API mutation,
//! written centrally so per-handler edits stay unnecessary. Login and
//! passkey logins audit explicitly (their credentials are not in headers).
//! Rows are metadata only — no request bodies, never event contents or
//! credentials.

use crate::{AppState, resolve_auth};
use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use uuid::Uuid;

pub(crate) async fn middleware(
    State(AppState { pool, .. }): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let response = next.run(req).await;
    let mutating = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !mutating || !path.starts_with("/api/") {
        return response;
    }
    if let Ok(auth) = resolve_auth(&pool, &headers).await {
        let actor_type = if auth.session.is_some() {
            "session"
        } else {
            "token"
        };
        write(
            &pool,
            actor_type,
            Some(auth.user.id),
            method.as_str(),
            object_type(&path),
            object_id(&path),
            Some(serde_json::json!({
                "path": path,
                "status": response.status().as_u16(),
            })),
        )
        .await;
    }
    response
}

/// Never fails a mutation: an audit insert error is swallowed (the read side
/// can tolerate a missing row; a lost calendar change cannot).
pub(crate) async fn write(
    pool: &sqlx::PgPool,
    actor_type: &str,
    actor_id: Option<Uuid>,
    action: &str,
    object_type: &str,
    object_id: Option<Uuid>,
    summary: Option<Value>,
) {
    let _ = sqlx::query(
        "INSERT INTO audit_log (actor_type, actor_id, action, object_type, object_id, change_summary)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor_type)
    .bind(actor_id)
    .bind(action)
    .bind(object_type)
    .bind(object_id)
    .bind(summary)
    .execute(pool)
    .await;
}

fn object_type(path: &str) -> &str {
    let segment = path
        .strip_prefix("/api/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("api");
    // "calendars" → "calendar"; leave longer compound names as-is.
    if segment.len() > 3 && segment.ends_with('s') {
        &segment[..segment.len() - 1]
    } else {
        segment
    }
}

fn object_id(path: &str) -> Option<Uuid> {
    path.split('/')
        .filter_map(|segment| Uuid::parse_str(segment).ok())
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_type_trims_plural() {
        assert_eq!(object_type("/api/events/123"), "event");
        assert_eq!(
            object_type("/api/notification-providers"),
            "notification-provider"
        );
        assert_eq!(object_type("/api/auth/tokens"), "auth");
    }

    #[test]
    fn object_id_takes_last_uuid_segment() {
        let id = Uuid::new_v4();
        assert_eq!(object_id(&format!("/api/calendars/{id}/events")), Some(id));
        assert_eq!(object_id("/api/auth/password"), None);
    }
}
