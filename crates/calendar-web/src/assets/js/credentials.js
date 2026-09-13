/* Credentials page: API tokens + CalDAV app passwords. */
$(function () {
  function showSecret(value) {
    $('#secret-value').text(value);
    $('#secret-banner').removeClass('d-none');
    window.scrollTo(0, 0);
  }

  function esc(text) {
    return $('<span>').text(text).html();
  }

  function fmtDate(value) {
    return value ? value.replace('T', ' ').slice(0, 16) : '—';
  }

  function revokeBtn(kind, id) {
    return '<button class="btn btn-outline-danger btn-sm" data-kind="' + kind + '" data-id="' + id + '">Revoke</button>';
  }

  function renderTokens(list) {
    $('#token-rows').html(list.map(function (t) {
      var scopes = (t.scopes && t.scopes.length) ? t.scopes.join(', ') : 'full';
      return '<tr><td>' + esc(t.name) + '</td><td>' + esc(scopes) + '</td><td>'
        + esc(fmtDate(t.created_at)) + '</td><td>' + esc(fmtDate(t.expires_at)) + '</td><td>'
        + revokeBtn('token', t.id) + '</td></tr>';
    }).join('') || '<tr><td colspan="5" class="text-body-secondary">No tokens.</td></tr>');
  }

  function renderPasswords(list) {
    $('#ap-rows').html(list.map(function (p) {
      return '<tr><td>' + esc(p.name) + '</td><td>' + esc(fmtDate(p.created_at))
        + '</td><td>' + esc(fmtDate(p.last_used_at)) + '</td><td>' + esc(fmtDate(p.expires_at))
        + '</td><td>' + revokeBtn('app-password', p.id) + '</td></tr>';
    }).join('') || '<tr><td colspan="5" class="text-body-secondary">No app passwords.</td></tr>');
  }

  function load() {
    api('GET', '/api/auth/tokens').done(renderTokens);
    api('GET', '/api/auth/app-passwords').done(renderPasswords);
  }

  $('#token-form').on('submit', function (ev) {
    ev.preventDefault();
    var expires = $('#token-expires').val();
    api('POST', '/api/auth/tokens', {
      name: $('#token-name').val(),
      scopes: $('#token-readonly').prop('checked') ? ['read'] : [],
      expires_at: expires ? new Date(expires + 'T23:59:59').toISOString() : null,
    }).done(function (resp) {
      $('#token-name').val('');
      $('#token-readonly').prop('checked', false);
      $('#token-expires').val('');
      showSecret(resp.secret);
      load();
    });
  });

  $('#app-password-form').on('submit', function (ev) {
    ev.preventDefault();
    api('POST', '/api/auth/app-passwords', { name: $('#ap-name').val() }).done(function (resp) {
      $('#ap-name').val('');
      showSecret(resp.password);
      load();
    });
  });

  $(document).on('click', '[data-kind][data-id]', function () {
    var kind = $(this).data('kind');
    var id = $(this).data('id');
    confirmDialog('Revoke this credential? Apps using it will stop working.').done(function () {
      api('DELETE', '/api/auth/' + kind + 's/' + id).done(load);
    });
  });

  $('#account-btn').on('click', function () { window.location.href = '/'; });
  $('#logout-btn').on('click', function () {
    api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
  });

  load();
});