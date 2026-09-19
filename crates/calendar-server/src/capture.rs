//! Dev-only DAV request capture for the client fixture session
//! (docs/INTEROP_CAPTURE.md). Off unless `DAV_CAPTURE_DIR` is set: one text
//! file per request holding the method, URL, headers, body and the response
//! status. Only DAV traffic is captured, never `/api`, so login passwords and
//! tokens are not written. `Authorization` and `Cookie` values are redacted,
//! but bodies are calendar content: point this at a throwaway directory.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bodies past this are cut in the file (the request itself is untouched).
const MAX_BODY: usize = 1 << 20;
static SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("DAV_CAPTURE_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}

/// The calendar mount and discovery. `/` counts only for DAV methods
/// (PROPFIND, OPTIONS...), not the web UI's GET.
fn is_dav(method: &Method, path: &str) -> bool {
    path.starts_with("/calendars")
        || path.starts_with("/.well-known/")
        || (path == "/" && method != Method::GET && method != Method::HEAD)
}

fn render(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> String {
    let mut out = format!("{method} {uri}\n");
    for (name, value) in headers {
        let shown = match name.as_str() {
            "authorization" | "proxy-authorization" | "cookie" => "<redacted>",
            _ => value.to_str().unwrap_or("<binary>"),
        };
        out.push_str(&format!("{name}: {shown}\n"));
    }
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&body[..body.len().min(MAX_BODY)]));
    if body.len() > MAX_BODY {
        out.push_str("\n<truncated>");
    }
    out
}

fn file_name(seq: u64, method: &Method, path: &str) -> String {
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(60)
        .collect();
    format!("{seq:06}-{method}-{slug}.txt")
}

pub(crate) async fn middleware(request: Request, next: Next) -> Response {
    let Some(dir) = dir().filter(|_| is_dav(request.method(), request.uri().path())) else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 256 * 1024 * 1024).await else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let mut text = render(&parts.method, &parts.uri, &parts.headers, &bytes);
    let file = dir.join(file_name(
        SEQ.fetch_add(1, Ordering::Relaxed),
        &parts.method,
        parts.uri.path(),
    ));
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    text.push_str(&format!(
        "\n\n--- response ---\nstatus: {}\n",
        response.status()
    ));
    if let Err(e) = write(&file, &text).await {
        tracing::warn!(error = %e, "DAV capture write failed");
    }
    response
}

async fn write(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(file, text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dav_traffic_is_captured() {
        assert!(is_dav(&Method::PUT, "/calendars/alice/work/a.ics"));
        assert!(is_dav(&Method::from_bytes(b"PROPFIND").unwrap(), "/"));
        assert!(is_dav(&Method::GET, "/.well-known/caldav"));
        assert!(!is_dav(&Method::GET, "/"));
        assert!(!is_dav(&Method::POST, "/api/auth/login"));
        assert!(!is_dav(&Method::GET, "/contacts/x"));
    }

    #[test]
    fn credentials_are_redacted() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic c2VjcmV0".parse().unwrap());
        headers.insert("cookie", "session=abc".parse().unwrap());
        headers.insert("content-type", "text/calendar".parse().unwrap());
        let out = render(
            &Method::PUT,
            &"/calendars/a/b/c.ics".parse().unwrap(),
            &headers,
            b"BEGIN:VCALENDAR",
        );
        assert!(!out.contains("c2VjcmV0") && !out.contains("session=abc"));
        assert!(out.contains("authorization: <redacted>"));
        assert!(out.contains("content-type: text/calendar"));
        assert!(out.ends_with("BEGIN:VCALENDAR"));
    }

    #[test]
    fn file_names_sort_by_arrival_and_are_path_safe() {
        assert_eq!(
            file_name(7, &Method::PUT, "/calendars/a b/c.ics"),
            "000007-PUT-_calendars_a_b_c_ics.txt"
        );
    }
}
