/* Admin page: list/create users, toggle is_admin/disabled via /api/admin/users. */
(function () {
  'use strict';

  function renderUsers(users) {
    var $rows = $('#user-rows').empty();
    users.forEach(function (u) {
      var $admin = $('<input type="checkbox" class="form-check-input">').prop('checked', u.is_admin);
      var $disabled = $('<input type="checkbox" class="form-check-input">').prop('checked', u.disabled);
      $admin.on('change', function () {
        api('PATCH', '/api/admin/users/' + u.id, { is_admin: $admin.is(':checked') })
          .fail(function () { $admin.prop('checked', !$admin.is(':checked')); });
      });
      $disabled.on('change', function () {
        api('PATCH', '/api/admin/users/' + u.id, { disabled: $disabled.is(':checked') })
          .fail(function () { $disabled.prop('checked', !$disabled.is(':checked')); });
      });
      $('<tr>')
        .append($('<td>').text(u.username))
        .append($('<td>').text(u.email))
        .append($('<td>').append($admin))
        .append($('<td>').append($disabled))
        .appendTo($rows);
    });
  }

  function loadUsers() {
    return api('GET', '/api/admin/users').done(renderUsers);
  }

  function renderAudit(rows) {
    var $rows = $('#audit-rows').empty();
    rows.forEach(function (r) {
      $('<tr>')
        .append($('<td>').text(r.created_at))
        .append($('<td>').text(r.action))
        .append($('<td>').text(r.object_type))
        .append($('<td>').text(r.change_summary || ''))
        .appendTo($rows);
    });
  }

  function loadAudit() {
    return api('GET', '/api/audit').done(renderAudit);
  }

  $(function () {
    loadUsers();
    loadAudit();
    $('#user-form').on('submit', function (ev) {
      ev.preventDefault();
      api('POST', '/api/admin/users', {
        username: $('#u-username').val(),
        email: $('#u-email').val(),
        password: $('#u-password').val(),
        is_admin: $('#u-is-admin').is(':checked'),
      }).done(function () {
        $('#user-form')[0].reset();
        loadUsers();
      }).fail(function (xhr) {
        alert((xhr.responseJSON && xhr.responseJSON.error) || 'Create failed');
      });
    });

    $('#account-btn').on('click', function () { window.location.href = '/'; });
    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
    });
  });
})();
