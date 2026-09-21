/* Shared jQuery ajax helper for admin/rules/categories/contacts/credentials pages. */
// opts.silent: skip the error dialog so the caller can show its own failure UI
// (used for WebAuthn endpoints that 500 when the server has no RP ID).
function api(method, url, data, opts) {
  // ponytail: see app.js's api() for why this disables document.activeElement
  // instead of threading a button reference through every caller.
  var $btn = method !== 'GET' ? $(document.activeElement).filter('button, input[type="submit"]') : $();
  $btn.prop('disabled', true);
  return $.ajax({
    method: method,
    url: url,
    data: data ? JSON.stringify(data) : null,
    contentType: 'application/json',
    headers: method !== 'GET' ? { 'X-CSRF-Token': sessionStorage.getItem('csrf') || '' } : {},
  }).always(function () {
    $btn.prop('disabled', false);
  }).fail(function (xhr) {
    if (xhr.status === 401) { window.location.href = '/login'; return; }
    if (!(opts && opts.silent)) {
      errorDialog((xhr.responseJSON && xhr.responseJSON.error) || 'Request failed');
    }
  });
}

// Mark the current page in the top nav (lives here because api.js is the one
// script every subpage loads; index does its own in app.js).
$(function () {
  $('.navbar-nav .nav-link').filter(function () {
    return this.getAttribute('href') === location.pathname;
  }).addClass('active');
});