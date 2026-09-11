/* Notification providers page: configure Postmark/SMTP/Twilio credentials
   used to send iTIP mail and (once wired to a sender) rule/reminder SMS. */
(function () {
  'use strict';

  var FIELD_SPECS = {
    postmark: [
      { key: 'from', label: 'From address', type: 'email' },
      { key: 'token', label: 'Server token', type: 'text' },
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

  function renderFields() {
    var kind = $('#provider-kind').val();
    var $fields = $('#provider-fields').empty();
    FIELD_SPECS[kind].forEach(function (f) {
      $('<div class="col">').append(
        $('<label class="form-label">').attr('for', 'pf-' + f.key).text(f.label),
        $('<input class="form-control" required>').attr({ id: 'pf-' + f.key, type: f.type })
      ).appendTo($fields);
    });
  }

  function renderProviders(providers) {
    var $rows = $('#provider-rows').empty();
    providers.forEach(function (p) {
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () { api('DELETE', '/api/notification-providers/' + p.id).done(loadProviders); });
      $('<tr>')
        .append($('<td>').text(p.kind))
        .append($('<td>').text(p.name))
        .append($('<td>').text(p.enabled ? 'Yes' : 'No'))
        .append($('<td>').append($del))
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
      var config = {};
      FIELD_SPECS[kind].forEach(function (f) { config[f.key] = $('#pf-' + f.key).val(); });
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
  });
})();
