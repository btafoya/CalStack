//! Journal (VJOURNAL) JSON API — docs/TASKS_JOURNALS_DESIGN.md section 7
//! (D9: the small case, one VJOURNAL per resource, no overrides).

use crate::{AppError, AppState, calendars_api::require_capability, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use calendar_db::{self as db};
use chrono::{DateTime, NaiveDate, Utc};
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct JournalBody {
    uid: Option<String>,
    summary: String,
    description_html: Option<String>,
    description_text: Option<String>,
    url: Option<String>,
    starts_at: Option<DateTime<Utc>>,
    start_date: Option<NaiveDate>,
    tzid: Option<String>,
    status: Option<String>,
    class: Option<String>,
    categories: Option<Vec<String>>,
}

fn validate_journal_body(body: &JournalBody) -> Result<(), AppError> {
    if body.starts_at.is_some() && body.start_date.is_some() {
        return Err(AppError::bad_request(
            "starts_at and start_date are mutually exclusive",
        ));
    }
    if let Some(s) = &body.status
        && !matches!(s.as_str(), "DRAFT" | "FINAL" | "CANCELLED")
    {
        return Err(AppError::bad_request(
            "status must be DRAFT, FINAL or CANCELLED",
        ));
    }
    if let Some(c) = &body.class
        && !matches!(c.as_str(), "PUBLIC" | "PRIVATE" | "CONFIDENTIAL")
    {
        return Err(AppError::bad_request(
            "class must be PUBLIC, PRIVATE or CONFIDENTIAL",
        ));
    }
    Ok(())
}

/// Journal view. Never includes extra_props (D9: they round-trip but stay
/// unmodelled and private).
pub(crate) fn journal_view(journal: &db::journals::JournalRow, etag: &str) -> serde_json::Value {
    serde_json::json!({
        "id": journal.id,
        "calendar_id": journal.calendar_id,
        "uid": journal.uid,
        "summary": journal.summary,
        "description_html": journal.description_html,
        "description_text": journal.description_text,
        "url": journal.url,
        "starts_at": journal.starts_at,
        "start_date": journal.start_date,
        "tzid": journal.tzid,
        "floating": journal.floating,
        "status": journal.status,
        "class": journal.class,
        "categories": journal.categories,
        "sequence": journal.sequence,
        "etag": etag,
        "created_at": journal.created_at,
        "updated_at": journal.updated_at,
    })
}

async fn create_journal(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    Json(body): Json<JournalBody>,
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
    validate_journal_body(&body)?;
    let data = db::journals::NewJournalData {
        uid: body
            .uid
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        summary: body.summary.clone(),
        description_html: body
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: body.description_text.clone(),
        url: body.url.clone(),
        starts_at: body.starts_at,
        start_date: body.start_date,
        tzid: body.tzid.clone(),
        status: body.status.clone(),
        class: body.class.clone(),
        categories: body.categories.clone().unwrap_or_default(),
        ..Default::default()
    };
    let (journal, etag) =
        db::journals::create_journal(&pool, calendar_id, auth.user.id, &data).await?;
    fire_journal_hooks(
        &pool,
        calendar_id,
        journal.id,
        "journal_created",
        crypto.as_deref(),
    )
    .await;
    Ok((StatusCode::CREATED, Json(journal_view(&journal, &etag))))
}

#[derive(serde::Deserialize)]
struct JournalListQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    undated: Option<bool>,
    category: Option<String>,
    q: Option<String>,
}

async fn list_journals(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(calendar_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<JournalListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_capability(
        &pool,
        calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    let filter = db::journals::JournalFilter {
        from: query.from,
        to: query.to,
        undated: query.undated.unwrap_or(false),
        category: query.category,
    };
    let rows = db::journals::list_journals(&pool, calendar_id, &filter).await?;
    let rows = match query.q.as_deref() {
        Some(q) => {
            let needle = q.to_lowercase();
            rows.into_iter()
                .filter(|j| j.summary.to_lowercase().contains(&needle))
                .collect::<Vec<_>>()
        }
        None => rows,
    };
    let out: Vec<serde_json::Value> = rows.iter().map(|j| journal_view(j, &j.etag)).collect();
    Ok(Json(serde_json::json!(out)))
}

async fn get_journal(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(journal_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let (journal, etag) = db::journals::get_journal(&pool, journal_id).await?;
    require_capability(
        &pool,
        journal.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadOnly,
    )
    .await?;
    Ok(Json(journal_view(&journal, &etag)))
}

async fn patch_journal(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(journal_id): Path<Uuid>,
    if_match: crate::tasks_api::IfMatch,
    Json(body): Json<JournalBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::journals::get_journal(&pool, journal_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    validate_journal_body(&body)?;
    let patch = db::journals::JournalPatch {
        summary: Some(body.summary),
        description_html: body
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: body.description_text,
        url: body.url,
        starts_at: body.starts_at,
        start_date: body.start_date,
        tzid: body.tzid,
        floating: None,
        status: body.status,
        class: body.class,
        categories: body.categories,
    };
    let (journal, etag) =
        db::journals::update_journal(&pool, journal_id, if_match.0.as_deref(), &patch).await?;
    fire_journal_hooks(
        &pool,
        journal.calendar_id,
        journal.id,
        "journal_updated",
        crypto.as_deref(),
    )
    .await;
    Ok(Json(journal_view(&journal, &etag)))
}

async fn delete_journal(
    State(AppState { pool, crypto, .. }): State<AppState>,
    headers: HeaderMap,
    Path(journal_id): Path<Uuid>,
    if_match: crate::tasks_api::IfMatch,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let (existing, _) = db::journals::get_journal(&pool, journal_id).await?;
    require_capability(
        &pool,
        existing.calendar_id,
        auth.user.id,
        calendar_core::CalendarCapability::ReadWrite,
    )
    .await?;
    db::journals::delete_journal(&pool, journal_id, if_match.0.as_deref()).await?;
    if let Ok(cal) = db::get_calendar(&pool, existing.calendar_id).await {
        crate::rules_api::run_rules(
            &pool,
            cal.tenant_id,
            cal.id,
            "journal_deleted",
            existing.id,
            serde_json::json!({"summary": existing.summary, "kind": "journal"}),
            crypto.as_deref(),
        )
        .await;
        crate::webhooks_api::fire(&pool, cal.tenant_id, existing.id, "journal_deleted").await;
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Rules + webhook triggers for journal mutations; fire-and-forget.
async fn fire_journal_hooks(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    journal_id: Uuid,
    trigger: &str,
    crypto: Option<&calendar_auth::Crypto>,
) {
    let Ok(cal) = db::get_calendar(pool, calendar_id).await else {
        return;
    };
    let summary: Option<String> = db::journals::get_journal(pool, journal_id)
        .await
        .ok()
        .map(|(j, _)| j.summary);
    crate::rules_api::run_rules(
        pool,
        cal.tenant_id,
        cal.id,
        trigger,
        journal_id,
        serde_json::json!({"summary": summary, "kind": "journal"}),
        crypto,
    )
    .await;
    crate::webhooks_api::fire(pool, cal.tenant_id, journal_id, trigger).await;
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/calendars/{id}/journals",
            post(create_journal).get(list_journals),
        )
        .route(
            "/api/journals/{id}",
            get(get_journal).patch(patch_journal).delete(delete_journal),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> db::journals::JournalRow {
        db::journals::JournalRow {
            id: Uuid::new_v4(),
            calendar_id: Uuid::new_v4(),
            uid: "uid-1".into(),
            href: Some("notes.ics".into()),
            starts_at: None,
            start_date: Some(NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()),
            tzid: None,
            floating: false,
            summary: "standup notes".into(),
            description_html: Some("<i>ok</i>".into()),
            description_text: Some("ok".into()),
            url: None,
            status: Some("FINAL".into()),
            class: Some("CONFIDENTIAL".into()),
            categories: vec!["work".into()],
            // extra_props must never leak through the view
            extra_props: serde_json::json!([{"name": "X-SECRET", "params": [], "value": "shh"}]),
            sequence: 1,
            etag: "stored-etag".into(),
            created_by: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn journal_view_shape_has_no_extra_props() {
        let j = row();
        let v = journal_view(&j, "etag-1");
        assert_eq!(v["summary"], "standup notes");
        assert_eq!(v["start_date"], "2026-09-20");
        assert_eq!(v["status"], "FINAL");
        assert_eq!(v["class"], "CONFIDENTIAL");
        assert_eq!(v["etag"], "etag-1");
        assert_eq!(v["sequence"], 1);
        assert!(v["extra_props"].is_null(), "extra_props must not leak");
        assert!(v["href"].is_null(), "href is CalDAV plumbing, not API");
    }
}
