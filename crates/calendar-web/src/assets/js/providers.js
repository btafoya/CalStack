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

  // ============ test-send modal ============
  var TEST_TO_PLACEHOLDER = { postmark: 'you@example.com', smtp: 'you@example.com', twilio: '+15551234567' };
  var TEST_IS_SMS = { postmark: false, smtp: false, twilio: true };

  function openTest(p) {
    $('#pt-id').val(p.id);
    $('#pt-kind').val(p.kind);
    $('#pt-provider-label').text(p.kind + ' · ' + p.name);
    $('#pt-to').val('').attr('placeholder', TEST_TO_PLACEHOLDER[p.kind] || '');
    $('#pt-subject').val('');
    $('#pt-body').val('');
    $('#pt-result').text('').removeAttr('data-result');
    // SMS has no subject line; hide the field for Twilio.
    $('#pt-subject-row').prop('hidden', TEST_IS_SMS[p.kind]);
    $('#provider-test-modal').modal('show');
  }

  function renderProviders(providers) {
    var $rows = $('#provider-rows').empty();
    providers.forEach(function (p) {
      var $edit = $('<button class="btn btn-outline-secondary btn-sm" type="button" title="Edit"><i class="bi bi-pencil"></i></button>');
      $edit.on('click', function () { openEdit(p.id); });
      var $test = $('<button class="btn btn-outline-secondary btn-sm" type="button" title="Send test message"><i class="bi bi-send"></i></button>');
      $test.on('click', function () { openTest(p); });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () { api('DELETE', '/api/notification-providers/' + p.id).done(loadProviders); });
      $('<tr>')
        .append($('<td>').text(p.kind))
        .append($('<td>').text(p.name))
        .append($('<td>').text(p.enabled ? 'Yes' : 'No'))
        .append($('<td class="text-end">').append($('<span class="btn-group btn-group-sm">').append($test, $edit, $del)))
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

    $('#provider-test-form').on('submit', function (ev) {
      ev.preventDefault();
      var payload = {
        to: $('#pt-to').val(),
        subject: $('#pt-subject').val() || null,
        body: $('#pt-body').val() || null,
      };
      var $result = $('#pt-result').text('Sending…');
      api('POST', '/api/notification-providers/' + $('#pt-id').val() + '/test', payload)
        .done(function (resp) {
          if (resp.ok) {
            $result.removeClass('text-danger').addClass('text-success').text('Sent.');
          } else {
            $result.removeClass('text-success').addClass('text-danger')
              .text('Send failed: ' + (resp.error || 'unknown error'));
          }
        })
        .fail(function () {
          $result.removeClass('text-success').addClass('text-danger').text('Send failed.');
        });
    });

    $('#account-btn').on('click', function () { window.location.href = '/'; });
    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
    });
  });
})();
