//! Remaining API surface: attachments, search, notifications, change
//! stream, audit (docs/PRD.md sections 12-13, 22; docs/IMPLEMENTATION_CHECKLIST.md
//! stages 12, 13, 19).

use crate::{AppError, AppState, require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use base64::Engine;
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

// ============ attachments ============

#[derive(serde::Deserialize)]
struct AttachmentBody {
    filename: String,
    content_type: String,
    /// base64-encoded bytes (ADR-010 caps the size in the app layer).
    data: String,
}

async fn create_attachment(
    State(AppState { pool, config, .. }): State<AppState>,
    headers: HeaderMap,
    Path((calendar_id, event_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AttachmentBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        CalendarCapability::ReadWrite,
    )
    .await?;
    let (event, _) = db::get_event(&pool, event_id).await?;
    if event.calendar_id != calendar_id {
        return Err(AppError::NotFound);
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.data)
        .map_err(|_| AppError::bad_request("data is not valid base64"))?;
    let max = config.attachment_max_bytes as usize;
    if data.len() > max {
        return Err(AppError::bad_request(format!(
            "attachment exceeds the {} byte cap",
            max
        )));
    }
    let row = db::attachments::create_attachment(
        &pool,
        event_id,
        &body.filename,
        &body.content_type,
        &data,
        &calendar_auth::sha256(&data),
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "id": row.id, "filename": row.filename, "content_type": row.content_type,
            "byte_size": row.byte_size, "created_at": row.created_at,
        })),
    ))
}

async fn list_attachments(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path((calendar_id, event_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        CalendarCapability::ReadOnly,
    )
    .await?;
    let (event, _) = db::get_event(&pool, event_id).await?;
    if event.calendar_id != calendar_id {
        return Err(AppError::NotFound);
    }
    let rows = db::attachments::list_attachments(&pool, event_id).await?;
    Ok(Json(json!(
        rows.iter()
            .map(|r| json!({
                "id": r.id, "filename": r.filename, "content_type": r.content_type,
                "byte_size": r.byte_size, "sha256": hex(&r.sha256), "created_at": r.created_at,
            }))
            .collect::<Vec<_>>()
    )))
}

/// Attachment download, guarded by the event's calendar ACL.
async fn get_attachment(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(attachment_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let event = db::attachments::event_of_attachment(&pool, attachment_id).await?;
    require_capability(
        &pool,
        event.calendar_id,
        auth.user.id,
        CalendarCapability::ReadOnly,
    )
    .await?;
    let (meta, data) = db::attachments::get_attachment(&pool, attachment_id).await?;
    let headers = [
        ("content-type", meta.content_type.clone()),
        (
            "content-disposition",
            format!(
                "attachment; filename=\"{}\"",
                meta.filename.replace('"', "")
            ),
        ),
    ];
    Ok((headers, data))
}

/// Attachment metadata only (no bytes), same ACL as the download.
async fn get_attachment_meta(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(attachment_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let event = db::attachments::event_of_attachment(&pool, attachment_id).await?;
    require_capability(
        &pool,
        event.calendar_id,
        auth.user.id,
        CalendarCapability::ReadOnly,
    )
    .await?;
    let meta = db::attachments::get_attachment_meta(&pool, attachment_id).await?;
    Ok(Json(json!({
        "id": meta.id, "filename": meta.filename, "content_type": meta.content_type,
        "byte_size": meta.byte_size, "sha256": hex(&meta.sha256), "created_at": meta.created_at,
    })))
}

async fn delete_attachment(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(attachment_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let event = db::attachments::event_of_attachment(&pool, attachment_id).await?;
    require_capability(
        &pool,
        event.calendar_id,
        auth.user.id,
        CalendarCapability::ReadWrite,
    )
    .await?;
    db::attachments::delete_attachment(&pool, attachment_id).await?;
    Ok(Json(json!({"ok": true})))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ============ search ============

#[derive(Deserialize)]
struct SearchQueryParams {
    q: Option<String>,
    attendee: Option<String>,
    limit: Option<i64>,
}

async fn search(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchQueryParams>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    if params.q.is_none() && params.attendee.is_none() {
        return Err(AppError::bad_request("q or attendee is required"));
    }
    let hits = db::search::search_events(
        &pool,
        auth.user.id,
        &db::search::SearchQuery {
            text: params.q.unwrap_or_default(),
            attendee: params.attendee,
            limit: params.limit.unwrap_or(50),
        },
    )
    .await?;
    Ok(Json(json!(
        hits.iter()
            .map(|hit| {
                let e = &hit.event;
                json!({
                    "id": e.id, "calendar_id": e.calendar_id, "uid": e.uid,
                    "summary": e.summary, "starts_at": e.starts_at, "ends_at": e.ends_at,
                    "start_date": e.start_date, "all_day": e.all_day,
                    "categories": e.categories, "rank": hit.rank,
                })
            })
            .collect::<Vec<_>>()
    )))
}

/// Application change stream (docs/PRD.md section 11): change_log entries
/// after a sequence, across every readable calendar.
#[derive(Deserialize)]
struct ChangesQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn list_changes(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ChangesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows = db::search::list_changes_since(
        &pool,
        auth.user.id,
        params.since.unwrap_or(0),
        params.limit.unwrap_or(500),
    )
    .await?;
    Ok(Json(json!({
        "changes": rows.iter().map(|c| json!({
            "seq": c.seq, "calendar_id": c.calendar_id, "resource_id": c.resource_id,
            "operation": c.operation, "changed_at": c.changed_at,
        })).collect::<Vec<_>>(),
    })))
}

// ============ notifications ============

type NotificationRow = (
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn list_notifications(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let rows: Vec<NotificationRow> = sqlx::query_as(
        "SELECT id, channel, title, body, read_at FROM notifications
             WHERE user_id = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(auth.user.id)
    .fetch_all(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter()
            .map(|(id, channel, title, body, read_at)| json!({
                "id": id, "channel": channel, "title": title, "body": body,
                "read_at": read_at, "read": read_at.is_some(),
            }))
            .collect::<Vec<_>>()
    )))
}

async fn mark_notification_read(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(notification_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    sqlx::query("UPDATE notifications SET read_at = now() WHERE id = $1 AND user_id = $2")
        .bind(notification_id)
        .bind(auth.user.id)
        .execute(&pool)
        .await
        .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!({"ok": true})))
}

// ============ audit (read-only; admin) ============

type AuditRow = (
    i64,
    Option<Uuid>,
    String,
    String,
    Option<serde_json::Value>,
    chrono::DateTime<chrono::Utc>,
);

async fn list_audit(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ChangesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    if !auth.user.is_admin {
        return Err(AppError::Forbidden);
    }
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT seq, tenant_id, action, object_type, change_summary, created_at
             FROM audit_log ORDER BY seq DESC LIMIT $1",
    )
    .bind(params.limit.unwrap_or(100).min(1000))
    .fetch_all(&pool)
    .await
    .map_err(|e| AppError::from(db::DbError::Sql(e)))?;
    Ok(Json(json!(
        rows.iter()
            .map(|(seq, tenant, action, object, summary, at)| json!({
                "seq": seq, "tenant_id": tenant, "action": action,
                "object_type": object, "change_summary": summary, "created_at": at,
            }))
            .collect::<Vec<_>>()
    )))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/calendars/{id}/events/{event_id}/attachments",
            post(create_attachment).get(list_attachments),
        )
        .route(
            "/api/attachments/{id}",
            get(get_attachment).delete(delete_attachment),
        )
        .route("/api/attachments/{id}/meta", get(get_attachment_meta))
        .route("/api/search", get(search))
        .route("/api/changes", get(list_changes))
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/{id}/read", post(mark_notification_read))
        .route("/api/audit", get(list_audit))
}
