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

    // Authenticate once; CalDAV clients expect a Basic challenge. A share
    // token supplied as the Basic username (password ignored) is the other
    // accepted identity: read-only on its shared calendar.
    let creds = match resolve_auth(&pool, request.headers()).await {
        Ok(auth) => DavAuth {
            user: auth.user,
            share_calendar_id: None,
        },
        Err(_) => match share_principal(&pool, request.headers()).await {
            Some(creds) => creds,
            None => return unauthorized_basic(),
        },
    };

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 256 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };

    // A PUT may only carry components its collection allows (RFC 4791
    // supported-calendar-component precondition).
    if method == axum::http::Method::PUT
        && let Some((calendar, _)) = calendar_at(&pool, &creds, collection_of(&path)).await
        && body_components(&String::from_utf8_lossy(&bytes))
            .any(|kind| !calendar.components.iter().any(|c| c == kind))
    {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            COMPONENT_ERROR_BODY,
        )
            .into_response();
    }

    // ADR-011: PUT of a VTODO is rejected with a CalDAV error body.
    if method == axum::http::Method::PUT
        && !bytes.is_empty()
        && let Err(calendar_caldav::IcsError::TodoUnsupported) =
            calendar_caldav::parse_ics(String::from_utf8_lossy(&bytes).as_ref())
    {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            COMPONENT_ERROR_BODY,
        )
            .into_response();
    }

    // Custom REPORTs.
    if method == axum::http::Method::from_bytes(b"REPORT").unwrap() {
        let body = String::from_utf8_lossy(&bytes).to_string();
        if body.contains("sync-collection") {
            return sync_collection(&pool, &creds, &path, &body).await;
        }
        if body.contains("free-busy-query") {
            return free_busy_report(&pool, &creds, &path, &body)
                .await
                .unwrap_or_else(IntoResponse::into_response);
        }
    }

    // dav-server ignores the MKCALENDAR body; apply displayname, description
    // and component set once it has created the collection.
    let mkcalendar_body =
        (method.as_str() == "MKCALENDAR").then(|| String::from_utf8_lossy(&bytes).into_owned());
    let request = axum::http::Request::from_parts(parts, Body::from(bytes));
    let response = dav
        .handle_guarded(request, "/calendars/".to_string(), creds.clone())
        .await;
    if let Some(body) = mkcalendar_body
        && response.status() == StatusCode::CREATED
    {
        apply_mkcalendar(&pool, &creds, &path, &body).await;
    }
    let response = convert(response);
    if method.as_str() == "PROPFIND" {
        return rewrite_component_sets(&pool, &creds, response).await;
    }
    response
}

/// Directory part of a resource path: "/calendars/u/slug/x.ics" -> "/calendars/u/slug".
fn collection_of(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir)
}

/// Component types a calendar body declares, in document order.
fn body_components(ics: &str) -> impl Iterator<Item = &str> {
    ics.lines()
        .filter_map(|line| line.trim().strip_prefix("BEGIN:"))
        .map(str::trim)
        .filter(|kind| matches!(*kind, "VEVENT" | "VTODO" | "VJOURNAL"))
}

async fn apply_mkcalendar(pool: &sqlx::PgPool, creds: &DavAuth, path: &str, body: &str) {
    // No (or unparsable) body still gets applied: it clears the default
    // description dav-server writes on every MKCALENDAR.
    let root = crate::xml::parse(body).unwrap_or_else(|| xmltree::Element::new("mkcalendar"));
    let text_of = |name| {
        crate::xml::find(&root, name)
            .map(crate::xml::text)
            .filter(|s| !s.is_empty())
    };
    let mut components: Vec<String> = Vec::new();
    for comp in crate::xml::find(&root, "supported-calendar-component-set")
        .into_iter()
        .flat_map(|set| set.children.iter().filter_map(|n| n.as_element()))
    {
        // ponytail: VFREEBUSY/VTIMEZONE requests are ignored, they are never stored.
        if let Some(name) = comp.attributes.get("name")
            && matches!(name.as_str(), "VEVENT" | "VTODO" | "VJOURNAL")
            && !components.contains(name)
        {
            components.push(name.clone());
        }
    }
    let changes = db::CalendarUpdate {
        name: text_of("displayname"),
        description: Some(text_of("calendar-description").unwrap_or_default()),
        components: (!components.is_empty()).then_some(components),
        ..Default::default()
    };
    let Some((calendar, _)) = calendar_at(pool, creds, path).await else {
        return;
    };
    if let Err(e) = db::update_calendar(pool, calendar.id, &changes).await {
        tracing::warn!(error = %e, "MKCALENDAR properties not applied");
    }
}

/// dav-server hardcodes supported-calendar-component-set for every
/// collection; replace it with the calendar's stored set.
async fn rewrite_component_sets(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    response: axum::response::Response,
) -> axum::response::Response {
    if response.status() != StatusCode::MULTI_STATUS {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
        return internal("PROPFIND response too large");
    };
    let sets: std::collections::HashMap<String, Vec<String>> =
        db::list_calendars_for_user(pool, creds.user.id)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|(cal, _)| (cal.slug, cal.components))
                    .collect()
            })
            .unwrap_or_default();
    let out = match std::str::from_utf8(&bytes) {
        Ok(xml) => rewrite_component_set_xml(xml, &sets).into_bytes().into(),
        Err(_) => bytes,
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, Body::from(out))
}

const SET_OPEN: &str = "<C:supported-calendar-component-set";
const SET_CLOSE: &str = "</C:supported-calendar-component-set>";

/// Per `<D:response>` whose href is `/calendars/{user}/{slug}/`, swaps the
/// component-set element for that calendar's stored set. Component names come
/// from a CHECK-constrained column, so they are safe to interpolate.
fn rewrite_component_set_xml(
    xml: &str,
    sets: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    const OPEN: &str = "<D:response>";
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find(OPEN) {
        let body = start + OPEN.len();
        let end = rest[body..]
            .find("</D:response>")
            .map_or(rest.len(), |i| body + i);
        out.push_str(&rest[..body]);
        out.push_str(&rewrite_response(&rest[body..end], sets));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn rewrite_response(chunk: &str, sets: &std::collections::HashMap<String, Vec<String>>) -> String {
    let href = chunk
        .split("<D:href>")
        .nth(1)
        .and_then(|h| h.split("</D:href>").next())
        .unwrap_or_default();
    let mut segments = href.trim_matches('/').split('/');
    let comps = match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some("calendars"), Some(_), Some(slug), None) => sets.get(slug),
        _ => None,
    };
    let (Some(comps), Some(start)) = (comps, chunk.find(SET_OPEN)) else {
        return chunk.to_string();
    };
    let Some(close) = chunk[start..].find(SET_CLOSE) else {
        return chunk.to_string();
    };
    let inner: String = comps
        .iter()
        .map(|c| format!(r#"<C:comp name="{c}"/>"#))
        .collect();
    format!(
        "{}{SET_OPEN}>{inner}{SET_CLOSE}{}",
        &chunk[..start],
        &chunk[start + close + SET_CLOSE.len()..]
    )
}

pub(crate) async fn entry_carddav(
    State(AppState {
        pool, dav_carddav, ..
    }): State<AppState>,
    request: Request,
) -> axum::response::Response {
    let Some(dav) = dav_carddav else {
        return internal("CardDAV is not configured");
    };

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    if method == axum::http::Method::OPTIONS {
        let mut response = (StatusCode::OK, "").into_response();
        response
            .headers_mut()
            .insert("DAV", "1, 2, 3, addressbook".parse().unwrap());
        response.headers_mut().insert(
            "Allow",
            "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCOL"
                .parse()
                .unwrap(),
        );
        return response;
    }

    let auth = match resolve_auth(&pool, request.headers()).await {
        Ok(auth) => auth,
        Err(_) => return unauthorized_basic(),
    };

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };

    if method == axum::http::Method::from_bytes(b"REPORT").unwrap() {
        let body = String::from_utf8_lossy(&bytes).to_string();
        if body.contains("sync-collection") {
            return sync_collection_addressbook(
                &pool,
                &calendar_carddav::DavAuth {
                    user: auth.user.clone(),
                },
                &path,
                &body,
            )
            .await;
        }
    }

    let request = axum::http::Request::from_parts(parts, Body::from(bytes));
    let response = dav
        .handle_guarded(
            request,
            "/contacts/".to_string(),
            calendar_carddav::DavAuth { user: auth.user },
        )
        .await;
    convert(response)
}

/// sync-collection REPORT for an address book: dav-server-rs implements it
/// for neither CalDAV nor CardDAV. Unlike the calendar path (which reads an
/// incremental change_log), this returns a full snapshot each time — no
/// per-address-book change journal exists yet. ponytail: fine while books
/// stay small; add change_log rows for contacts if a large book makes full
/// resync too slow.
async fn sync_collection_addressbook(
    pool: &sqlx::PgPool,
    creds: &calendar_carddav::DavAuth,
    path: &str,
    _body: &str,
) -> axum::response::Response {
    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let slug = match segments.as_slice() {
        ["contacts", _user, slug] => *slug,
        _ => return not_found(),
    };
    let tenant_id = match db::find_personal_tenant(pool, creds.user.id).await {
        Ok(id) => id,
        Err(_) => return not_found(),
    };
    let (changes, sync_token): (Vec<calendar_caldav::store::SyncChange>, i64) =
        if slug == db::contacts::DIRECTORY_SLUG {
            let people = db::contacts::directory_entries(pool, tenant_id)
                .await
                .unwrap_or_default();
            let token = db::contacts::directory_ctag(pool, tenant_id)
                .await
                .ok()
                .and_then(|c| c.split('-').next_back().and_then(|n| n.parse().ok()))
                .unwrap_or(0);
            let changes = people
                .into_iter()
                .map(|p| calendar_caldav::store::SyncChange {
                    href_suffix: format!("{}.vcf", p.user_id),
                    etag: Some(format!("\"dir-{}\"", p.updated_at.timestamp())),
                    deleted: false,
                })
                .collect();
            (changes, token)
        } else {
            let Ok(book) = db::contacts::get_address_book_by_slug(pool, creds.user.id, slug).await
            else {
                return not_found();
            };
            let mut changes: Vec<calendar_caldav::store::SyncChange> =
                db::contacts::list_contacts(pool, book.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| calendar_caldav::store::SyncChange {
                        href_suffix: format!("{}.vcf", c.uid),
                        etag: Some(c.etag),
                        deleted: false,
                    })
                    .collect();
            for id in db::contacts::list_deleted_contacts(pool, book.id)
                .await
                .unwrap_or_default()
            {
                changes.push(calendar_caldav::store::SyncChange {
                    href_suffix: format!("{id}.vcf"),
                    etag: None,
                    deleted: true,
                });
            }
            (changes, book.ctag)
        };
    let base = path.trim_end_matches('/');
    let xml = calendar_caldav::store::sync_collection_xml(base, &changes, sync_token);
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
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
    let root = crate::xml::parse(body)?;
    crate::xml::find(&root, tag).map(crate::xml::text)
}

fn xml_attr(body: &str, tag: &str, attr: &str) -> Option<String> {
    let root = crate::xml::parse(body)?;
    crate::xml::find(&root, tag)?.attributes.get(attr).cloned()
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
        class: Option<String>,
        href: String,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT cl.seq, cl.resource_id, cl.operation, e.etag, e.deleted_at, e.class,
                COALESCE(e.href, cl.resource_id::text || '.ics') AS href
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
    // Keep the newest change per resource. A share principal never sees
    // non-PUBLIC events, including their tombstones.
    let share = creds.share_calendar_id.is_some();
    let mut latest: std::collections::HashMap<Uuid, Row> = std::collections::HashMap::new();
    for row in rows {
        if share && row.class.as_deref() != Some("PUBLIC") {
            continue;
        }
        latest.insert(row.resource_id, row);
    }
    let base = path.trim_end_matches('/');
    let changes: Vec<calendar_caldav::store::SyncChange> = latest
        .into_values()
        .map(|row| calendar_caldav::store::SyncChange {
            href_suffix: row.href,
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

/// Busy instants of an all-day row: RFC 5545 DTEND is the inclusive last day,
/// so the busy span is [start midnight, day-after-last-day midnight) at UTC.
/// All-day rows carry no timezone; UTC matches the SQL range filter.
fn all_day_span(
    start: chrono::NaiveDate,
    end: Option<chrono::NaiveDate>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|n| Utc.from_utc_datetime(&n));
    Some((
        midnight(start)?,
        midnight(end.unwrap_or(start).succ_opt()?)?,
    ))
}

/// One non-recurring row's busy interval: the timed instants, or an all-day
/// row's whole date range (there is no `ends_at` to read).
fn event_period(row: &db::EventRow) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if let (Some(starts), Some(ends)) = (row.starts_at, row.ends_at) {
        return Some((starts, ends));
    }
    all_day_span(row.start_date?, row.end_date)
}

/// Trim a period to the query window (all-day spans can extend beyond it).
fn clamp_period(
    period: Option<(DateTime<Utc>, DateTime<Utc>)>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (start, end) = period?;
    let start = start.max(from);
    let end = end.min(to);
    (start < end).then_some((start, end))
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
    Ok(busy_periods_from(&rows, &exceptions, from, to))
}

fn busy_periods_from(
    rows: &[db::EventRow],
    exceptions: &[db::EventRow],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut periods: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for event in rows {
        if event.transp.as_deref() == Some("TRANSPARENT")
            || event.status.as_deref() == Some("CANCELLED")
        {
            continue;
        }
        if event.master_event_id.is_some() || event.rrule.is_some() {
            continue; // recurring masters surface through expansion below
        }
        if let Some(period) = clamp_period(event_period(event), from, to) {
            periods.push(period);
        }
    }
    for master in rows.iter().filter(|r| r.rrule.is_some()) {
        let rdate = json_to_points(&master.rdate);
        let exdate = json_to_points(&master.exdate);
        let tz = calendar_core::recurrence::resolve_tz(master.tzid.as_deref());
        // Exceptions keyed on the original occurrence wall-clock.
        let exception_at = |wall: chrono::NaiveDateTime| {
            exceptions.iter().find(|ex| {
                ex.master_event_id == Some(master.id)
                    && (ex.recurrence_id == Some(wall)
                        || ex.recurrence_id_date == Some(wall.date()))
            })
        };
        let expanded = if let Some(dtstart) = master.starts_at {
            calendar_core::recurrence::expand_occurrences(
                calendar_core::DateOrDateTime::Timed(dtstart),
                master.tzid.as_deref(),
                Some(master.rrule.as_deref().unwrap_or_default()),
                &rdate,
                &exdate,
                from,
                to,
            )
        } else if let Some(start) = master.start_date {
            calendar_core::recurrence::expand_occurrences(
                calendar_core::DateOrDateTime::AllDay(start),
                master.tzid.as_deref(),
                Some(master.rrule.as_deref().unwrap_or_default()),
                &rdate,
                &exdate,
                from,
                to,
            )
        } else {
            continue;
        };
        let Ok(expanded) = expanded else { continue };
        for point in expanded {
            // Timed occurrences keep their duration; all-day occurrences
            // occupy the master's whole inclusive date range each time.
            let period = match point {
                calendar_core::DateOrDateTime::Timed(at) => {
                    if exception_at(at.with_timezone(&tz).naive_local()).is_some() {
                        continue; // the exception row supplies its own period
                    }
                    let Some(dtstart) = master.starts_at else {
                        continue;
                    };
                    let duration = master.ends_at.map(|e| e - dtstart).unwrap_or_else(|| {
                        master
                            .duration
                            .map(|d| chrono::Duration::microseconds(d.microseconds))
                            .unwrap_or_else(|| chrono::Duration::hours(1))
                    });
                    Some((at, at + duration))
                }
                calendar_core::DateOrDateTime::AllDay(date) => {
                    let Some(wall) = date.and_hms_opt(0, 0, 0) else {
                        continue;
                    };
                    if exception_at(wall).is_some() {
                        continue; // the exception row supplies its own period
                    }
                    let Some(start) = master.start_date else {
                        continue;
                    };
                    let span_days = (master.end_date.unwrap_or(start) - start).num_days() + 1;
                    all_day_span(date, Some(date + chrono::Duration::days(span_days - 1)))
                }
            };
            if let Some(period) = clamp_period(period, from, to) {
                periods.push(period);
            }
        }
    }
    for ex in exceptions.iter().filter(|e| e.deleted_at.is_none()) {
        if ex.transp.as_deref() == Some("TRANSPARENT") || ex.status.as_deref() == Some("CANCELLED")
        {
            continue;
        }
        if let Some(period) = clamp_period(event_period(ex), from, to) {
            periods.push(period);
        }
    }
    periods.sort();
    periods
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

/// A share token supplied as the Basic username (password ignored) grants
/// read-only DAV access to its shared calendar. Revocation, expiry, and the
/// `allows_caldav` flag are enforced by the live-share lookup on every
/// request; calendar-level shares only (single-event shares stay web-only).
async fn share_principal(pool: &sqlx::PgPool, headers: &axum::http::HeaderMap) -> Option<DavAuth> {
    use base64::Engine;
    let basic = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let encoded = basic.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let username = std::str::from_utf8(&decoded).ok()?.split(':').next()?;
    let token_hash = calendar_auth::sha256(username.as_bytes());
    let share = db::sharing::find_live_share(pool, &token_hash).await.ok()?;
    if !share.allows_caldav || share.event_id.is_some() {
        return None;
    }
    let calendar = db::get_calendar(pool, share.calendar_id).await.ok()?;
    let owner_id = match share.created_by {
        Some(id) => id,
        None => sqlx::query_scalar(
            "SELECT principal_user_id FROM calendar_acl
             WHERE calendar_id = $1 AND capability = 'owner' LIMIT 1",
        )
        .bind(calendar.id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?,
    };
    let owner = db::find_user_by_id(pool, owner_id).await.ok()?;
    Some(DavAuth {
        user: owner,
        share_calendar_id: Some(calendar.id),
    })
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
    if let Some(calendar_id) = creds.share_calendar_id {
        let cal = db::get_calendar(pool, calendar_id).await.ok()?;
        if cal.slug != slug {
            return None;
        }
        return Some((cal, CalendarCapability::ReadOnly));
    }
    db::list_calendars_for_user(pool, creds.user.id)
        .await
        .ok()?
        .into_iter()
        .find(|(cal, _)| cal.slug == slug)
}

const COMPONENT_ERROR_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <C:supported-calendar-component/>
</D:error>"#;

async fn free_busy_report(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
    body: &str,
) -> Result<axum::response::Response, AppError> {
    let Some((calendar, cap)) = calendar_at(pool, creds, path).await else {
        return Ok(not_found());
    };
    // Free-busy is calendar content: require read access, like the other
    // REPORT paths.
    if !cap.satisfies(CalendarCapability::ReadOnly) {
        return Ok(not_found());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Shape emitted by dav-server 0.11: hardcoded four-component set on every collection.
    const PROPFIND: &str = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">"#,
        r#"<D:response><D:href>/calendars/alice/tasks/</D:href><D:propstat><D:prop>"#,
        r#"<C:supported-calendar-component-set><C:comp name="VEVENT"/><C:comp name="VTODO"/><C:comp name="VJOURNAL"/><C:comp name="VFREEBUSY"/></C:supported-calendar-component-set>"#,
        r#"<D:displayname>My Tasks</D:displayname></D:prop></D:propstat></D:response>"#,
        r#"<D:response><D:href>/calendars/alice/tasks/x.ics</D:href><D:propstat><D:prop>"#,
        r#"<C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set>"#,
        r#"</D:prop></D:propstat></D:response></D:multistatus>"#,
    );

    #[test]
    fn propfind_component_set_is_replaced_per_collection_only() {
        let sets = HashMap::from([("tasks".to_string(), vec!["VTODO".to_string()])]);
        let out = rewrite_component_set_xml(PROPFIND, &sets);
        assert!(out.contains(
            r#"<C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set><D:displayname>My Tasks"#
        ));
        assert!(!out.contains("VFREEBUSY"));
        // A resource href (4 segments) is left alone.
        assert!(out.contains(r#"/x.ics</D:href><D:propstat><D:prop><C:supported-calendar-component-set><C:comp name="VEVENT"/>"#));
        // Unknown collections are untouched.
        assert_eq!(
            rewrite_component_set_xml(PROPFIND, &HashMap::new()),
            PROPFIND
        );
    }

    #[test]
    fn put_gate_helpers() {
        assert_eq!(
            collection_of("/calendars/alice/work/a.ics"),
            "/calendars/alice/work"
        );
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nBEGIN:VALARM\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert_eq!(body_components(ics).collect::<Vec<_>>(), ["VTODO"]);
    }

    fn base_row(id: Uuid) -> db::EventRow {
        db::EventRow {
            id,
            calendar_id: Uuid::new_v4(),
            uid: id.to_string(),
            href: None,
            master_event_id: None,
            recurrence_id: None,
            recurrence_id_date: None,
            is_exception: false,
            starts_at: None,
            ends_at: None,
            start_date: None,
            end_date: None,
            duration: None,
            tzid: None,
            all_day: false,
            floating: false,
            rrule: None,
            rdate: serde_json::json!([]),
            exdate: serde_json::json!([]),
            summary: String::new(),
            description_html: None,
            description_text: None,
            url: None,
            status: None,
            priority: None,
            class: None,
            transp: None,
            categories: Vec::new(),
            location_id: None,
            organizer_user_id: None,
            organizer_email: String::new(),
            organizer_name: None,
            sequence: 0,
            etag: String::new(),
            created_by: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn free_busy_includes_all_day_events() {
        let from = at(2026, 9, 15, 10);
        let to = at(2026, 9, 15, 12);
        let mut single = base_row(Uuid::new_v4());
        single.start_date = Some(date(2026, 9, 15));
        let mut multi = base_row(Uuid::new_v4());
        multi.start_date = Some(date(2026, 9, 13));
        multi.end_date = Some(date(2026, 9, 17));
        let mut private = base_row(Uuid::new_v4());
        private.start_date = Some(date(2026, 9, 15));
        private.class = Some("CONFIDENTIAL".into());
        let mut transparent = base_row(Uuid::new_v4());
        transparent.start_date = Some(date(2026, 9, 15));
        transparent.transp = Some("TRANSPARENT".into());
        let mut outside = base_row(Uuid::new_v4());
        outside.start_date = Some(date(2026, 9, 16));
        let mut timed = base_row(Uuid::new_v4());
        timed.starts_at = Some(at(2026, 9, 15, 10));
        timed.ends_at = Some(at(2026, 9, 15, 11));
        // All-day rows become busy (multi-day clipped to the window),
        // PRIVATE/CONFIDENTIAL stay busy, TRANSPARENT and non-overlapping do not.
        assert_eq!(
            busy_periods_from(
                &[single, multi, private, transparent, outside, timed],
                &[],
                from,
                to
            ),
            vec![
                (from, at(2026, 9, 15, 11)),
                (from, to),
                (from, to),
                (from, to),
            ]
        );
    }

    #[test]
    fn free_busy_expands_all_day_recurrence_with_exceptions() {
        let from = at(2026, 9, 14, 0);
        let to = at(2026, 9, 21, 0);
        let mut master = base_row(Uuid::new_v4());
        master.start_date = Some(date(2026, 9, 14));
        master.end_date = Some(date(2026, 9, 15)); // two days per occurrence
        master.rrule = Some("FREQ=DAILY;INTERVAL=2".into());
        // Overrides the 2026-09-16 occurrence, moving it to the 17th.
        let mut ex = base_row(Uuid::new_v4());
        ex.master_event_id = Some(master.id);
        ex.recurrence_id_date = Some(date(2026, 9, 16));
        ex.start_date = Some(date(2026, 9, 17));
        let periods = busy_periods_from(&[master], &[ex], from, to);
        // Every other day starting on the 14th, each spanning two whole days;
        // the 16th occurrence is replaced by the moved exception, and the
        // 20th occurrence is clipped at the window end.
        assert_eq!(
            periods,
            vec![
                (at(2026, 9, 14, 0), at(2026, 9, 16, 0)),
                (at(2026, 9, 17, 0), at(2026, 9, 18, 0)),
                (at(2026, 9, 18, 0), at(2026, 9, 20, 0)),
                (at(2026, 9, 20, 0), at(2026, 9, 21, 0)),
            ]
        );
    }

    #[test]
    fn free_busy_timed_recurrence_keeps_duration() {
        let from = at(2026, 9, 14, 0);
        let to = at(2026, 9, 15, 0);
        let mut master = base_row(Uuid::new_v4());
        master.starts_at = Some(at(2026, 9, 14, 9));
        master.ends_at = Some(at(2026, 9, 14, 10));
        master.rrule = Some("FREQ=DAILY".into());
        assert_eq!(
            busy_periods_from(&[master], &[], from, to),
            vec![(at(2026, 9, 14, 9), at(2026, 9, 14, 10))]
        );
    }
}
