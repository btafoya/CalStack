//! Remaining API surface: attachments, search, notifications, change
//! stream, audit (docs/PRD.md sections 12-13, 22; docs/IMPLEMENTATION_CHECKLIST.md
//! stages 12, 13, 19).

use std::collections::VecDeque;
use std::time::Duration;

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
    let event = db::attachments::event_of_attachment(&pool, attachment_id)
        .await
        // A gone attachment is a 404, not the global NotFound->Unauthorized
        // mapping (which exists to avoid leaking resource existence).
        .map_err(|e| match e {
            db::DbError::NotFound => AppError::NotFound,
            other => other.into(),
        })?;
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
    let event = db::attachments::event_of_attachment(&pool, attachment_id)
        .await
        // A gone attachment is a 404, not the global NotFound->Unauthorized
        // mapping (which exists to avoid leaking resource existence).
        .map_err(|e| match e {
            db::DbError::NotFound => AppError::NotFound,
            other => other.into(),
        })?;
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
    let event = db::attachments::event_of_attachment(&pool, attachment_id)
        .await
        // A gone attachment is a 404, not the global NotFound->Unauthorized
        // mapping (which exists to avoid leaking resource existence).
        .map_err(|e| match e {
            db::DbError::NotFound => AppError::NotFound,
            other => other.into(),
        })?;
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
    let query = db::search::SearchQuery {
        text: params.q.unwrap_or_default(),
        attendee: params.attendee,
        limit: params.limit.unwrap_or(50),
    };
    let hits = db::search::search_events(&pool, auth.user.id, &query).await?;
    let task_hits = db::search::search_tasks(&pool, auth.user.id, &query).await?;
    let journal_hits = db::search::search_journals(&pool, auth.user.id, &query).await?;
    // JSON arrays cannot carry keys, so "add tasks/journals beside the existing
    // top-level array" lands as an object: the event hits keep their exact
    // item shape under "events", "tasks"/"journals" are additive keys.
    Ok(Json(json!({
        "events": hits.iter().map(|hit| {
                let e = &hit.event;
                json!({
                    "id": e.id, "calendar_id": e.calendar_id, "uid": e.uid,
                    "summary": e.summary, "starts_at": e.starts_at, "ends_at": e.ends_at,
                    "start_date": e.start_date, "all_day": e.all_day,
                    "categories": e.categories, "rank": hit.rank,
                })
            })
            .collect::<Vec<_>>(),
        "tasks": task_hits.iter().map(|hit| {
                let t = &hit.task;
                json!({
                    "id": t.id, "calendar_id": t.calendar_id, "uid": t.uid,
                    "summary": t.summary, "starts_at": t.starts_at, "start_date": t.start_date,
                    "due_at": t.due_at, "due_date": t.due_date,
                    "status": t.status, "parent_uid": t.parent_uid, "url": t.url,
                    "categories": t.categories, "rank": hit.rank,
                })
            })
            .collect::<Vec<_>>(),
        "journals": journal_hits.iter().map(|hit| {
                let j = &hit.journal;
                json!({
                    "id": j.id, "calendar_id": j.calendar_id, "uid": j.uid,
                    "summary": j.summary, "starts_at": j.starts_at, "start_date": j.start_date,
                    "status": j.status, "url": j.url,
                    "categories": j.categories, "rank": hit.rank,
                })
            })
            .collect::<Vec<_>>(),
    })))
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

// ============ change stream: SSE adapter ============

/// How often the SSE adapter polls `change_log` for new entries.
const STREAM_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Connection cap; the terminal `done` frame tells the client to reconnect.
const STREAM_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Current head of the change log — the cursor handed to fresh SSE clients.
async fn max_change_seq(pool: &sqlx::PgPool) -> Result<i64, db::DbError> {
    let (seq,): (Option<i64>,) = sqlx::query_as("SELECT MAX(seq) FROM change_log")
        .fetch_one(pool)
        .await?;
    Ok(seq.unwrap_or(0))
}

/// Same shape as the `changes` array of the JSON poll (`GET /api/changes`).
fn change_frame(row: &db::search::ChangeRow) -> serde_json::Value {
    json!({
        "seq": row.seq, "calendar_id": row.calendar_id, "resource_id": row.resource_id,
        "operation": row.operation, "changed_at": row.changed_at,
    })
}

/// Cursor-only payload of the `sync` and `done` frames.
fn cursor_frame(seq: i64) -> serde_json::Value {
    json!({"seq": seq})
}

/// Frames pending on an SSE connection: a one-shot sync cursor, then changes.
enum Frame {
    Sync(i64),
    Change(db::search::ChangeRow),
}

fn frame_event(frame: Frame) -> axum::response::sse::Event {
    use axum::response::sse::Event;
    match frame {
        Frame::Sync(seq) => Event::default()
            .event("sync")
            .json_data(cursor_frame(seq))
            .expect("sync frame is valid JSON"),
        Frame::Change(row) => Event::default()
            .event("change")
            .json_data(change_frame(&row))
            .expect("change frame is valid JSON"),
    }
}

fn done_event(seq: i64) -> axum::response::sse::Event {
    use axum::response::sse::Event;
    Event::default()
        .event("done")
        .json_data(cursor_frame(seq))
        .expect("done frame is valid JSON")
}

/// Per-connection state for `GET /api/changes/stream`.
struct ChangeStreamState {
    pool: sqlx::PgPool,
    user_id: Uuid,
    since: i64,
    pending: VecDeque<Frame>,
    deadline: tokio::time::Instant,
    finished: bool,
}

/// Drains buffered frames, then polls for new changes until the 30-minute
/// cap; ends after yielding the terminal `done` frame (client reconnects with
/// its last `seq`). Dropped when the client disconnects.
async fn next_change_stream_state(
    mut st: ChangeStreamState,
) -> Option<(
    Result<axum::response::sse::Event, std::convert::Infallible>,
    ChangeStreamState,
)> {
    if st.finished {
        return None;
    }
    loop {
        if let Some(frame) = st.pending.pop_front() {
            if let Frame::Change(ref row) = frame {
                st.since = row.seq;
            }
            return Some((Ok(frame_event(frame)), st));
        }
        if tokio::time::Instant::now() >= st.deadline {
            st.finished = true;
            return Some((Ok(done_event(st.since)), st));
        }
        // Same tenant/ACL scoping as the JSON poll (`list_changes_since`).
        match db::search::list_changes_since(&st.pool, st.user_id, st.since, 500).await {
            Ok(rows) => {
                st.pending.extend(rows.into_iter().map(Frame::Change));
                if st.pending.is_empty() {
                    tokio::time::sleep(STREAM_POLL_INTERVAL).await;
                }
            }
            Err(e) => {
                // Transient DB errors keep the stream alive; the client still
                // holds its cursor if the server dies outright.
                tracing::warn!(error = %e, "change stream poll failed");
                tokio::time::sleep(STREAM_POLL_INTERVAL).await;
            }
        }
    }
}

/// SSE transport adapter for the application change stream (docs/PRD.md
/// section 11). Cursor is `?since=N` with the same semantics as the JSON
/// poll; a fresh connect (no `since`) starts from the current head.
#[derive(Deserialize)]
struct StreamChangesQuery {
    since: Option<i64>,
}

async fn stream_changes(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<StreamChangesQuery>,
) -> Result<impl IntoResponse, AppError> {
    use axum::response::sse::{KeepAlive, Sse};
    let auth = resolve_auth(&pool, &headers).await?;
    let mut pending = VecDeque::new();
    let since = match params.since {
        Some(s) => s,
        None => {
            let head = max_change_seq(&pool).await?;
            pending.push_back(Frame::Sync(head));
            head
        }
    };
    let state = ChangeStreamState {
        pool,
        user_id: auth.user.id,
        since,
        pending,
        deadline: tokio::time::Instant::now() + STREAM_MAX_LIFETIME,
        finished: false,
    };
    Ok(Sse::new(futures_util::stream::unfold(
        state,
        next_change_stream_state,
    ))
    .keep_alive(KeepAlive::default()))
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
        .route("/api/changes/stream", get(stream_changes))
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/{id}/read", post(mark_notification_read))
        .route("/api/audit", get(list_audit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_change(seq: i64) -> db::search::ChangeRow {
        db::search::ChangeRow {
            seq,
            calendar_id: Uuid::nil(),
            resource_id: Uuid::nil(),
            operation: "upsert".to_string(),
            changed_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn change_frame_matches_the_json_poll_shape() {
        let frame = change_frame(&sample_change(7));
        assert_eq!(frame["seq"], 7);
        assert_eq!(frame["calendar_id"], json!(Uuid::nil()));
        assert_eq!(frame["resource_id"], json!(Uuid::nil()));
        assert_eq!(frame["operation"], "upsert");
        assert!(frame["changed_at"].is_string());
        assert_eq!(
            frame.as_object().unwrap().len(),
            5,
            "frame keys must stay aligned with GET /api/changes"
        );
    }

    #[test]
    fn sync_and_done_frames_carry_only_the_cursor() {
        assert_eq!(cursor_frame(12), json!({"seq": 12}));
    }

    #[test]
    fn every_frame_serializes_through_the_sse_wire_path() {
        // json_data is the actual encoding path; any failure is a panic at
        // runtime, so prove the builders survive it.
        let _ = frame_event(Frame::Sync(1));
        let _ = frame_event(Frame::Change(sample_change(2)));
        let _ = done_event(3);
    }
}
