/* Shared jQuery ajax helper for admin/rules pages. */
function api(method, url, data) {
  return $.ajax({
    method: method,
    url: url,
    data: data ? JSON.stringify(data) : null,
    contentType: 'application/json',
    headers: method !== 'GET' ? { 'X-CSRF-Token': sessionStorage.getItem('csrf') || '' } : {},
  }).fail(function (xhr) {
    if (xhr.status === 401) { window.location.href = '/login'; return; }
    window.alert((xhr.responseJSON && xhr.responseJSON.error) || 'Request failed');
  });
}
