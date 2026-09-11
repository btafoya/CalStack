/* Rules page: list/create/delete/toggle rules against /api/rules. */
(function () {
  'use strict';

  function renderRules(rules) {
    var $rows = $('#rules-rows').empty();
    rules.forEach(function (rule) {
      var actions = (rule.actions || []).map(function (a) { return a.type; }).join(', ') || '(none)';
      var $enabled = $('<input type="checkbox" class="form-check-input">').prop('checked', rule.enabled);
      $enabled.on('change', function () {
        api('PATCH', '/api/rules/' + rule.id, { enabled: $enabled.is(':checked') });
      });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () { api('DELETE', '/api/rules/' + rule.id).done(loadRules); });
      $('<tr>')
        .append($('<td>').text(rule.name))
        .append($('<td>').text(rule.trigger_type))
        .append($('<td>').text(actions))
        .append($('<td>').append($enabled))
        .append($('<td>').append($del))
        .appendTo($rows);
    });
  }

  function loadRules() {
    return api('GET', '/api/rules').done(renderRules);
  }

  $(function () {
    loadRules();
    $('#rule-form').on('submit', function (ev) {
      ev.preventDefault();
      api('POST', '/api/rules', {
        name: $('#rule-name').val(),
        trigger_type: $('#rule-trigger').val(),
        enabled: $('#rule-enabled').is(':checked'),
        actions: [{ type: 'create_notification', title: $('#rule-title').val(), body: $('#rule-body').val() }],
      }).done(function () {
        $('#rule-form')[0].reset();
        loadRules();
      }).fail(function (xhr) {
        alert((xhr.responseJSON && xhr.responseJSON.error) || 'Create failed');
      });
    });
  });
})();
