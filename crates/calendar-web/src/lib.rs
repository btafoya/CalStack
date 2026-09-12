//! Embedded web UI (docs/PRD.md section 18): Bootstrap 5.3 + jQuery 4 +
//! jQuery Migrate + vendored bs-calendar, all served from the executable, no
//! CDN, no build step. Server-rendered shells with progressive enhancement.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
};

// ============ embedded assets ============

macro_rules! asset {
    ($path:literal) => {
        include_bytes!(concat!("assets/", $path))
    };
}

static ASSETS: &[(&str, &[u8], &str)] = &[
    (
        "css/bootstrap.min.css",
        asset!("css/bootstrap.min.css"),
        "text/css; charset=utf-8",
    ),
    (
        "css/bootstrap-icons.css",
        asset!("css/bootstrap-icons.css"),
        "text/css; charset=utf-8",
    ),
    (
        "fonts/bootstrap-icons.woff2",
        asset!("fonts/bootstrap-icons.woff2"),
        "font/woff2",
    ),
    (
        "fonts/bootstrap-icons.woff",
        asset!("fonts/bootstrap-icons.woff"),
        "font/woff",
    ),
    (
        "js/bootstrap.bundle.min.js",
        asset!("js/bootstrap.bundle.min.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/jquery.min.js",
        asset!("js/jquery.min.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/jquery-migrate.min.js",
        asset!("js/jquery-migrate.min.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/bs-calendar.min.js",
        asset!("js/bs-calendar.min.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/app.js",
        asset!("js/app.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/api.js",
        asset!("js/api.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/rules.js",
        asset!("js/rules.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/admin.js",
        asset!("js/admin.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/providers.js",
        asset!("js/providers.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "css/summernote-bs5.min.css",
        asset!("css/summernote-bs5.min.css"),
        "text/css; charset=utf-8",
    ),
    (
        "css/font/summernote.woff2",
        asset!("css/font/summernote.woff2"),
        "font/woff2",
    ),
    (
        "css/font/summernote.woff",
        asset!("css/font/summernote.woff"),
        "font/woff",
    ),
    (
        "css/font/summernote.ttf",
        asset!("css/font/summernote.ttf"),
        "font/ttf",
    ),
    (
        "js/summernote-bs5.min.js",
        asset!("js/summernote-bs5.min.js"),
        "text/javascript; charset=utf-8",
    ),
];

async fn assets(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    for (name, bytes, mime) in ASSETS {
        if *name == path {
            // ponytail: no cache-busting versioning; flip Cache-Control when
            // assets start churning between deploys.
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("public, max-age=60"),
                    ),
                ],
                bytes.to_vec(),
            )
                .into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

// ============ pages ============

const LOGIN_PAGE: &str = r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Calendar — Sign in</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
</head>
<body class="d-flex align-items-center bg-body-tertiary" style="min-height:100vh">
<div class="container" style="max-width:420px">
  <form id="login-form" class="card p-4 mt-5">
    <h1 class="h4 mb-3">Calendar</h1>
    <div class="mb-3"><label class="form-label" for="user">Username or email</label>
      <input class="form-control" id="user" name="user" autocomplete="username" required></div>
    <div class="mb-3"><label class="form-label" for="pass">Password</label>
      <input class="form-control" id="pass" type="password" autocomplete="current-password" required></div>
    <div class="mb-3" id="totp-row" hidden><label class="form-label" for="totp">2FA code</label>
      <input class="form-control" id="totp" inputmode="numeric" autocomplete="one-time-code"></div>
    <button class="btn btn-primary" type="submit">Sign in</button>
    <div id="error" class="alert alert-danger mt-3 mb-0 d-none" role="alert"></div>
  </form>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script>
$(function () {
  $('#login-form').on('submit', function (ev) {
    ev.preventDefault();
    $('#error').addClass('d-none');
    $.ajax({
      method: 'POST',
      url: '/api/auth/login',
      contentType: 'application/json',
      data: JSON.stringify({
        username_or_email: $('#user').val(),
        password: $('#pass').val(),
        totp_code: $('#totp').val() || null,
      }),
    })
      .done(function (resp) {
        sessionStorage.setItem('csrf', resp.csrf_token);
        window.location.href = '/';
      })
      .fail(function (xhr) {
        if (xhr.status === 401 && $('#totp-row').prop('hidden')) {
          $('#totp-row').prop('hidden', false);
          $('#error').text('Enter your two-factor code.').removeClass('d-none');
        } else {
          $('#error').text(xhr.responseJSON && xhr.responseJSON.error || 'Sign in failed').removeClass('d-none');
        }
      });
  });
});
</script>
</body></html>"#;

const APP_PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Calendar</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/summernote-bs5.min.css">
<style>
  /* ponytail: bs-calendar's own left-hand nav drawer (button[data-bs-toggle="sidebar"])
     duplicates our calendars sidebar and slides in on top of it; simplest fix
     is to not offer the redundant second drawer at all. */
  #calendar [data-bs-toggle="sidebar"] { display: none !important; }
</style>
</head>
<body class="bg-body-tertiary vh-100 overflow-hidden d-flex flex-column">
<nav class="navbar bg-body border-bottom px-3 flex-shrink-0">
  <a class="navbar-brand" href="/"><i class="bi bi-calendar3" aria-hidden="true"></i> Calendar</a>
  <div class="ms-auto d-flex gap-2">
    <button id="search-btn" class="btn btn-outline-secondary btn-sm" type="button"><i class="bi bi-search"></i> Search</button>
    <a id="rules-link" class="btn btn-outline-secondary btn-sm" href="/rules"><i class="bi bi-sliders"></i> Rules</a>
    <a class="btn btn-outline-secondary btn-sm" href="/providers"><i class="bi bi-bell"></i> Providers</a>
    <a id="admin-nav-link" class="btn btn-outline-secondary btn-sm" href="/admin" hidden><i class="bi bi-shield-lock"></i> Admin</a>
    <button id="share-btn" class="btn btn-outline-secondary btn-sm" type="button"><i class="bi bi-share"></i> Share</button>
    <button id="logout-btn" class="btn btn-outline-secondary btn-sm" type="button">Log out</button>
  </div>
</nav>
<div class="container-fluid flex-grow-1 overflow-hidden">
  <div class="row h-100">
    <aside class="col-md-3 col-lg-2 p-3 border-end h-100 overflow-auto">
      <div class="d-flex justify-content-between align-items-center mb-2">
        <span class="fw-semibold">Calendars</span>
        <button id="add-cal-btn" class="btn btn-sm btn-outline-primary" type="button" aria-label="Add calendar">+</button>
      </div>
      <ul id="cal-list" class="list-group list-group-flush"></ul>
      <div class="d-flex justify-content-between align-items-center mb-2 mt-4">
        <span class="fw-semibold">Subscriptions</span>
      </div>
      <div class="input-group input-group-sm mb-2">
        <input id="sub-token" class="form-control" placeholder="Share token">
        <button id="sub-add-btn" class="btn btn-outline-primary" type="button">Add</button>
      </div>
      <ul id="sub-list" class="list-group list-group-flush small"></ul>
    </aside>
    <main class="col-md-9 col-lg-10 p-3 h-100 overflow-auto">
      <div id="calendar" hidden></div>
      <p id="calendar-empty" class="text-body-secondary text-center mt-5">Select a calendar to view its events.</p>
    </main>
  </div>
</div>
<!-- event editor -->
<div class="modal fade" id="event-modal" tabindex="-1" aria-hidden="true">
  <div class="modal-dialog"><form id="event-form" class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Event</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close"></button></div>
    <div class="modal-body">
      <div class="mb-3"><label class="form-label" for="ev-title">Title</label>
        <input class="form-control" id="ev-title" required></div>
      <div class="row mb-3"><div class="col"><label class="form-label" for="ev-start">Start</label>
        <input class="form-control" id="ev-start" type="datetime-local" required></div>
      <div class="col"><label class="form-label" for="ev-end">End</label>
        <input class="form-control" id="ev-end" type="datetime-local" required></div></div>
      <div class="form-check mb-3">
        <input class="form-check-input" type="checkbox" id="ev-all-day">
        <label class="form-check-label" for="ev-all-day">All day</label>
      </div>
      <div class="row mb-3">
        <div class="col"><label class="form-label" for="ev-location-name">Location</label>
          <input class="form-control" id="ev-location-name" placeholder="Name"></div>
        <div class="col"><label class="form-label" for="ev-location-address">&nbsp;</label>
          <input class="form-control" id="ev-location-address" placeholder="Address"></div>
      </div>
      <div class="mb-3"><label class="form-label" for="ev-url">URL</label>
        <input class="form-control" id="ev-url" type="url"></div>
      <div class="row mb-3">
        <div class="col"><label class="form-label" for="ev-status">Status</label>
          <select class="form-select" id="ev-status">
            <option value="">(none)</option>
            <option value="CONFIRMED">Confirmed</option>
            <option value="TENTATIVE">Tentative</option>
            <option value="CANCELLED">Cancelled</option>
          </select></div>
        <div class="col"><label class="form-label" for="ev-class">Visibility</label>
          <select class="form-select" id="ev-class">
            <option value="">(none)</option>
            <option value="PUBLIC">Public</option>
            <option value="PRIVATE">Private</option>
            <option value="CONFIDENTIAL">Confidential</option>
          </select></div>
        <div class="col"><label class="form-label" for="ev-transp">Show as</label>
          <select class="form-select" id="ev-transp">
            <option value="">(none)</option>
            <option value="OPAQUE">Busy</option>
            <option value="TRANSPARENT">Free</option>
          </select></div>
      </div>
      <div class="mb-3"><label class="form-label" for="ev-categories">Categories (comma-separated)</label>
        <input class="form-control" id="ev-categories"></div>
      <div class="row mb-3">
        <div class="col"><label class="form-label" for="ev-repeat">Repeat</label>
          <select class="form-select" id="ev-repeat">
            <option value="">Does not repeat</option>
            <option value="DAILY">Daily</option>
            <option value="WEEKLY">Weekly</option>
            <option value="MONTHLY">Monthly</option>
            <option value="YEARLY">Yearly</option>
          </select></div>
        <div class="col" id="ev-repeat-interval-row" hidden>
          <label class="form-label" for="ev-repeat-interval">Every</label>
          <input class="form-control" id="ev-repeat-interval" type="number" min="1" value="1"></div>
        <div class="col" id="ev-repeat-until-row" hidden>
          <label class="form-label" for="ev-repeat-until">Until</label>
          <input class="form-control" id="ev-repeat-until" type="date"></div>
      </div>
      <div class="mb-3"><label class="form-label" for="ev-desc">Description</label>
        <div id="ev-desc"></div></div>
      <div class="mb-3">
        <label class="form-label">Attendees</label>
        <ul id="ev-attendees" class="list-group list-group-flush mb-2"></ul>
        <div class="input-group input-group-sm">
          <input id="ev-attendee-email" class="form-control" type="email" placeholder="Email">
          <input id="ev-attendee-name" class="form-control" placeholder="Name (optional)">
          <button id="ev-attendee-add" class="btn btn-outline-primary" type="button">Add</button>
        </div>
      </div>
      <div class="mb-3" id="ev-attachments-section" hidden>
        <label class="form-label">Attachments</label>
        <ul id="ev-attachments" class="list-group list-group-flush mb-2"></ul>
        <input type="file" id="ev-attach-file" class="form-control form-control-sm">
      </div>
    </div>
    <div class="modal-footer">
      <button type="button" class="btn btn-outline-danger me-auto" id="ev-delete" hidden>Delete</button>
      <button class="btn btn-secondary" type="button" data-bs-dismiss="modal">Cancel</button>
      <button class="btn btn-primary" type="submit">Save</button>
    </div>
  </div></div>
</div>
<div class="modal fade" id="share-modal" aria-hidden="true">
  <div class="modal-dialog modal-lg modal-dialog-scrollable"><div class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Sharing</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <div class="row g-2 mb-3">
        <div class="col"><input id="acl-user" class="form-control" placeholder="User UUID"></div>
        <div class="col-auto"><select id="acl-cap" class="form-select">
          <option>read_only</option><option>read_write</option><option>owner</option><option>free_busy</option>
        </select></div>
        <div class="col-auto"><button id="acl-add" class="btn btn-primary" type="button">Add</button></div>
      </div>
      <table class="table table-sm"><tbody id="acl-rows"></tbody></table>
      <hr>
      <div class="d-flex gap-2">
        <button id="share-create" class="btn btn-outline-primary btn-sm" type="button">Create public link</button>
        <button id="share-create-caldav" class="btn btn-outline-primary btn-sm" type="button">Create link + CalDAV</button>
      </div>
      <div id="share-out" class="mt-2"></div>
    </div>
  </div></div>
</div>
<div class="modal fade" id="search-modal" aria-hidden="true">
  <div class="modal-dialog modal-lg modal-dialog-scrollable"><div class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Search</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <div class="input-group mb-3">
        <input id="search-q" class="form-control" placeholder="Search events…">
        <button id="search-go" class="btn btn-primary" type="button">Search</button>
      </div>
      <ul id="search-results" class="list-group"></ul>
    </div>
  </div></div>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/bs-calendar.min.js"></script>
<script src="/assets/js/summernote-bs5.min.js"></script>
<script src="/assets/js/app.js?v=6"></script>
</body></html>"#;

const RULES_PAGE: &str = r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Calendar — Rules</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
</head>
<body class="bg-body-tertiary">
<nav class="navbar bg-body border-bottom px-3">
  <a class="navbar-brand" href="/"><i class="bi bi-calendar3" aria-hidden="true"></i> Calendar</a>
  <div class="ms-auto"><a class="btn btn-outline-secondary btn-sm" href="/">Back</a></div>
</nav>
<div class="container p-3">
  <div class="d-flex align-items-center gap-2 mb-1">
    <h1 class="h4 mb-0">Rules</h1>
    <select class="form-select form-select-sm w-auto" id="rule-calendar-select">
      <option value="">All calendars</option>
    </select>
  </div>
  <p id="rules-scope-note" class="text-body-secondary small"></p>
  <form id="rule-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col"><label class="form-label" for="rule-name">Name</label>
        <input class="form-control" id="rule-name" required></div>
      <div class="col-auto"><label class="form-label" for="rule-trigger">Trigger</label>
        <select class="form-select" id="rule-trigger">
          <option value="event_created">event_created</option>
        </select></div>
      <div class="col-auto form-check mb-2">
        <input class="form-check-input" type="checkbox" id="rule-enabled" checked>
        <label class="form-check-label" for="rule-enabled">Enabled</label></div>
      <div id="rule-global-row" class="col-auto form-check mb-2">
        <input class="form-check-input" type="checkbox" id="rule-global">
        <label class="form-check-label" for="rule-global">Apply to all calendars</label></div>
    </div>
    <div class="row g-2 mt-1">
      <div class="col-auto"><label class="form-label" for="rule-action-type">Action</label>
        <select class="form-select" id="rule-action-type">
          <option value="create_notification">In-app notification</option>
          <option value="sms">SMS (requires a Twilio provider)</option>
        </select></div>
      <div id="rule-title-row" class="col"><label class="form-label" for="rule-title">Notification title</label>
        <input class="form-control" id="rule-title" required></div>
      <div id="rule-to-row" class="col" hidden><label class="form-label" for="rule-to">To (phone number)</label>
        <input class="form-control" id="rule-to"></div>
      <div class="col"><label class="form-label" for="rule-body">Message</label>
        <input class="form-control" id="rule-body"></div>
      <div class="col-auto d-flex align-items-end">
        <button class="btn btn-primary" type="submit">Add rule</button></div>
    </div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Name</th><th>Trigger</th><th>Scope</th><th>Actions</th><th>Enabled</th><th></th></tr></thead>
    <tbody id="rules-rows"></tbody>
  </table>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/rules.js"></script>
</body></html>"#;

const ADMIN_PAGE: &str = r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Calendar — Admin</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
</head>
<body class="bg-body-tertiary">
<nav class="navbar bg-body border-bottom px-3">
  <a class="navbar-brand" href="/"><i class="bi bi-calendar3" aria-hidden="true"></i> Calendar</a>
  <div class="ms-auto"><a class="btn btn-outline-secondary btn-sm" href="/">Back</a></div>
</nav>
<div class="container p-3">
  <h1 class="h4 mb-3">Users</h1>
  <form id="user-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col"><label class="form-label" for="u-username">Username</label>
        <input class="form-control" id="u-username" required></div>
      <div class="col"><label class="form-label" for="u-email">Email</label>
        <input class="form-control" id="u-email" type="email" required></div>
      <div class="col"><label class="form-label" for="u-password">Password</label>
        <input class="form-control" id="u-password" type="password" minlength="8" required></div>
      <div class="col-auto form-check mb-2">
        <input class="form-check-input" type="checkbox" id="u-is-admin">
        <label class="form-check-label" for="u-is-admin">Admin</label></div>
      <div class="col-auto"><button class="btn btn-primary" type="submit">Add user</button></div>
    </div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Username</th><th>Email</th><th>Admin</th><th>Disabled</th></tr></thead>
    <tbody id="user-rows"></tbody>
  </table>
  <h1 class="h4 mb-3 mt-4">Audit log</h1>
  <table class="table table-sm bg-body">
    <thead><tr><th>When</th><th>Action</th><th>Object</th><th>Summary</th></tr></thead>
    <tbody id="audit-rows"></tbody>
  </table>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/admin.js"></script>
</body></html>"#;

const PROVIDERS_PAGE: &str = r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Calendar — Providers</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
</head>
<body class="bg-body-tertiary">
<nav class="navbar bg-body border-bottom px-3">
  <a class="navbar-brand" href="/"><i class="bi bi-calendar3" aria-hidden="true"></i> Calendar</a>
  <div class="ms-auto"><a class="btn btn-outline-secondary btn-sm" href="/">Back</a></div>
</nav>
<div class="container p-3">
  <h1 class="h4 mb-3">Notification providers</h1>
  <form id="provider-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col-auto"><label class="form-label" for="provider-kind">Kind</label>
        <select class="form-select" id="provider-kind">
          <option value="postmark">Postmark (email)</option>
          <option value="smtp">SMTP (email)</option>
          <option value="twilio">Twilio (SMS)</option>
        </select></div>
      <div class="col"><label class="form-label" for="provider-name">Name</label>
        <input class="form-control" id="provider-name" required></div>
    </div>
    <div id="provider-fields" class="row g-2 mt-1"></div>
    <div class="mt-2"><button class="btn btn-primary" type="submit">Add provider</button></div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Kind</th><th>Name</th><th>Enabled</th><th></th></tr></thead>
    <tbody id="provider-rows"></tbody>
  </table>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/providers.js"></script>
</body></html>"#;

async fn providers_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        PROVIDERS_PAGE,
    )
}

async fn rules_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        RULES_PAGE,
    )
}

async fn admin_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        ADMIN_PAGE,
    )
}

async fn index() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        APP_PAGE_HEAD,
    )
}

async fn login_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        LOGIN_PAGE,
    )
}

pub fn router<S: Clone + Send + Sync + 'static>() -> axum::Router<S> {
    axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/login", axum::routing::get(login_page))
        .route("/rules", axum::routing::get(rules_page))
        .route("/admin", axum::routing::get(admin_page))
        .route("/providers", axum::routing::get(providers_page))
        .route("/assets/{*path}", axum::routing::get(assets))
}
