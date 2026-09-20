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

    // Custom REPORTs, routed on the request body's root element: a
    // calendar-query whose text-match mentions "sync-collection" must not be
    // misrouted. Unparsable bodies fall through to dav-server (400).
    if method == axum::http::Method::from_bytes(b"REPORT").unwrap() {
        let body = String::from_utf8_lossy(&bytes).to_string();
        match report_kind(&body) {
            ReportKind::SyncCollection => {
                return sync_collection(&pool, &creds, &path, &body).await;
            }
            ReportKind::FreeBusyQuery => {
                return free_busy_report(&pool, &creds, &path, &body)
                    .await
                    .unwrap_or_else(IntoResponse::into_response);
            }
            ReportKind::CalendarQuery => {
                return calendar_query_report(&pool, &creds, &path, &body)
                    .await
                    .unwrap_or_else(IntoResponse::into_response);
            }
            ReportKind::Other => {} // calendar-multiget etc. stay with dav-server
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

/// The REPORTs implemented in this file, advertised via supported-report-set:
/// dav-server emits none of its own, so clients cannot discover
/// sync-collection (RFC 6578 section 3.2) without this injection.
const SYNC_REPORT_SET: &str = "<D:supported-report-set><D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report></D:supported-report-set>";
const FULL_REPORT_SET: &str = concat!(
    "<D:supported-report-set>",
    "<D:supported-report><D:report><C:calendar-query/></D:report></D:supported-report>",
    "<D:supported-report><D:report><C:free-busy-query/></D:report></D:supported-report>",
    "<D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>",
    "</D:supported-report-set>",
);

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
    let seg = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    );
    // Calendar collections advertise everything; the home set only what it
    // can answer (an aggregate sync-collection).
    let reports = match seg {
        (Some("calendars"), Some(_), None, None) => Some(SYNC_REPORT_SET),
        (Some("calendars"), Some(_), Some(_), None) => Some(FULL_REPORT_SET),
        _ => None,
    };
    let chunk: std::borrow::Cow<str> = match reports {
        Some(set) => chunk.find("</D:prop>").map_or(chunk.into(), |i| {
            format!("{}{set}{}", &chunk[..i], &chunk[i..]).into()
        }),
        None => chunk.into(),
    };
    let comps = match seg {
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
        if report_kind(&body) == ReportKind::SyncCollection {
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
                    calendar_data: None,
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
                        calendar_data: None,
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
                    calendar_data: None,
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

// ============ REPORT dispatch (root element, RFC 4791/6578) ============

#[derive(Debug, PartialEq, Eq)]
enum ReportKind {
    SyncCollection,
    FreeBusyQuery,
    CalendarQuery,
    Other,
}

/// The custom REPORT a request body asks for, decided by its root element's
/// local name (any prefix/namespace declaration style). Bodies that do not
/// parse are `Other`: dav-server answers them (400 for garbage, or its own
/// implementation for reports this server does not intercept).
fn report_kind(body: &str) -> ReportKind {
    match crate::xml::parse(body).map(|root| root.name) {
        Some(name) => match name.as_str() {
            "sync-collection" => ReportKind::SyncCollection,
            "free-busy-query" => ReportKind::FreeBusyQuery,
            "calendar-query" => ReportKind::CalendarQuery,
            _ => ReportKind::Other,
        },
        None => ReportKind::Other,
    }
}

// ============ sync-collection REPORT (RFC 6578) ============

async fn sync_collection(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
    body: &str,
) -> axum::response::Response {
    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    // Target is one calendar collection (/calendars/{user}/{slug}) or the
    // home set (/calendars/{user}/), which aggregates every calendar the
    // caller can read behind a single global change_log token.
    let (base, calendar_ids, aggregate) = match segments.as_slice() {
        ["calendars", _user, _slug] => {
            let Some((calendar, cap)) = calendar_at(pool, creds, path).await else {
                return not_found();
            };
            if !cap.satisfies(CalendarCapability::ReadOnly) {
                return forbidden();
            }
            (
                path.trim_end_matches('/').to_string(),
                vec![calendar.id],
                false,
            )
        }
        ["calendars", _user] => {
            let ids: Vec<Uuid> = if let Some(share_id) = creds.share_calendar_id {
                vec![share_id]
            } else {
                db::list_calendars_for_user(pool, creds.user.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(_, cap)| cap.satisfies(CalendarCapability::ReadOnly))
                    .map(|(cal, _)| cal.id)
                    .collect()
            };
            (path.trim_end_matches('/').to_string(), ids, true)
        }
        _ => return not_found(),
    };
    if calendar_ids.is_empty() {
        return sync_xml_response(&base, &[], 0);
    }
    let root = crate::xml::parse(body);
    let since: i64 = root
        .as_ref()
        .and_then(|root| crate::xml::find(root, "sync-token"))
        .map(crate::xml::text)
        .and_then(|t| {
            t.trim()
                .split('/')
                .next_back()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(0);
    let limit: Option<usize> = root
        .as_ref()
        .and_then(|root| crate::xml::find(root, "limit"))
        .and_then(|l| crate::xml::find(l, "nresults"))
        .map(crate::xml::text)
        .and_then(|t| t.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n as usize);
    let want_data = root
        .as_ref()
        .and_then(|root| crate::xml::find(root, "prop"))
        .map(|p| {
            p.children
                .iter()
                .filter_map(|n| n.as_element())
                .any(|c| c.name == "calendar-data")
        })
        .unwrap_or(false);

    // Token and rows must come from one consistent snapshot: a change
    // committed between the two reads would be reported with a token that
    // claims to cover it, and the client would never see it.
    let Ok(mut tx) = pool.begin().await else {
        return internal("sync unavailable");
    };
    if sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return internal("sync unavailable");
    }
    let fetched = async {
        let current: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(seq), 0) FROM change_log WHERE calendar_id = ANY($1)",
        )
        .bind(&calendar_ids)
        .fetch_one(&mut *tx)
        .await?;
        let rows = sqlx::query_as::<_, ChangeRow>(
            "SELECT cl.seq, cl.resource_id, cl.operation, e.etag, e.deleted_at, e.class,
                    COALESCE(e.href, cl.resource_id::text || '.ics') AS href, c.slug AS slug
             FROM change_log cl
             JOIN calendars c ON c.id = cl.calendar_id
             LEFT JOIN events e ON e.id = cl.resource_id
             WHERE cl.calendar_id = ANY($1) AND cl.seq > $2
             ORDER BY cl.seq",
        )
        .bind(&calendar_ids)
        .bind(since)
        .fetch_all(&mut *tx)
        .await?;
        Ok::<_, sqlx::Error>((current, rows))
    }
    .await;
    let (current, rows) = match fetched {
        Ok((current, rows)) => (current, rows),
        Err(_) => return internal("sync unavailable"),
    };
    // A token beyond the current sequence (purged or fabricated) cannot name
    // a consistent past state: the client must resync from scratch.
    if since > current {
        return (
            StatusCode::CONFLICT,
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            calendar_caldav::store::invalid_sync_token_error(),
        )
            .into_response();
    }
    let (changes, token) = truncate_changes(
        latest_changes(rows, creds.share_calendar_id.is_some()),
        limit,
        current,
    );
    let share = creds.share_calendar_id.is_some();
    let mut data: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    if want_data {
        let ids: Vec<Uuid> = changes
            .iter()
            .filter(|r| !r.is_delete())
            .map(|r| r.resource_id)
            .collect();
        let masters = sqlx::query_as::<_, db::EventRow>(
            "SELECT * FROM events WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for master in masters {
            if share && master.class.as_deref() != Some("PUBLIC") {
                continue;
            }
            // Home-set reports mix calendars; each resource renders with its
            // own calendar's stored VTIMEZONEs.
            let vtimezones = db::timezones::list_for_calendar(pool, master.calendar_id)
                .await
                .unwrap_or_default();
            let ics = resource_ics(pool, &master, share, &vtimezones).await;
            data.insert(master.id, ics);
        }
    }
    let out: Vec<calendar_caldav::store::SyncChange> = changes
        .into_iter()
        .map(|row| {
            let deleted = row.is_delete();
            calendar_caldav::store::SyncChange {
                href_suffix: change_suffix(aggregate, &row.slug, &row.href),
                etag: row.etag.filter(|_| !deleted),
                deleted,
                calendar_data: data.get(&row.resource_id).cloned(),
            }
        })
        .collect();
    sync_xml_response(&base, &out, token)
}

fn sync_xml_response(
    base: &str,
    changes: &[calendar_caldav::store::SyncChange],
    token: i64,
) -> axum::response::Response {
    let xml = calendar_caldav::store::sync_collection_xml(base, changes, token);
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ChangeRow {
    seq: i64,
    resource_id: Uuid,
    operation: String,
    etag: Option<String>,
    deleted_at: Option<chrono::DateTime<Utc>>,
    class: Option<String>,
    href: String,
    slug: String,
}

impl ChangeRow {
    fn is_delete(&self) -> bool {
        self.operation == "deleted" || self.deleted_at.is_some()
    }
}

/// One sync href: a single calendar reports resources relative to itself; a
/// home-set report needs the calendar slug in the path.
fn change_suffix(aggregate: bool, slug: &str, href: &str) -> String {
    if aggregate {
        format!("{slug}/{href}")
    } else {
        href.to_string()
    }
}

/// Keep the newest change per resource, ordered by seq. A share principal
/// never sees non-PUBLIC events, including their tombstones.
fn latest_changes(rows: Vec<ChangeRow>, share: bool) -> Vec<ChangeRow> {
    let mut latest: std::collections::HashMap<Uuid, ChangeRow> = std::collections::HashMap::new();
    for row in rows {
        if share && row.class.as_deref() != Some("PUBLIC") {
            continue;
        }
        latest.insert(row.resource_id, row);
    }
    let mut changes: Vec<ChangeRow> = latest.into_values().collect();
    changes.sort_by_key(|r| r.seq);
    changes
}

/// Truncate to `limit` changes, returning (changes, sync-token). The token
/// is the sequence of the last returned change, which the client uses as its
/// next sync-token for the rest (RFC 6578 partial sync). Truncation never
/// hides a deletion: deletion entries beyond the limit are still included,
/// and get re-reported after the client catches up.
fn truncate_changes(
    changes: Vec<ChangeRow>,
    limit: Option<usize>,
    current: i64,
) -> (Vec<ChangeRow>, i64) {
    match limit {
        Some(n) if changes.len() > n => {
            let mut selected: Vec<ChangeRow> = changes[..n].to_vec();
            let token = selected.last().map(|r| r.seq).unwrap_or(current);
            selected.extend(changes[n..].iter().filter(|r| r.is_delete()).cloned());
            (selected, token)
        }
        _ => (changes, current),
    }
}

/// One resource's VCALENDAR: the master plus its overrides, with attendees,
/// alarms and location — the adapter's series_ics, as inline queries. A share
/// principal gets no alarm data (events outside their visibility carry none).
async fn resource_ics(
    pool: &sqlx::PgPool,
    master: &db::EventRow,
    share: bool,
    vtimezones: &[db::timezones::StoredTimezone],
) -> String {
    let mut events = vec![master.clone()];
    events.extend(
        db::list_exceptions(pool, &[master.id])
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|ex| !share || ex.class.as_deref() == Some("PUBLIC")),
    );
    let mut rows = Vec::with_capacity(events.len());
    for event in events {
        rows.push(calendar_caldav::ExportRow {
            attendees: db::list_attendees(pool, event.id).await.unwrap_or_default(),
            alarms: if share {
                Vec::new()
            } else {
                db::alarms::list_alarms(pool, event.id)
                    .await
                    .unwrap_or_default()
            },
            location: db::location_for_event(pool, &event).await,
            vtimezones: vtimezones.to_vec(),
            event,
        });
    }
    calendar_caldav::events_to_ics(&rows)
}

// ============ free-busy-query REPORT (RFC 4791 section 9.3) ============

fn parse_ics_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    // VALUE=DATE (e.g. a free-busy or time-range boundary): midnight UTC.
    if !value.contains('T') {
        let day = chrono::NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        return day.and_hms_opt(0, 0, 0).map(|n| Utc.from_utc_datetime(&n));
    }
    let naive =
        chrono::NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
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
    let resolver = calendar_zone_resolver(pool, calendar_id).await;
    Ok(busy_periods_from(&rows, &exceptions, &resolver, from, to))
}

/// The calendar's stored client-supplied VTIMEZONEs (ADR-012) as a resolver;
/// an unreadable zone table degrades to tzdb-only expansion.
async fn calendar_zone_resolver(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
) -> calendar_core::recurrence::TzResolver {
    db::timezones::load_for_calendar(pool, calendar_id)
        .await
        .unwrap_or_default()
}

/// Expanded occurrences of one recurring master within [from, to), as
/// (period, override) pairs: an exception row that replaced that occurrence
/// yields (None, Some(exception)) — the caller takes the period and
/// properties from the override row instead.
type Occurrence<'a> = (
    Option<(DateTime<Utc>, DateTime<Utc>)>,
    Option<&'a db::EventRow>,
);

fn recurring_candidates<'a>(
    master: &'a db::EventRow,
    exceptions: &'a [db::EventRow],
    resolver: &calendar_core::recurrence::TzResolver,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Occurrence<'a>> {
    let rdate = json_to_points(&master.rdate);
    let exdate = json_to_points(&master.exdate);
    // An unresolvable tzid expands to nothing rather than silently recurring
    // as UTC (ADR-012).
    let tz = match calendar_core::recurrence::resolve_tz(master.tzid.as_deref(), Some(resolver)) {
        Ok(tz) => tz,
        Err(_) => return Vec::new(),
    };
    // Exceptions keyed on the original occurrence wall-clock.
    let exception_at = |wall: chrono::NaiveDateTime| {
        exceptions.iter().find(|ex| {
            ex.master_event_id == Some(master.id)
                && (ex.recurrence_id == Some(wall) || ex.recurrence_id_date == Some(wall.date()))
        })
    };
    let expanded = match (master.starts_at, master.start_date) {
        (Some(dtstart), _) => calendar_core::recurrence::expand_occurrences(
            calendar_core::DateOrDateTime::Timed(dtstart),
            master.tzid.as_deref(),
            Some(resolver),
            Some(master.rrule.as_deref().unwrap_or_default()),
            &rdate,
            &exdate,
            from,
            to,
        ),
        (None, Some(start)) => calendar_core::recurrence::expand_occurrences(
            calendar_core::DateOrDateTime::AllDay(start),
            master.tzid.as_deref(),
            Some(resolver),
            Some(master.rrule.as_deref().unwrap_or_default()),
            &rdate,
            &exdate,
            from,
            to,
        ),
        _ => return Vec::new(),
    };
    let Ok(expanded) = expanded else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for point in expanded {
        // Timed occurrences keep their duration; all-day occurrences occupy
        // the master's whole inclusive date range each time.
        match point {
            calendar_core::DateOrDateTime::Timed(at) => {
                if let Some(ex) = exception_at(tz.to_local(at)) {
                    candidates.push((None, Some(ex)));
                    continue;
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
                candidates.push((Some((at, at + duration)), None));
            }
            calendar_core::DateOrDateTime::AllDay(date) => {
                let Some(wall) = date.and_hms_opt(0, 0, 0) else {
                    continue;
                };
                if let Some(ex) = exception_at(wall) {
                    candidates.push((None, Some(ex)));
                    continue;
                }
                let Some(start) = master.start_date else {
                    continue;
                };
                let span_days = (master.end_date.unwrap_or(start) - start).num_days() + 1;
                candidates.push((
                    all_day_span(date, Some(date + chrono::Duration::days(span_days - 1))),
                    None,
                ));
            }
        }
    }
    candidates
}

fn busy_periods_from(
    rows: &[db::EventRow],
    exceptions: &[db::EventRow],
    resolver: &calendar_core::recurrence::TzResolver,
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
        for (period, _override) in recurring_candidates(master, exceptions, resolver, from, to) {
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

// ============ calendar-query REPORT (RFC 4791 section 9.5) ============

#[derive(Debug, Clone, Default)]
struct TextMatch {
    value: String,
    negate: bool,
}

#[derive(Debug, Clone, Default)]
struct PropFilter {
    name: String,
    text: Option<TextMatch>,
    /// Some(true) = is-defined, Some(false) = is-not-defined, None = bare.
    defined: Option<bool>,
}

/// The core RFC 4791 filter set this server supports, parsed from the
/// request body. Non-goals (per docs/DEFERRED_REQUIREMENTS.md): nested
/// VALARM comp-filters, limit-recurrence-set/limit-freebusy-set, and
/// CALDAV:timezone request bodies (times are read as UTC, like free-busy).
#[derive(Debug, Clone, Default)]
struct QueryFilter {
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    prop_filters: Vec<PropFilter>,
    /// The comp-filters exclude VEVENT (other component, or VEVENT with
    /// is-not-defined): no stored event can match.
    none: bool,
}

impl QueryFilter {
    fn matches_row(&self, row: &db::EventRow) -> bool {
        self.prop_filters
            .iter()
            .all(|f| prop_filter_matches(row, f))
    }
}

fn parse_calendar_query(root: &xmltree::Element) -> (QueryFilter, Vec<String>) {
    let props: Vec<String> = crate::xml::find(root, "prop")
        .map(|p| {
            p.children
                .iter()
                .filter_map(|n| n.as_element())
                .map(|e| e.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut filter = QueryFilter::default();
    // Standard shape: filter > comp-filter(VCALENDAR) > comp-filter(VEVENT).
    let Some(vcalendar) = crate::xml::find(root, "comp-filter")
        .filter(|c| c.attributes.get("name").map(String::as_str) == Some("VCALENDAR"))
    else {
        return (filter, props); // no comp-filter: no constraints
    };
    let comps: Vec<&xmltree::Element> = vcalendar
        .children
        .iter()
        .filter_map(|n| n.as_element())
        .filter(|c| c.name == "comp-filter")
        .collect();
    let Some(vevent) = comps
        .iter()
        .copied()
        .find(|c| c.attributes.get("name").map(String::as_str) == Some("VEVENT"))
    else {
        filter.none = true; // e.g. a VTODO query: nothing stored matches
        return (filter, props);
    };
    if vevent
        .children
        .iter()
        .filter_map(|n| n.as_element())
        .any(|c| c.name == "is-not-defined")
    {
        filter.none = true;
        return (filter, props);
    }
    filter.time_range = vevent
        .children
        .iter()
        .filter_map(|n| n.as_element())
        .find(|c| c.name == "time-range")
        .and_then(|tr| {
            let start = tr.attributes.get("start").and_then(|s| parse_ics_time(s));
            let end = tr.attributes.get("end").and_then(|s| parse_ics_time(s));
            match (start, end) {
                (Some(start), Some(end)) => Some((start, end)),
                _ => None,
            }
        });
    filter.prop_filters = vevent
        .children
        .iter()
        .filter_map(|n| n.as_element())
        .filter(|c| c.name == "prop-filter")
        .filter_map(|pf| {
            let name = pf.attributes.get("name")?.clone();
            let children: Vec<&xmltree::Element> =
                pf.children.iter().filter_map(|n| n.as_element()).collect();
            let defined = if children.iter().any(|c| c.name == "is-not-defined") {
                Some(false)
            } else if children.iter().any(|c| c.name == "is-defined") {
                Some(true)
            } else {
                None
            };
            let text = children
                .iter()
                .find(|c| c.name == "text-match")
                .map(|tm| TextMatch {
                    value: crate::xml::text(tm),
                    negate: tm.attributes.get("negate-condition").map(String::as_str)
                        == Some("yes"),
                });
            Some(PropFilter {
                name,
                text,
                defined,
            })
        })
        .collect();
    (filter, props)
}

/// The row property a filter name reads; None = the property is not defined
/// on the row (or is one this server keeps elsewhere, e.g. structured
/// LOCATION text).
fn prop_value(row: &db::EventRow, name: &str) -> Option<String> {
    let value = match name {
        "SUMMARY" => Some(row.summary.clone()),
        "DESCRIPTION" => row
            .description_text
            .clone()
            .or_else(|| row.description_html.clone()),
        "STATUS" => row.status.clone(),
        "CLASS" => row.class.clone(),
        "TRANSP" => row.transp.clone(),
        "UID" => Some(row.uid.clone()),
        "URL" => row.url.clone(),
        "PRIORITY" => row.priority.map(|p| p.to_string()),
        "CATEGORIES" => (!row.categories.is_empty()).then(|| row.categories.join(",")),
        _ => None,
    };
    value.filter(|s| !s.is_empty())
}

fn prop_filter_matches(row: &db::EventRow, f: &PropFilter) -> bool {
    if f.defined == Some(false) {
        return prop_value(row, &f.name).is_none();
    }
    let value = prop_value(row, &f.name);
    match &f.text {
        // text-match on an undefined property never matches, negated or not.
        Some(tm) => match value {
            Some(v) => v.to_lowercase().contains(&tm.value.to_lowercase()) != tm.negate,
            None => false,
        },
        None => value.is_some(),
    }
}

/// Resource ids (master or single events) matching the query: a recurring
/// master matches when any of its occurrences or exception overrides inside
/// the window matches; the response carries one resource per master.
fn matching_resources(
    rows: &[db::EventRow],
    exceptions: &[db::EventRow],
    resolver: &calendar_core::recurrence::TzResolver,
    filter: &QueryFilter,
) -> Vec<Uuid> {
    if filter.none {
        return Vec::new();
    }
    let mut matched: Vec<Uuid> = Vec::new();
    for row in rows
        .iter()
        .filter(|r| r.master_event_id.is_none() && r.rrule.is_none())
    {
        let in_window = match filter.time_range {
            None => true,
            Some((from, to)) => clamp_period(event_period(row), from, to).is_some(),
        };
        if in_window && filter.matches_row(row) {
            matched.push(row.id);
        }
    }
    for master in rows.iter().filter(|r| r.rrule.is_some()) {
        let hit = match filter.time_range {
            // No time-range: occurrences share the master's properties, so
            // the master row plus its overrides decide.
            None => {
                filter.matches_row(master)
                    || exceptions
                        .iter()
                        .any(|ex| ex.master_event_id == Some(master.id) && filter.matches_row(ex))
            }
            Some((from, to)) => {
                recurring_candidates(master, exceptions, resolver, from, to)
                    .into_iter()
                    .any(|(period, _override_row)| {
                        let Some((start, end)) = period else {
                            return false; // overridden: the exception decides below
                        };
                        start < to && end > from && filter.matches_row(master)
                    })
                    || exceptions.iter().any(|ex| {
                        ex.master_event_id == Some(master.id)
                            && event_period(ex).is_some_and(|(start, end)| start < to && end > from)
                            && filter.matches_row(ex)
                    })
            }
        };
        if hit {
            matched.push(master.id);
        }
    }
    matched
}

async fn calendar_query_report(
    pool: &sqlx::PgPool,
    creds: &DavAuth,
    path: &str,
    body: &str,
) -> Result<axum::response::Response, AppError> {
    let Some((calendar, cap)) = calendar_at(pool, creds, path).await else {
        return Ok(not_found());
    };
    // Content read: require read access like free-busy; unreadable 404s.
    if !cap.satisfies(CalendarCapability::ReadOnly) {
        return Ok(not_found());
    }
    let root = crate::xml::parse(body).unwrap_or_else(|| xmltree::Element::new("calendar-query"));
    let (filter, props) = parse_calendar_query(&root);
    let share = creds.share_calendar_id.is_some();
    let rows: Vec<db::EventRow> = match filter.time_range {
        Some((from, to)) => db::list_events_in_range(pool, calendar.id, from, to).await?,
        None => sqlx::query_as::<_, db::EventRow>(
            "SELECT * FROM events WHERE calendar_id = $1 AND deleted_at IS NULL",
        )
        .bind(calendar.id)
        .fetch_all(pool)
        .await
        .map_err(db::DbError::from)?,
    };
    let rows: Vec<db::EventRow> = rows
        .into_iter()
        .filter(|r| !share || r.class.as_deref() == Some("PUBLIC"))
        .collect();
    let masters: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.rrule.is_some())
        .map(|r| r.id)
        .collect();
    let exceptions: Vec<db::EventRow> = db::list_exceptions(pool, &masters)
        .await?
        .into_iter()
        .filter(|ex| !share || ex.class.as_deref() == Some("PUBLIC"))
        .collect();
    let want_etag = props.is_empty() || props.iter().any(|p| p == "getetag");
    let want_data = props.iter().any(|p| p == "calendar-data");
    let resolver = calendar_zone_resolver(pool, calendar.id).await;
    let vtimezones = db::timezones::list_for_calendar(pool, calendar.id)
        .await
        .unwrap_or_default();
    let mut out = String::from(concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#,
    ));
    for id in matching_resources(&rows, &exceptions, &resolver, &filter) {
        let Some(row) = rows.iter().find(|r| r.id == id) else {
            continue;
        };
        let href = row
            .href
            .clone()
            .unwrap_or_else(|| format!("{}.ics", row.id));
        out.push_str("<D:response><D:href>");
        out.push_str(&xml_escape(&format!(
            "{}/{}",
            path.trim_end_matches('/'),
            percent_encode_path(&href)
        )));
        out.push_str("</D:href><D:propstat><D:prop>");
        if want_etag {
            out.push_str(&format!("<D:getetag>{}</D:getetag>", xml_escape(&row.etag)));
        }
        if want_data {
            out.push_str(&format!(
                "<C:calendar-data>{}</C:calendar-data>",
                xml_escape(&resource_ics(pool, row, share, &vtimezones).await)
            ));
        }
        out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>");
    }
    out.push_str("</D:multistatus>");
    Ok((
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        out,
    )
        .into_response())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One resource name as a URL path segment (dav-server hrefs are encoded).
fn percent_encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            let mut out = String::with_capacity(seg.len());
            for byte in seg.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
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
    let parsed = crate::xml::parse(body);
    let time_range = parsed
        .as_ref()
        .and_then(|root| crate::xml::find(root, "time-range"));
    let start = time_range
        .as_ref()
        .and_then(|tr| tr.attributes.get("start"))
        .and_then(|s| parse_ics_time(s))
        .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
    let end = time_range
        .as_ref()
        .and_then(|tr| tr.attributes.get("end"))
        .and_then(|s| parse_ics_time(s))
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
        // Unknown collections are untouched, but calendar-collection hrefs
        // still gain the supported-report-set advertisement.
        let unsets = rewrite_component_set_xml(PROPFIND, &HashMap::new());
        // Even without a stored set, calendar-collection hrefs gain the
        // supported-report-set advertisement (the hardcoded component set
        // stays as dav-server emitted it).
        assert!(unsets.contains(FULL_REPORT_SET));
        assert!(unsets.contains(r#"<C:comp name="VTODO"/>"#));
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

    /// matching_resources with a tzdb-only resolver (no stored VTIMEZONEs).
    fn matched(
        rows: &[db::EventRow],
        exceptions: &[db::EventRow],
        filter: &QueryFilter,
    ) -> Vec<Uuid> {
        matching_resources(
            rows,
            exceptions,
            &calendar_core::recurrence::TzResolver::default(),
            filter,
        )
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
                &calendar_core::recurrence::TzResolver::default(),
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
        let periods = busy_periods_from(
            &[master],
            &[ex],
            &calendar_core::recurrence::TzResolver::default(),
            from,
            to,
        );
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
            busy_periods_from(
                &[master],
                &[],
                &calendar_core::recurrence::TzResolver::default(),
                from,
                to
            ),
            vec![(at(2026, 9, 14, 9), at(2026, 9, 14, 10))]
        );
    }

    // ===== REPORT routing (root element, not substring) =====

    #[test]
    fn ics_time_parses_zulu_naive_and_date_only_values() {
        let zulu = Utc.with_ymd_and_hms(2026, 9, 14, 9, 30, 0).unwrap();
        assert_eq!(parse_ics_time("20260914T093000Z"), Some(zulu));
        assert_eq!(parse_ics_time("20260914T093000"), Some(zulu));
        assert_eq!(parse_ics_time(" 20260914T093000Z "), Some(zulu));
        assert_eq!(parse_ics_time("20260914"), Some(at(2026, 9, 14, 0)));
        assert_eq!(parse_ics_time("not-a-time"), None);
    }

    #[test]
    fn report_routing_uses_the_root_element() {
        // A calendar-query whose text-match literally contains
        // "sync-collection" must not be routed to the sync handler.
        let sneaky = r#"<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
            <D:prop><D:getetag/></D:prop>
            <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
            <C:prop-filter name="SUMMARY"><C:text-match>sync-collection</C:text-match></C:prop-filter>
            </C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
        assert_eq!(report_kind(sneaky), ReportKind::CalendarQuery);
        // Previously, substring order routed this to sync_collection.
        let free_busy = r#"<C:free-busy-query xmlns:C="urn:ietf:params:xml:ns:caldav">
            <C:time-range start="20260914T000000Z" end="20260921T000000Z"/>
            <C:comment>see also sync-collection</C:comment></C:free-busy-query>"#;
        assert_eq!(report_kind(free_busy), ReportKind::FreeBusyQuery);
        assert_eq!(
            report_kind(
                r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token>1</D:sync-token></D:sync-collection>"#
            ),
            ReportKind::SyncCollection
        );
        assert_eq!(
            report_kind(
                r#"<D:calendar-multiget xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:calendar-multiget>"#
            ),
            ReportKind::Other
        );
        assert_eq!(report_kind("<a><b></a>"), ReportKind::Other);
    }

    #[test]
    fn query_body_parses_with_client_style_prefixes() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<x0:calendar-query xmlns:x0="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
  <D:prop><D:getetag/><x0:calendar-data/></D:prop>
  <x0:filter>
    <x0:comp-filter name="VCALENDAR">
      <x0:comp-filter name="VEVENT">
        <x0:time-range start="20260914T000000Z" end="20260921T000000Z"/>
        <x0:prop-filter name="SUMMARY">
          <x0:text-match collation="i;ascii-casemap" negate-condition="yes">stand</x0:text-match>
        </x0:prop-filter>
      </x0:comp-filter>
    </x0:comp-filter>
  </x0:filter>
</x0:calendar-query>"#;
        let root = crate::xml::parse(body).unwrap();
        let (filter, props) = parse_calendar_query(&root);
        assert_eq!(props, ["getetag", "calendar-data"]);
        assert_eq!(
            filter.time_range,
            Some((at(2026, 9, 14, 0), at(2026, 9, 21, 0)))
        );
        assert_eq!(filter.prop_filters.len(), 1);
        let pf = &filter.prop_filters[0];
        assert_eq!(pf.name, "SUMMARY");
        assert_eq!(pf.text.as_ref().unwrap().value, "stand");
        assert!(pf.text.as_ref().unwrap().negate);
        assert!(!filter.none);
    }

    // ===== calendar-query matching =====

    fn query_time_range(from: DateTime<Utc>, to: DateTime<Utc>) -> QueryFilter {
        QueryFilter {
            time_range: Some((from, to)),
            ..Default::default()
        }
    }

    #[test]
    fn query_time_range_matches_overlapping_singles_and_expands_recurrence() {
        let from = at(2026, 9, 14, 0);
        let to = at(2026, 9, 21, 0);
        let mut inside = base_row(Uuid::new_v4());
        inside.starts_at = Some(at(2026, 9, 15, 10));
        inside.ends_at = Some(at(2026, 9, 15, 11));
        let mut outside = base_row(Uuid::new_v4());
        outside.starts_at = Some(at(2026, 9, 30, 10));
        outside.ends_at = Some(at(2026, 9, 30, 11));
        let mut daily = base_row(Uuid::new_v4());
        daily.starts_at = Some(at(2026, 9, 14, 9));
        daily.ends_at = Some(at(2026, 9, 14, 10));
        daily.rrule = Some("FREQ=DAILY".into());
        let got = matched(
            &[inside.clone(), outside, daily.clone()],
            &[],
            &query_time_range(from, to),
        );
        // The recurring master appears once, not once per occurrence.
        assert_eq!(got, vec![inside.id, daily.id]);
    }

    #[test]
    fn query_exception_moved_out_of_window_does_not_match_master() {
        // Weekly Tuesday event; the 2026-09-15 occurrence is overridden and
        // moved out of the query window.
        let mut weekly = base_row(Uuid::new_v4());
        weekly.starts_at = Some(at(2026, 9, 15, 9));
        weekly.ends_at = Some(at(2026, 9, 15, 10));
        weekly.rrule = Some("FREQ=WEEKLY".into());
        let mut moved_out = base_row(Uuid::new_v4());
        moved_out.master_event_id = Some(weekly.id);
        moved_out.recurrence_id = Some(
            Utc.with_ymd_and_hms(2026, 9, 15, 9, 0, 0)
                .unwrap()
                .naive_utc(),
        );
        moved_out.starts_at = Some(at(2026, 9, 30, 9));
        moved_out.ends_at = Some(at(2026, 9, 30, 10));
        // A window covering only the overridden slot finds nothing: the
        // series occurrence that was there is replaced and moved away.
        let week_window = query_time_range(at(2026, 9, 15, 0), at(2026, 9, 16, 0));
        assert!(matched(&[weekly.clone()], &[moved_out.clone()], &week_window).is_empty());
        // With no time-range, the master row still matches on its own.
        assert!(!matched(&[weekly], &[moved_out], &QueryFilter::default()).is_empty());
    }

    #[test]
    fn query_exception_inside_the_window_matches_its_master() {
        let mut weekly = base_row(Uuid::new_v4());
        weekly.starts_at = Some(at(2026, 9, 15, 9));
        weekly.ends_at = Some(at(2026, 9, 15, 10));
        weekly.rrule = Some("FREQ=WEEKLY".into());
        let mut override_ = base_row(Uuid::new_v4());
        override_.master_event_id = Some(weekly.id);
        override_.recurrence_id = Some(
            Utc.with_ymd_and_hms(2026, 9, 15, 9, 0, 0)
                .unwrap()
                .naive_utc(),
        );
        override_.starts_at = Some(at(2026, 9, 15, 14));
        override_.ends_at = Some(at(2026, 9, 15, 15));
        let week_window = query_time_range(at(2026, 9, 15, 0), at(2026, 9, 16, 0));
        assert_eq!(
            matched(&[weekly.clone()], &[override_.clone()], &week_window),
            vec![weekly.id]
        );
        // The same override relocated outside the window stops matching.
        let mut moved = override_;
        moved.starts_at = Some(at(2026, 9, 30, 14));
        moved.ends_at = Some(at(2026, 9, 30, 15));
        assert!(matched(&[weekly], &[moved], &week_window).is_empty());
    }

    #[test]
    fn query_text_match_honors_negate_condition_and_definedness() {
        let mut standup = base_row(Uuid::new_v4());
        standup.starts_at = Some(at(2026, 9, 15, 9));
        standup.ends_at = Some(at(2026, 9, 15, 10));
        standup.summary = "Standup meeting".into();
        let mut lunch = base_row(Uuid::new_v4());
        lunch.starts_at = Some(at(2026, 9, 15, 12));
        lunch.ends_at = Some(at(2026, 9, 15, 13));
        lunch.summary = "Team lunch".into();
        let mut confidential = base_row(Uuid::new_v4());
        confidential.starts_at = Some(at(2026, 9, 15, 14));
        confidential.ends_at = Some(at(2026, 9, 15, 15));
        confidential.summary = "Doctor".into();
        confidential.class = Some("PRIVATE".into());
        let rows = [standup, lunch, confidential];

        let contains = QueryFilter {
            prop_filters: vec![PropFilter {
                name: "SUMMARY".into(),
                text: Some(TextMatch {
                    value: "stand".into(),
                    negate: false,
                }),
                defined: None,
            }],
            ..Default::default()
        };
        assert_eq!(matched(&rows, &[], &contains), vec![rows[0].id]);
        let negated = QueryFilter {
            prop_filters: vec![PropFilter {
                name: "SUMMARY".into(),
                text: Some(TextMatch {
                    value: "stand".into(),
                    negate: true,
                }),
                defined: None,
            }],
            ..Default::default()
        };
        assert_eq!(matched(&rows, &[], &negated), vec![rows[1].id, rows[2].id]);
        // is-not-defined: only rows without a CLASS match.
        let class_undefined = QueryFilter {
            prop_filters: vec![PropFilter {
                name: "CLASS".into(),
                text: None,
                defined: Some(false),
            }],
            ..Default::default()
        };
        assert_eq!(
            matched(&rows, &[], &class_undefined),
            vec![rows[0].id, rows[1].id]
        );
        // is-defined.
        let class_defined = QueryFilter {
            prop_filters: vec![PropFilter {
                name: "CLASS".into(),
                text: None,
                defined: Some(true),
            }],
            ..Default::default()
        };
        assert_eq!(matched(&rows, &[], &class_defined), vec![rows[2].id]);
    }

    #[test]
    fn query_excluding_vevent_matches_nothing() {
        let mut row = base_row(Uuid::new_v4());
        row.starts_at = Some(at(2026, 9, 15, 9));
        row.ends_at = Some(at(2026, 9, 15, 10));
        // A VTODO-only query against an event store matches nothing.
        let vtodo = QueryFilter {
            none: true,
            ..Default::default()
        };
        assert!(matched(&[row], &[], &vtodo).is_empty());
    }

    // ===== sync-collection =====

    fn change(seq: i64, deleted: bool) -> ChangeRow {
        ChangeRow {
            seq,
            resource_id: Uuid::new_v4(),
            operation: if deleted {
                "deleted".into()
            } else {
                "updated".into()
            },
            etag: Some(format!("\"{seq}\"")),
            deleted_at: None,
            class: Some("PUBLIC".into()),
            href: format!("{seq}.ics"),
            slug: "work".into(),
        }
    }

    #[test]
    fn sync_limit_truncates_but_never_drops_deletions() {
        let changes = vec![
            change(1, false),
            change(2, false),
            change(3, true),
            change(4, false),
        ];
        // Limit 2: the first two updates set the token; the deletion behind
        // the limit is still reported, so the client cannot miss it.
        let (selected, token) = truncate_changes(changes.clone(), Some(2), 99);
        assert_eq!(
            selected
                .iter()
                .map(|r| (r.seq, r.is_delete()))
                .collect::<Vec<_>>(),
            vec![(1, false), (2, false), (3, true)]
        );
        assert_eq!(token, 2);
        // No limit (or a limit covering everything): token is the current max.
        let (selected, token) = truncate_changes(changes.clone(), None, 99);
        assert_eq!(selected.len(), 4);
        assert_eq!(token, 99);
        let (selected, token) = truncate_changes(changes, Some(10), 99);
        assert_eq!(selected.len(), 4);
        assert_eq!(token, 99);
        // A limit of zero yields an empty page (with a usable token); the
        // REPORT layer maps nresults <= 0 to "no limit".
        let (selected, token) = truncate_changes(vec![change(5, false)], Some(0), 99);
        assert!(selected.is_empty());
        assert_eq!(token, 99);
    }

    #[test]
    fn sync_dedupes_to_the_newest_change_per_resource() {
        let mut a1 = change(1, false);
        let a2 = change(4, false);
        a1.resource_id = a2.resource_id;
        let b1 = change(2, false);
        let latest = latest_changes(vec![a1, b1.clone(), a2], false);
        assert_eq!(latest.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2, 4]);
        // Tombstones hide from share principals; the newest change is kept.
        let mut deleted = change(3, true);
        deleted.class = Some("PRIVATE".into());
        assert!(latest_changes(vec![deleted.clone()], true).is_empty());
        assert_eq!(latest_changes(vec![deleted], false).len(), 1);
        assert!(!b1.is_delete());
    }

    #[test]
    fn sync_soft_deleted_rows_report_as_deletions() {
        let mut row = change(1, false);
        row.deleted_at = Some(Utc::now());
        assert!(row.is_delete());
    }

    #[test]
    fn home_set_sync_prefixes_the_calendar_slug() {
        assert_eq!(change_suffix(false, "work", "abc.ics"), "abc.ics");
        assert_eq!(change_suffix(true, "work", "abc.ics"), "work/abc.ics");
    }

    // ===== PROPFIND supported-report-set advertisement =====

    #[test]
    fn propfind_advertises_implemented_reports_per_target() {
        let xml = r#"<D:multistatus xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
<D:response><D:href>/calendars/alice/</D:href><D:propstat><D:prop><D:displayname>Home</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/calendars/alice/work/</D:href><D:propstat><D:prop><D:displayname>Work</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/calendars/alice/work/x.ics</D:href><D:propstat><D:prop><D:getetag>"1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#;
        let out = rewrite_component_set_xml(xml, &HashMap::new());
        // Home set: sync-collection only.
        let home = out.split("</D:response>").next().unwrap();
        assert!(home.contains(SYNC_REPORT_SET));
        assert!(!home.contains("<C:calendar-query/>"));
        // Calendar collection: all three implemented reports.
        let work = out.split("</D:response>").nth(1).unwrap();
        assert!(work.contains(FULL_REPORT_SET));
        // Resources carry no report set.
        let resource = out.split("</D:response>").nth(2).unwrap();
        assert!(!resource.contains("supported-report-set"));
    }
}
