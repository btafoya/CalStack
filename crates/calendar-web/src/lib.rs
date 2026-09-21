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
        "sw.js",
        asset!("js/sw.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "css/bootstrap.min.css",
        asset!("css/bootstrap.min.css"),
        "text/css; charset=utf-8",
    ),
    (
        "css/app.css",
        asset!("css/app.css"),
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
    ("img/logo.png", asset!("img/logo.png"), "image/png"),
    (
        "img/logo-horizontal-small.png",
        asset!("img/logo-horizontal-small.png"),
        "image/png",
    ),
    (
        "img/favicon-16x16.png",
        asset!("img/favicon-16x16.png"),
        "image/png",
    ),
    (
        "img/favicon-32x32.png",
        asset!("img/favicon-32x32.png"),
        "image/png",
    ),
    (
        "img/apple-touch-icon.png",
        asset!("img/apple-touch-icon.png"),
        "image/png",
    ),
    (
        "js/theme.js",
        asset!("js/theme.js"),
        "text/javascript; charset=utf-8",
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
        "js/dialogs.js",
        asset!("js/dialogs.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/rules.js",
        asset!("js/rules.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/categories.js",
        asset!("js/categories.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/contacts.js",
        asset!("js/contacts.js"),
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
        "js/credentials.js",
        asset!("js/credentials.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/swagger-ui-bundle.js",
        asset!("js/swagger-ui-bundle.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/swagger-ui-bundle.js.LICENSE.txt",
        asset!("js/swagger-ui-bundle.js.LICENSE.txt"),
        "text/plain; charset=utf-8",
    ),
    (
        "css/swagger-ui.css",
        asset!("css/swagger-ui.css"),
        "text/css; charset=utf-8",
    ),
    (
        "css/swagger-ui.css.map",
        asset!("css/swagger-ui.css.map"),
        "application/json",
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

macro_rules! footer_html {
    () => {
        concat!(
            r#"<footer class="footer footer-transparent d-print-none">
  <div class="container-xl">
    <div class="row text-center align-items-center flex-row-reverse">
      <div class="col-lg-auto ms-lg-auto">
        <nav aria-label="Footer">
          <ul class="list-inline list-inline-dots mb-0">
            <li class="list-inline-item"><a href="https://github.com/btafoya/CalStack/blob/main/LICENSE" target="_blank" class="link-secondary" rel="noopener">License</a></li>
            <li class="list-inline-item"><a href="https://github.com/btafoya/CalStack" target="_blank" class="link-secondary" rel="noopener">Source code</a></li>
          </ul>
        </nav>
      </div>
      <div class="col-12 col-lg-auto mt-3 mt-lg-0">
        <ul class="list-inline list-inline-dots mb-0">
          <li class="list-inline-item">Copyright © 2026 CalStack. All rights reserved.</li>
          <li class="list-inline-item">v"#,
            env!("CARGO_PKG_VERSION"),
            r#"</li>
        </ul>
      </div>
    </div>
  </div>
</footer>
"#
        )
    };
}

macro_rules! subpage_header {
    () => {
        r##"<header class="navbar navbar-expand-md d-print-none bg-body border-bottom">
  <div class="container-fluid">
    <button class="navbar-toggler" type="button" data-bs-toggle="collapse" data-bs-target="#navbar-menu" aria-controls="navbar-menu" aria-expanded="false" aria-label="Toggle navigation">
      <span class="navbar-toggler-icon"></span>
    </button>
    <a class="navbar-brand" href="/"><img src="/assets/img/logo-horizontal-small.png" alt="CalStack"></a>
    <div class="navbar-nav flex-row order-md-last">
      <div class="nav-item me-2">
        <button type="button" id="theme-toggle" class="nav-link px-0" aria-label="Toggle dark mode" title="Toggle dark mode"><i class="bi bi-circle-half"></i></button>
      </div>
      <div class="nav-item dropdown">
        <a href="#" class="nav-link d-flex lh-1 p-0 px-2" data-bs-toggle="dropdown" aria-label="Open user menu" aria-expanded="false"><i class="bi bi-person-circle fs-3"></i></a>
        <div class="dropdown-menu dropdown-menu-end dropdown-menu-arrow">
          <button type="button" class="dropdown-item" id="account-btn">Account</button>
          <button type="button" class="dropdown-item" id="logout-btn">Sign out</button>
        </div>
      </div>
    </div>
  </div>
</header>
"##
    };
}

macro_rules! nav_menu_gated {
    () => {
        r#"<div class="navbar-expand-md flex-shrink-0">
  <div class="collapse navbar-collapse" id="navbar-menu">
    <div class="navbar w-100 bg-body border-bottom">
      <div class="container-fluid">
        <ul class="navbar-nav">
          <li class="nav-item"><a class="nav-link" href="/"><span class="nav-link-icon me-1"><i class="bi bi-calendar3"></i></span><span class="nav-link-title">Calendar</span></a></li>
          <li class="nav-item"><a class="nav-link" href="/categories"><span class="nav-link-icon me-1"><i class="bi bi-tags"></i></span><span class="nav-link-title">Categories</span></a></li>
          <li class="nav-item"><a class="nav-link" href="/contacts-ui"><span class="nav-link-icon me-1"><i class="bi bi-person-lines-fill"></i></span><span class="nav-link-title">Contacts</span></a></li>
          <li class="nav-item"><a class="nav-link" id="rules-link" href="/rules" hidden><span class="nav-link-icon me-1"><i class="bi bi-sliders"></i></span><span class="nav-link-title">Rules</span></a></li>
          <li class="nav-item"><a class="nav-link" id="providers-nav-link" href="/providers" hidden><span class="nav-link-icon me-1"><i class="bi bi-bell"></i></span><span class="nav-link-title">Providers</span></a></li>
          <li class="nav-item"><a class="nav-link" id="credentials-nav-link" href="/credentials" hidden><span class="nav-link-icon me-1"><i class="bi bi-key"></i></span><span class="nav-link-title">Credentials</span></a></li>
          <li class="nav-item"><a class="nav-link" id="admin-nav-link" href="/admin" hidden><span class="nav-link-icon me-1"><i class="bi bi-shield-lock"></i></span><span class="nav-link-title">Admin</span></a></li>
        </ul>
      </div>
    </div>
  </div>
</div>
"#
    };
}

macro_rules! nav_menu_open {
    () => {
        r#"<div class="navbar-expand-md flex-shrink-0">
  <div class="collapse navbar-collapse" id="navbar-menu">
    <div class="navbar w-100 bg-body border-bottom">
      <div class="container-fluid">
        <ul class="navbar-nav">
          <li class="nav-item"><a class="nav-link" href="/"><span class="nav-link-icon me-1"><i class="bi bi-calendar3"></i></span><span class="nav-link-title">Calendar</span></a></li>
          <li class="nav-item"><a class="nav-link" href="/categories"><span class="nav-link-icon me-1"><i class="bi bi-tags"></i></span><span class="nav-link-title">Categories</span></a></li>
          <li class="nav-item"><a class="nav-link" href="/contacts-ui"><span class="nav-link-icon me-1"><i class="bi bi-person-lines-fill"></i></span><span class="nav-link-title">Contacts</span></a></li>
          <li class="nav-item"><a class="nav-link" id="rules-link" href="/rules"><span class="nav-link-icon me-1"><i class="bi bi-sliders"></i></span><span class="nav-link-title">Rules</span></a></li>
          <li class="nav-item"><a class="nav-link" id="providers-nav-link" href="/providers"><span class="nav-link-icon me-1"><i class="bi bi-bell"></i></span><span class="nav-link-title">Providers</span></a></li>
          <li class="nav-item"><a class="nav-link" id="credentials-nav-link" href="/credentials"><span class="nav-link-icon me-1"><i class="bi bi-key"></i></span><span class="nav-link-title">Credentials</span></a></li>
          <li class="nav-item"><a class="nav-link" id="admin-nav-link" href="/admin"><span class="nav-link-icon me-1"><i class="bi bi-shield-lock"></i></span><span class="nav-link-title">Admin</span></a></li>
        </ul>
      </div>
    </div>
  </div>
</div>
"#
    };
}

const LOGIN_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Sign in</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="d-flex flex-column bg-body-tertiary" style="min-height:100vh">
<div class="flex-grow-1 d-flex align-items-center">
<div class="container" style="max-width:420px">
  <form id="login-form" class="card p-4 mt-5">
    <h1 class="h4 mb-3">CalStack</h1>
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
"#,
    footer_html!(),
    r#"</body></html>"#
);

const APP_PAGE_HEAD: &str = concat!(
    r##"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/summernote-bs5.min.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
<style>
  /* ponytail: bs-calendar's own left-hand nav drawer (button[data-bs-toggle="sidebar"])
     duplicates our calendars sidebar and slides in on top of it; simplest fix
     is to not offer the redundant second drawer at all. */
  #calendar [data-bs-toggle="sidebar"] { display: none !important; }
</style>
</head>
<body class="bg-body-tertiary d-flex flex-column min-vh-100">
<header class="navbar navbar-expand-md d-print-none bg-body border-bottom flex-shrink-0">
  <div class="container-fluid">
    <button class="navbar-toggler" type="button" data-bs-toggle="collapse" data-bs-target="#navbar-menu" aria-controls="navbar-menu" aria-expanded="false" aria-label="Toggle navigation">
      <span class="navbar-toggler-icon"></span>
    </button>
    <a class="navbar-brand" href="/"><img src="/assets/img/logo-horizontal-small.png" alt="CalStack"></a>
    <div class="navbar-nav flex-row order-md-last">
      <div class="nav-item me-2 d-none d-md-flex">
        <button id="search-btn" class="nav-link px-2" type="button" aria-label="Search"><i class="bi bi-search"></i></button>
      </div>
      <div class="nav-item me-2 d-none d-md-flex">
        <button id="share-btn" class="nav-link px-2" type="button" aria-label="Share"><i class="bi bi-share"></i></button>
      </div>
      <div class="nav-item me-2">
        <button type="button" id="theme-toggle" class="nav-link px-0" aria-label="Toggle dark mode" title="Toggle dark mode"><i class="bi bi-circle-half"></i></button>
      </div>
      <div class="nav-item dropdown">
        <a href="#" class="nav-link d-flex lh-1 p-0 px-2" data-bs-toggle="dropdown" aria-label="Open user menu" aria-expanded="false"><i class="bi bi-person-circle fs-3"></i></a>
        <div class="dropdown-menu dropdown-menu-end dropdown-menu-arrow">
          <button type="button" class="dropdown-item" id="conn-info-btn">Connection info</button>
          <button type="button" class="dropdown-item" id="account-btn">Account</button>
          <button type="button" class="dropdown-item" id="logout-btn">Sign out</button>
        </div>
      </div>
    </div>
  </div>
</header>
"##,
    nav_menu_gated!(),
    r#"<div class="container-fluid">
  <div class="row">
    <aside class="col-md-3 col-lg-2 p-3 border-end">
      <div class="d-flex justify-content-between align-items-center mb-2">
        <span class="fw-semibold">Calendars</span>
        <button id="add-cal-btn" class="btn btn-outline-primary" style="width:2.75rem;height:2.75rem" type="button" aria-label="Add calendar">+</button>
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
    <main class="col-md-9 col-lg-10 p-3">
      <div id="calendar" hidden></div>
      <p id="calendar-empty" class="text-body-secondary text-center mt-5">Select a calendar to view its events.</p>
    </main>
  </div>
</div>
"#,
    footer_html!(),
    r#"<!-- event editor -->
<div class="modal fade" id="event-modal" tabindex="-1" aria-hidden="true" data-bs-backdrop="static" data-bs-keyboard="false">
  <div class="modal-dialog"><form id="event-form" class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Event</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close"></button></div>
    <div class="modal-body">
      <div class="mb-3"><label class="form-label" for="ev-title">Title</label>
        <input class="form-control" id="ev-title" required></div>
      <div class="mb-3"><label class="form-label" for="ev-start-date">Start</label>
        <div class="row g-2">
          <div class="col-5"><input class="form-control" id="ev-start-date" type="date" required></div>
          <div class="col-3"><select class="form-select" id="ev-start-hour"></select></div>
          <div class="col-2"><select class="form-select" id="ev-start-min"></select></div>
          <div class="col-2"><select class="form-select" id="ev-start-ampm"><option value="AM">AM</option><option value="PM">PM</option></select></div>
        </div>
        <input type="hidden" id="ev-start"></div>
      <div class="mb-3"><label class="form-label" for="ev-end-date">End</label>
        <div class="row g-2">
          <div class="col-5"><input class="form-control" id="ev-end-date" type="date" required></div>
          <div class="col-3"><select class="form-select" id="ev-end-hour"></select></div>
          <div class="col-2"><select class="form-select" id="ev-end-min"></select></div>
          <div class="col-2"><select class="form-select" id="ev-end-ampm"><option value="AM">AM</option><option value="PM">PM</option></select></div>
        </div>
        <input type="hidden" id="ev-end"></div>
      <div class="form-check mb-3">
        <input class="form-check-input" type="checkbox" id="ev-all-day">
        <label class="form-check-label" for="ev-all-day">All day</label>
      </div>
      <div class="row mb-3">
        <div class="col position-relative"><label class="form-label" for="ev-location">Location</label>
          <input class="form-control" id="ev-location" placeholder="Type a place or address" autocomplete="off">
          <div id="ev-places-menu" class="list-group position-absolute shadow" style="top:100%;left:0;right:0;z-index:1060" hidden></div></div>
      </div>
      <div class="mb-3"><label class="form-label">Categories</label>
        <div id="ev-categories-box"></div></div>
      <div class="mb-3"><label class="form-label" for="ev-desc">Description</label>
        <div id="ev-desc"></div></div>
      <!-- ponytail: native <details> over a JS-toggled div — free collapse
           state, no open/close JS needed. Doesn't auto-open when editing an
           event that already has e.g. a status set; add that if it bites. -->
      <details class="mb-3" id="ev-more-options">
        <summary class="form-label" style="cursor:pointer">More options</summary>
        <div class="mt-2">
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
        </div>
      </details>
      <div class="mb-3">
        <label class="form-label">Attendees</label>
        <ul id="ev-attendees" class="list-group list-group-flush mb-2"></ul>
        <div class="position-relative">
          <input id="ev-attendee-search" class="form-control form-control-sm" type="text" placeholder="Search contacts…" autocomplete="off">
          <div id="ev-attendee-results" class="list-group position-absolute w-100 shadow-sm" style="z-index: 1060;" hidden></div>
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
<div class="modal fade" id="account-modal" aria-hidden="true">
  <div class="modal-dialog"><div class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Change password</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <div class="mb-2"><label class="form-label" for="account-current-password">Current password</label>
        <input class="form-control" type="password" id="account-current-password" autocomplete="current-password"></div>
      <div class="mb-2"><label class="form-label" for="account-new-password">New password</label>
        <input class="form-control" type="password" id="account-new-password" autocomplete="new-password" minlength="8"></div>
      <div class="mb-2"><label class="form-label" for="account-new-password-confirm">Confirm new password</label>
        <input class="form-control" type="password" id="account-new-password-confirm" autocomplete="new-password" minlength="8"></div>
      <div id="account-password-msg" class="small text-body-secondary"></div>
      <hr>
      <h3 class="h6">Reminders</h3>
      <div class="form-check"><input class="form-check-input" type="checkbox" id="notify-email">
        <label class="form-check-label" for="notify-email">Email reminders</label></div>
      <div class="form-check"><input class="form-check-input" type="checkbox" id="notify-sms">
        <label class="form-check-label" for="notify-sms">SMS reminders</label></div>
      <div class="form-check"><input class="form-check-input" type="checkbox" id="notify-push">
        <label class="form-check-label" for="notify-push">Push notifications</label></div>
      <div class="mt-2 d-flex gap-2">
        <button id="notify-prefs-save" class="btn btn-outline-primary btn-sm" type="button">Save reminder settings</button>
        <button id="push-enable-btn" class="btn btn-outline-secondary btn-sm" type="button">Enable push on this device</button>
      </div>
      <div id="notify-msg" class="small text-body-secondary mt-1"></div>
    </div>
    <div class="modal-footer">
      <button id="account-password-save" class="btn btn-primary" type="button">Change password</button>
    </div>
  </div></div>
</div>
<div class="modal fade" id="conn-modal" aria-hidden="true">
  <div class="modal-dialog"><div class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Connection info</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <div class="mb-3"><label class="form-label">Server URL</label>
        <div class="input-group"><input id="conn-server" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div class="mb-3"><label class="form-label">Username</label>
        <div class="input-group"><input id="conn-username" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div class="mb-3"><label class="form-label">CalDAV root (calendar home)</label>
        <div class="input-group"><input id="conn-caldav-root" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div class="mb-3"><label class="form-label">CalDAV discovery</label>
        <div class="input-group"><input id="conn-caldav-wk" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div class="mb-3"><label class="form-label">CardDAV contacts</label>
        <div class="input-group"><input id="conn-carddav" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div class="mb-3"><label class="form-label">CardDAV discovery</label>
        <div class="input-group"><input id="conn-carddav-wk" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div></div>
      <div id="conn-calendar-row" class="mb-3">
        <label class="form-label">Calendar CalDAV URL</label>
        <div class="input-group"><input id="conn-calendar-url" class="form-control conn-copy" readonly><button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button></div>
      </div>
      <div id="conn-cal-list" class="mb-3">
        <label class="form-label">Calendars</label>
        <div id="conn-cal-rows"></div>
      </div>
      <div class="mb-3"><label class="form-label">Password</label>
        <p class="mb-1 text-body-secondary small">Use an <strong>app password</strong>, not your login password — DAV clients authenticate with Basic auth against app passwords only.</p>
        <a href="/credentials" class="btn btn-outline-primary btn-sm">Manage app passwords</a></div>
    </div>
  </div></div>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/bs-calendar.min.js"></script>
<script src="/assets/js/summernote-bs5.min.js"></script>
<script src="/assets/js/dialogs.js"></script>
<script src="/assets/js/app.js?v=16"></script>
</body></html>"#
);

const RULES_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Rules</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_open!(),
    r#"<div class="container p-3">
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
          <option value="event_updated">event_updated</option>
          <option value="event_deleted">event_deleted</option>
          <option value="task_created">task_created</option>
          <option value="task_updated">task_updated</option>
          <option value="task_deleted">task_deleted</option>
          <option value="task_completed">task_completed</option>
          <option value="task_due">task_due</option>
          <option value="journal_created">journal_created</option>
          <option value="journal_updated">journal_updated</option>
          <option value="journal_deleted">journal_deleted</option>
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
          <option value="webhook">Webhook (delivers to the tenant's webhooks)</option>
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
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/rules.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

const CATEGORIES_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Categories</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_gated!(),
    r#"<div class="container p-3">
  <div class="d-flex align-items-center gap-2 mb-1">
    <h1 class="h4 mb-0">Categories</h1>
    <select class="form-select form-select-sm w-auto" id="cat-calendar-select">
      <option value="">All calendars</option>
    </select>
  </div>
  <p id="cat-scope-note" class="text-body-secondary small"></p>
  <form id="cat-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col"><label class="form-label" for="cat-name">Name</label>
        <input class="form-control" id="cat-name" required></div>
      <div class="col"><label class="form-label" for="cat-slug">Slug</label>
        <input class="form-control" id="cat-slug" pattern="[a-z0-9][a-z0-9-]*" required></div>
      <div class="col-auto"><label class="form-label" for="cat-color">Color</label>
        <select class="form-select" id="cat-color"></select></div>
      <div class="col-auto form-check mb-2">
        <input class="form-check-input" type="checkbox" id="cat-global">
        <label class="form-check-label" for="cat-global">Apply to all calendars</label></div>
      <div class="col-auto d-flex align-items-end">
        <button class="btn btn-primary" type="submit">Add category</button></div>
    </div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Preview</th><th>Name</th><th>Slug</th><th>Scope</th><th></th></tr></thead>
    <tbody id="cat-rows"></tbody>
  </table>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/dialogs.js"></script>
<script src="/assets/js/categories.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

const CONTACTS_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Contacts</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_gated!(),
    r#"<div class="container-fluid p-3">
  <div class="row">
    <div class="col-md-3 mb-3">
      <div class="d-flex align-items-center justify-content-between mb-2">
        <h1 class="h5 mb-0">Address books</h1>
        <button id="ab-new" class="btn btn-sm btn-outline-primary" type="button" title="New address book"><i class="bi bi-plus-lg"></i></button>
      </div>
      <div class="list-group" id="ab-list"></div>
      <p class="text-body-secondary small mt-2">Personal books sync via CardDAV at <code>/contacts/</code>. The directory book lists every user in your tenant and is read-only.</p>
    </div>
    <div class="col-md-9">
      <div class="d-flex align-items-center gap-2 mb-2">
        <h1 class="h5 mb-0" id="ab-current-name">Contacts</h1>
        <input class="form-control form-control-sm w-auto ms-auto" id="ct-search" placeholder="Search">
      </div>
      <form id="ct-form" class="card p-3 mb-3">
        <div class="row g-2 align-items-end">
          <div class="col"><label class="form-label" for="ct-name">Name</label>
            <input class="form-control" id="ct-name" required></div>
          <div class="col"><label class="form-label" for="ct-org">Organization</label>
            <input class="form-control" id="ct-org"></div>
          <div class="col"><label class="form-label" for="ct-email">Email</label>
            <input class="form-control" id="ct-email" type="email"></div>
          <div class="col"><label class="form-label" for="ct-tel">Phone</label>
            <input class="form-control" id="ct-tel" type="tel"></div>
          <div class="col-auto form-check mb-2">
            <input class="form-check-input" type="checkbox" id="ct-mobile" checked>
            <label class="form-check-label" for="ct-mobile">Mobile</label></div>
          <div class="col-auto d-flex align-items-end">
            <button class="btn btn-primary" type="submit">Add contact</button></div>
        </div>
      </form>
      <table class="table table-sm bg-body">
        <thead><tr><th>Name</th><th>Org</th><th>Email</th><th>Phone</th><th></th></tr></thead>
        <tbody id="ct-rows"></tbody>
      </table>
    </div>
  </div>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/dialogs.js"></script>
<script src="/assets/js/contacts.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

const ADMIN_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Admin</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_open!(),
    r#"<div class="container p-3">
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
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/admin.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

const PROVIDERS_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Providers</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_open!(),
    r#"<div class="container p-3">
  <h1 class="h4 mb-3">Notification providers</h1>
  <form id="provider-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col-auto"><label class="form-label" for="provider-kind">Kind</label>
        <select class="form-select" id="provider-kind">
          <option value="postmark">Postmark (email)</option>
          <option value="smtp">SMTP (email)</option>
          <option value="twilio">Twilio (SMS)</option>
          <option value="webpush">Web Push</option>
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
<div class="modal fade" id="provider-edit-modal" aria-hidden="true">
  <div class="modal-dialog"><form id="provider-edit-form" class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Edit provider</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <input type="hidden" id="pe-id">
      <div class="row g-2">
        <div class="col-auto"><label class="form-label" for="pe-kind">Kind</label>
          <input class="form-control" id="pe-kind" readonly></div>
        <div class="col"><label class="form-label" for="pe-name">Name</label>
          <input class="form-control" id="pe-name" required></div>
      </div>
      <div id="pe-fields" class="row g-2 mt-1"></div>
      <div class="form-check mt-2"><input class="form-check-input" type="checkbox" id="pe-enabled">
        <label class="form-check-label" for="pe-enabled">Enabled</label></div>
    </div>
    <div class="modal-footer">
      <button class="btn btn-primary" type="submit">Save</button>
      <button class="btn btn-secondary" type="button" data-bs-dismiss="modal">Cancel</button>
    </div>
  </form></div>
</div>
<div class="modal fade" id="provider-test-modal" aria-hidden="true">
  <div class="modal-dialog"><form id="provider-test-form" class="modal-content">
    <div class="modal-header"><h2 class="modal-title h5">Send test message</h2>
      <button type="button" class="btn-close" data-bs-dismiss="modal"></button></div>
    <div class="modal-body">
      <input type="hidden" id="pt-id">
      <input type="hidden" id="pt-kind">
      <p id="pt-provider-label" class="text-body-secondary small"></p>
      <div class="mb-2"><label class="form-label" for="pt-to">To</label>
        <input class="form-control" id="pt-to" required></div>
      <div class="mb-2" id="pt-subject-row"><label class="form-label" for="pt-subject">Subject</label>
        <input class="form-control" id="pt-subject"></div>
      <div class="mb-2"><label class="form-label" for="pt-body">Message</label>
        <textarea class="form-control" id="pt-body" rows="3"></textarea></div>
      <div id="pt-result" class="small"></div>
    </div>
    <div class="modal-footer">
      <button class="btn btn-primary" type="submit">Send test</button>
      <button class="btn btn-secondary" type="button" data-bs-dismiss="modal">Close</button>
    </div>
  </form></div>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/providers.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

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

async fn categories_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        CATEGORIES_PAGE,
    )
}

async fn contacts_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        CONTACTS_PAGE,
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

const CREDENTIALS_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en" data-bs-theme="light">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — Credentials</title>
<link rel="stylesheet" href="/assets/css/bootstrap.min.css">
<link rel="stylesheet" href="/assets/css/bootstrap-icons.css">
<link rel="stylesheet" href="/assets/css/app.css">
<link rel="icon" type="image/png" sizes="16x16" href="/assets/img/favicon-16x16.png">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<link rel="apple-touch-icon" href="/assets/img/apple-touch-icon.png">
<script src="/assets/js/theme.js"></script>
</head>
<body class="bg-body-tertiary">
"#,
    subpage_header!(),
    nav_menu_open!(),
    r#"<div class="container p-3">
  <div id="secret-banner" class="alert alert-warning d-none" role="alert">
    <div class="fw-bold mb-1">Copy it now — it will not be shown again.</div>
    <code id="secret-value"></code>
  </div>

  <h1 class="h4 mb-3">API tokens</h1>
  <p class="text-body-secondary small">
    Bearer tokens for third-party apps (<code>Authorization: Bearer</code>).
    Scopes: empty or <code>full</code> = everything, <code>write</code> implies read,
    <code>read</code> = GET only.
  </p>
  <form id="token-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col"><label class="form-label" for="token-name">Name</label>
        <input class="form-control" id="token-name" required></div>
      <div class="col-auto form-check mb-2">
        <input class="form-check-input" type="checkbox" id="token-readonly">
        <label class="form-check-label" for="token-readonly">Read-only</label></div>
      <div class="col-auto"><label class="form-label" for="token-expires">Expires (optional)</label>
        <input class="form-control" id="token-expires" type="date"></div>
      <div class="col-auto"><button class="btn btn-primary" type="submit">Create token</button></div>
    </div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Name</th><th>Scopes</th><th>Created</th><th>Expires</th><th></th></tr></thead>
    <tbody id="token-rows"></tbody>
  </table>

  <h1 class="h4 mb-3 mt-4">App passwords</h1>
  <p class="text-body-secondary small">
    For CalDAV clients that only speak Basic auth. Username is your normal account
    username; the password is generated here.
  </p>
  <form id="app-password-form" class="card p-3 mb-4">
    <div class="row g-2 align-items-end">
      <div class="col"><label class="form-label" for="ap-name">Name</label>
        <input class="form-control" id="ap-name" required></div>
      <div class="col-auto"><button class="btn btn-primary" type="submit">Create app password</button></div>
    </div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Name</th><th>Created</th><th>Last used</th><th>Expires</th><th></th></tr></thead>
    <tbody id="ap-rows"></tbody>
  </table>

  <h1 class="h4 mb-3 mt-4">Two-factor authentication</h1>
  <p id="totp-status" class="text-body-secondary small">Loading…</p>
  <div class="d-flex gap-2">
    <button id="totp-setup-btn" class="btn btn-outline-primary btn-sm" type="button" hidden>Set up 2FA</button>
    <button id="totp-disable-btn" class="btn btn-outline-danger btn-sm" type="button" hidden>Disable 2FA</button>
  </div>
  <div id="totp-setup-panel" class="card p-3 mt-2 d-none">
    <p class="small text-body-secondary">Add this secret to your authenticator app (it is what the
      QR code would encode), then enter a code from it to confirm.</p>
    <div class="mb-2"><label class="form-label" for="totp-secret">Secret</label>
      <input class="form-control font-monospace" id="totp-secret" readonly></div>
    <div class="mb-2"><label class="form-label" for="totp-url">otpauth URL</label>
      <input class="form-control font-monospace" id="totp-url" readonly></div>
    <div class="row g-2 align-items-end">
      <div class="col-auto"><label class="form-label" for="totp-code">Authenticator code</label>
        <input class="form-control" id="totp-code" inputmode="numeric" autocomplete="one-time-code"></div>
      <div class="col-auto"><button id="totp-verify-btn" class="btn btn-primary" type="button">Verify &amp; enable</button></div>
    </div>
  </div>
  <div id="totp-recovery-panel" class="alert alert-warning mt-2 d-none" role="alert">
    <div class="fw-bold mb-1">Save these recovery codes now — they are shown only once.</div>
    <textarea id="totp-recovery-codes" class="form-control font-monospace" rows="8" readonly></textarea>
  </div>

  <h1 class="h4 mb-3 mt-4">Passkeys</h1>
  <p id="pk-note" class="text-body-secondary small d-none"></p>
  <form id="passkey-form" class="row g-2 align-items-end mb-2">
    <div class="col"><label class="form-label" for="pk-name">Name</label>
      <input class="form-control" id="pk-name" required></div>
    <div class="col-auto"><button class="btn btn-primary" type="submit">Add passkey</button></div>
  </form>
  <table class="table table-sm bg-body">
    <thead><tr><th>Name</th><th>Created</th><th>Last used</th><th></th></tr></thead>
    <tbody id="pk-rows"></tbody>
  </table>
</div>
<script src="/assets/js/jquery.min.js"></script>
<script src="/assets/js/jquery-migrate.min.js"></script>
<script src="/assets/js/api.js"></script>
<script src="/assets/js/bootstrap.bundle.min.js"></script>
<script src="/assets/js/dialogs.js"></script>
<script src="/assets/js/credentials.js"></script>
"#,
    footer_html!(),
    r#"</body></html>"#
);

const SWAGGER_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CalStack — API docs</title>
<link rel="stylesheet" href="/assets/css/swagger-ui.css">
<link rel="icon" type="image/png" sizes="32x32" href="/assets/img/favicon-32x32.png">
<style>body{margin:0}.topbar{display:none}</style>
</head>
<body>
<div id="swagger-ui"></div>
<script src="/assets/js/swagger-ui-bundle.js"></script>
<script>
SwaggerUIBundle({
  url: '/api/openapi.json',
  dom_id: '#swagger-ui',
  deepLinking: true,
  tryItOutEnabled: true,
});
</script>
</body></html>"#;

async fn swagger_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        SWAGGER_PAGE,
    )
}

async fn credentials_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        CREDENTIALS_PAGE,
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
        .route("/categories", axum::routing::get(categories_page))
        .route("/contacts-ui", axum::routing::get(contacts_page))
        .route("/admin", axum::routing::get(admin_page))
        .route("/providers", axum::routing::get(providers_page))
        .route("/credentials", axum::routing::get(credentials_page))
        .route("/docs", axum::routing::get(swagger_page))
        .route("/assets/{*path}", axum::routing::get(assets))
        // Service workers must be served at their intended scope root.
        .route(
            "/sw.js",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    asset!("js/sw.js"),
                )
            }),
        )
}
