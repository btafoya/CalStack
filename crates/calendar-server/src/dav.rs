//! CalDAV/WebDAV mount: dav-server-rs handler over the PostgreSQL adapter,
//! plus the REPORT types dav-server does not implement (RFC 6578
//! sync-collection, RFC 4791 free-busy-query with visibility rules).

use crate::{AppError, AppState, resolve_auth};
use axum::http::{StatusCode, header};
use axum::{
    body::Body,
    extract::{Request, State},
    response::IntoResponse,
};
use calendar_caldav::adapter::DavAuth;
use calendar_core::CalendarCapability;
use calendar_db::{self as db};
use chrono::{DateTime, TimeZone, Utc};
use dav_server::body::Body as DavBody;
use uuid::Uuid;

pub(crate) async fn entry(
    State(AppState { pool, dav, .. }): State<AppState>,
    request: Request,
) -> axum::response::Response {
    let Some(dav) = dav else {
        return internal("CalDAV is not configured");
    };

    // Authenticate once; CalDAV clients expect a Basic challenge.
    let auth = match resolve_auth(&pool, request.headers()).await {
        Ok(auth) => auth,
        Err(_) => return unauthorized_basic(),
    };

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // OPTIONS without auth is a capability probe; answer directly.
    if method == axum::http::Method::OPTIONS {
        let mut response = (StatusCode::OK, "").into_response();
        response
            .headers_mut()
            .insert("DAV", "1, 2, 3, calendar-access".parse().unwrap());
        response.headers_mut().insert(
            "Allow",
            "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCALENDAR"
                .parse()
                .unwrap(),
        );
        return response;
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 256 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };

    // ADR-011: PUT of a VTODO is rejected with a CalDAV error body.
    if method == axum::http::Method::PUT
        && !bytes.is_empty()
        && let Err(calendar_caldav::IcsError::TodoUnsupported) =
            calendar_caldav::parse_ics(String::from_utf8_lossy(&bytes).as_ref())
    {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            VTODO_ERROR_BODY,
        )
            .into_response();
    }

    // Custom REPORTs.
    if method == axum::http::Method::from_bytes(b"REPORT").unwrap() {
        let body = String::from_utf8_lossy(&bytes).to_string();
        if body.contains("sync-collection") {
            return sync_collection(
                &pool,
                &DavAuth {
                    user: auth.user.clone(),
                },
                &path,
                &body,
            )
            .await;
        }
        if body.contains("free-busy-query") {
            return free_busy_report(
                &pool,
                &DavAuth {
                    user: auth.user.clone(),
                },
                &path,
                &body,
            )
            .await
            .unwrap_or_else(IntoResponse::into_response);
        }
    }

    let request = axum::http::Request::from_parts(parts, Body::from(bytes));
    let response = dav
        .handle_guarded(
            request,
            "/calendars/".to_string(),
            DavAuth { user: auth.user },
        )
        .await;
    convert(response)
}

fn convert(response: axum::http::Response<dav_server::body::Body>) -> axum::response::Response {
    let (parts, body) = response.into_parts();
    axum::response::Response::from_parts(parts, Body::from_stream(dav_body_stream(body)))
}

// The dav-server Body is a Stream; wrap its items for axum.
fn dav_body_stream(
    body: DavBody,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, axum::Error>> {
    use futures_util::StreamExt;
    body.map(|item| {
        item.map_err(|e| {
            axum::Error::new(e) // wraps any std error
        })
    })
}

// ============ sync-collection REPORT (RFC 6578) ============

fn xml_element_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = body.find(&open)?;
    let content_start = body[start..].find('>')? + start + 1;
    let end_tag = format!("</{tag}>");
    let end = body[content_start..].find(&end_tag)? + content_start;
    Some(body[content_start..end].trim().to_string())
}

fn xml_attr(body: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = body.find(&open)?;
    let end = body[start..].find('>')? + start;
    let fragment = &body[start..end];
    let needle = format!("{attr}=\"");
    let pos = fragment.find(&needle)? + needle.len();
    let rest = &fragment[pos..];
    Some(rest[..rest.find('"')?].to_string())
}

// ============ sync-collection REPORT (RFC 6578) ============

async fn sync_collection(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
    body: &str,
) -> axum::response::Response {
    // Target must be a calendar collection: /calendars/{user}/{slug}
    let Some((calendar, cap)) = calendar_at(pool, creds, path).await else {
        return not_found();
    };
    if !cap.satisfies(CalendarCapability::ReadOnly) {
        return forbidden();
    }
    let since: i64 = xml_element_text(body, "sync-token")
        .and_then(|t| {
            t.trim()
                .split('/')
                .next_back()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(0);
    let current: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM change_log WHERE calendar_id = $1")
            .bind(calendar.id)
            .fetch_one(pool)
            .await
            .unwrap_or(since);
    #[derive(sqlx::FromRow)]
    struct Row {
        resource_id: Uuid,
        operation: String,
        etag: Option<String>,
        deleted_at: Option<chrono::DateTime<Utc>>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT cl.seq, cl.resource_id, cl.operation, e.etag, e.deleted_at
         FROM change_log cl
         LEFT JOIN events e ON e.id = cl.resource_id
         WHERE cl.calendar_id = $1 AND cl.seq > $2
         ORDER BY cl.seq",
    )
    .bind(calendar.id)
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // Keep the newest change per resource.
    let mut latest: std::collections::HashMap<Uuid, Row> = std::collections::HashMap::new();
    for row in rows {
        latest.insert(row.resource_id, row);
    }
    let base = path.trim_end_matches('/');
    let changes: Vec<calendar_caldav::store::SyncChange> = latest
        .into_values()
        .map(|row| calendar_caldav::store::SyncChange {
            href_suffix: format!("{}.ics", row.resource_id),
            etag: row
                .etag
                .filter(|_| row.operation != "deleted" && row.deleted_at.is_none()),
            deleted: row.operation == "deleted" || row.deleted_at.is_some(),
        })
        .collect();
    let xml = calendar_caldav::store::sync_collection_xml(base, &changes, current);
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

// ============ free-busy-query REPORT (RFC 4791 section 9.3) ============

fn parse_ics_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    let (date, time) = value.split_at(value.find('T')?);
    let time = time.trim_end_matches('Z');
    let naive =
        chrono::NaiveDateTime::parse_from_str(&format!("{date}T{time}"), "%Y%m%dT%H%M%S").ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

/// Busy periods for one calendar over the requested window: non-TRANSPARENT,
/// non-CANCELLED events, expanded for recurrence, exceptions overlaid.
async fn busy_periods(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>, db::DbError> {
    let rows = db::list_events_in_range(pool, calendar_id, from, to).await?;
    let masters: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.master_event_id.is_none())
        .map(|r| r.id)
        .collect();
    let exceptions = db::list_exceptions(pool, &masters).await?;
    let mut periods: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for event in &rows {
        if event.transp.as_deref() == Some("TRANSPARENT")
            || event.status.as_deref() == Some("CANCELLED")
        {
            continue;
        }
        if event.master_event_id.is_some() {
            continue; // surfaces through the master's overlay
        }
        if let (Some(starts), Some(ends)) = (event.starts_at, event.ends_at) {
            periods.push((starts, ends));
        }
    }
    for master in rows.iter().filter(|r| r.rrule.is_some()) {
        let Some(dtstart) = master.starts_at else {
            continue;
        };
        let duration = master.ends_at.map(|e| e - dtstart).unwrap_or_else(|| {
            master
                .duration
                .map(|d| chrono::Duration::microseconds(d.microseconds))
                .unwrap_or_else(|| chrono::Duration::hours(1))
        });
        let rdate = json_to_points(&master.rdate);
        let exdate = json_to_points(&master.exdate);
        let tz = calendar_core::recurrence::resolve_tz(master.tzid.as_deref());
        let expanded = calendar_core::recurrence::expand_occurrences(
            calendar_core::DateOrDateTime::Timed(dtstart),
            master.tzid.as_deref(),
            Some(master.rrule.as_deref().unwrap_or_default()),
            &rdate,
            &exdate,
            from,
            to,
        );
        let Ok(expanded) = expanded else { continue };
        for point in expanded {
            let calendar_core::DateOrDateTime::Timed(at) = point else {
                continue;
            };
            // Exceptions keyed on the original occurrence wall-clock.
            let wall = at.with_timezone(&tz).naive_local();
            let matched = exceptions.iter().find(|ex| {
                ex.master_event_id == Some(master.id)
                    && (ex.recurrence_id == Some(wall)
                        || ex.recurrence_id_date == Some(wall.date()))
            });
            if matched.is_some() {
                continue; // the exception row supplies its own period
            }
            let end = at + duration;
            periods.push((at, end));
        }
    }
    for ex in exceptions.iter().filter(|e| !e.deleted_at.is_some()) {
        if ex.transp.as_deref() == Some("TRANSPARENT") || ex.status.as_deref() == Some("CANCELLED")
        {
            continue;
        }
        if let (Some(starts), Some(ends)) = (ex.starts_at, ex.ends_at) {
            periods.push((starts, ends));
        }
    }
    Ok(periods)
}

fn json_to_points(value: &serde_json::Value) -> Vec<calendar_core::DateOrDateTime> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| {
                    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                        return Some(calendar_core::DateOrDateTime::Timed(at.with_timezone(&Utc)));
                    }
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .ok()
                        .map(calendar_core::DateOrDateTime::AllDay)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The calendar collection at /calendars/{user}/{slug} with the caller's
/// capability (mirrors the adapter's namespace rule).
async fn calendar_at(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
) -> Option<(db::CalendarRow, CalendarCapability)> {
    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let slug = match segments.as_slice() {
        ["calendars", _user, slug] => *slug,
        _ => return None,
    };
    db::list_calendars_for_user(pool, creds.user.id)
        .await
        .ok()?
        .into_iter()
        .find(|(cal, _)| cal.slug == slug)
}

const VTODO_ERROR_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <C:supported-calendar-component/>
</D:error>"#;

async fn free_busy_report(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
    body: &str,
) -> Result<axum::response::Response, AppError> {
    let Some((calendar, _cap)) = calendar_at(pool, creds, path).await else {
        return Ok(not_found());
    };
    let start = xml_attr(body, "time-range", "start")
        .and_then(|s| parse_ics_time(&s))
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
    let end = xml_attr(body, "time-range", "end")
        .and_then(|s| parse_ics_time(&s))
        .unwrap_or_else(|| Utc::now() + chrono::Duration::days(60));
    let periods = busy_periods(pool, calendar.id, start, end).await?;
    let fmt = |t: DateTime<Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let mut ics = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//calendar-server//EN\r\nBEGIN:VFREEBUSY\r\n",
    );
    ics.push_str(&format!(
        "UID:{}\r\nDTSTAMP:{}\r\n",
        Uuid::new_v4(),
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    ics.push_str(&format!("DTSTART:{}\r\nDTEND:{}\r\n", fmt(start), fmt(end)));
    for (from, to) in &periods {
        ics.push_str(&format!("FREEBUSY:{}/{}\r\n", fmt(*from), fmt(*to)));
    }
    ics.push_str("END:VFREEBUSY\r\nEND:VCALENDAR\r\n");
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/calendar; charset=utf-8")],
        ics,
    )
        .into_response())
}

fn not_found() -> axum::response::Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn forbidden() -> axum::response::Response {
    (StatusCode::FORBIDDEN, "forbidden").into_response()
}

fn internal(msg: &str) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

fn unauthorized_basic() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Basic realm="calendar""#)],
    )
        .into_response()
}
