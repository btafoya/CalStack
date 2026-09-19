/* Notification providers page: configure Postmark/SMTP/Twilio credentials
   used to send iTIP mail and (once wired to a sender) rule/reminder SMS. */
(function () {
  'use strict';

  var FIELD_SPECS = {
    postmark: [
      { key: 'from', label: 'From address', type: 'email' },
      { key: 'token', label: 'Server token', type: 'text' },
      { key: 'message_stream', label: 'Message stream', type: 'text', placeholder: 'outbound', optional: true },
    ],
    smtp: [
      { key: 'server', label: 'Host:port', type: 'text' },
      { key: 'from', label: 'From address', type: 'email' },
      { key: 'token', label: 'Password (optional)', type: 'password' },
    ],
    twilio: [
      { key: 'account_sid', label: 'Account SID', type: 'text' },
      { key: 'auth_token', label: 'Auth token', type: 'password' },
      { key: 'from', label: 'From phone number', type: 'text' },
    ],
  };

  // Fields are built into two containers (create form + edit modal); the
  // prefix keeps their element ids distinct.
  function buildFields(kind, $container, values, prefix) {
    var $fields = $container.empty();
    FIELD_SPECS[kind].forEach(function (f) {
      $('<div class="col">').append(
        $('<label class="form-label">').attr('for', prefix + f.key).text(f.label),
        $('<input class="form-control" required>').attr({
          id: prefix + f.key,
          type: f.type,
          placeholder: f.placeholder || '',
        }).prop('required', !f.optional).val(values ? (values[f.key] || '') : '')
      ).appendTo($fields);
    });
  }

  function readFields(kind, prefix) {
    var config = {};
    FIELD_SPECS[kind].forEach(function (f) {
      var val = $('#' + prefix + f.key).val();
      // Omit empty optional fields so backend defaults apply (e.g. Postmark
      // message_stream falls back to "outbound").
      if (f.optional && !val) { return; }
      config[f.key] = val;
    });
    return config;
  }

  function renderFields() {
    buildFields($('#provider-kind').val(), $('#provider-fields'), null, 'cf-');
  }

  // ============ edit modal ============
  var editKind = null;

  function openEdit(id) {
    api('GET', '/api/notification-providers/' + id).done(function (p) {
      editKind = p.kind;
      $('#pe-id').val(p.id);
      $('#pe-kind').val(p.kind);
      $('#pe-name').val(p.name);
      $('#pe-enabled').prop('checked', p.enabled);
      buildFields(p.kind, $('#pe-fields'), p.config, 'ef-');
      $('#provider-edit-modal').modal('show');
    });
  }

  function renderProviders(providers) {
    var $rows = $('#provider-rows').empty();
    providers.forEach(function (p) {
      var $edit = $('<button class="btn btn-outline-secondary btn-sm" type="button" title="Edit"><i class="bi bi-pencil"></i></button>');
      $edit.on('click', function () { openEdit(p.id); });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () { api('DELETE', '/api/notification-providers/' + p.id).done(loadProviders); });
      $('<tr>')
        .append($('<td>').text(p.kind))
        .append($('<td>').text(p.name))
        .append($('<td>').text(p.enabled ? 'Yes' : 'No'))
        .append($('<td class="text-end">').append($('<span class="btn-group btn-group-sm">').append($edit, $del)))
        .appendTo($rows);
    });
  }

  function loadProviders() {
    return api('GET', '/api/notification-providers').done(renderProviders);
  }

  $(function () {
    renderFields();
    $('#provider-kind').on('change', renderFields);
    loadProviders();
    $('#provider-form').on('submit', function (ev) {
      ev.preventDefault();
      var kind = $('#provider-kind').val();
      var config = readFields(kind, 'cf-');
      if (kind === 'postmark') { config.server = 'postmark'; } // unused by Postmark send; backend requires the field
      api('POST', '/api/notification-providers', {
        kind: kind,
        name: $('#provider-name').val(),
        config: config,
      }).done(function () {
        $('#provider-form')[0].reset();
        renderFields();
        loadProviders();
      });
    });

    $('#provider-edit-form').on('submit', function (ev) {
      ev.preventDefault();
      var config = readFields(editKind, 'ef-');
      if (editKind === 'postmark') { config.server = 'postmark'; }
      api('PATCH', '/api/notification-providers/' + $('#pe-id').val(), {
        name: $('#pe-name').val(),
        enabled: $('#pe-enabled').prop('checked'),
        config: config,
      }).done(function () {
        $('#provider-edit-modal').modal('hide');
        loadProviders();
      });
    });

    $('#account-btn').on('click', function () { window.location.href = '/'; });
    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
    });
  });
})();
