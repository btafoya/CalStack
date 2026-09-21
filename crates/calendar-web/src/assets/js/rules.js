/* Rules pane: list/create/delete/toggle rules against /api/rules. On the
 * index page it lives in a tab; the sidebar's selected calendar sets the
 * scope (app.js calls RulesPane.load on tab show / calendar switch). */
(function () {
  'use strict';

  var calendarId = null;

  var TRIGGER_TEXT = {
    event_created: 'Event created', event_updated: 'Event updated', event_deleted: 'Event deleted',
    task_created: 'Task created', task_updated: 'Task updated', task_deleted: 'Task deleted',
    task_completed: 'Task completed', task_due: 'Task due',
    journal_created: 'Journal created', journal_updated: 'Journal updated', journal_deleted: 'Journal deleted',
  };
  var ACTION_TEXT = { create_notification: 'In-app notification', sms: 'SMS', webhook: 'Webhook' };

  function renderRules(rules) {
    var $rows = $('#rules-rows').empty();
    rules.forEach(function (rule) {
      var actions = (rule.actions || []).map(function (a) { return ACTION_TEXT[a.type] || a.type; }).join(', ') || '(none)';
      var scope = rule.calendar_id ? 'This calendar' : 'All calendars';
      var $enabled = $('<input type="checkbox" class="form-check-input">').prop('checked', rule.enabled);
      $enabled.on('change', function () {
        api('PATCH', '/api/rules/' + rule.id, { enabled: $enabled.is(':checked') });
      });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () {
        confirmDialog('Delete rule "' + rule.name + '"?').done(function () {
          api('DELETE', '/api/rules/' + rule.id).done(function () {
            toast('Rule deleted.');
            loadRules();
          });
        });
      });
      $('<tr>')
        .append($('<td>').text(rule.name))
        .append($('<td>').text(TRIGGER_TEXT[rule.trigger_type] || rule.trigger_type))
        .append($('<td>').text(scope))
        .append($('<td>').text(actions))
        .append($('<td>').append($enabled))
        .append($('<td>').append($del))
        .appendTo($rows);
    });
  }

  function loadRules() {
    var url = '/api/rules' + (calendarId ? '?calendar_id=' + calendarId : '');
    return api('GET', url).done(renderRules);
  }

  window.RulesPane = {
    _cal: null,
    load: function (cal) {
      if (this._cal === cal.id) { return; }
      this._cal = cal.id;
      calendarId = cal.readOnly ? null : cal.id;
      $('#rules-scope-note').text(calendarId
        ? 'Showing rules for this calendar, plus any that apply to all calendars.'
        : 'Select one of your own calendars to add a calendar-specific rule.');
      loadRules();
    },
  };

  $(function () {
    $('#rule-action-type').on('change', function () {
      var isSms = $(this).val() === 'sms';
      $('#rule-title-row').prop('hidden', isSms);
      $('#rule-title').prop('required', !isSms);
      $('#rule-to-row').prop('hidden', !isSms);
    });
    $('#rule-form').on('submit', function (ev) {
      ev.preventDefault();
      var global = $('#rule-global').is(':checked');
      var type = $('#rule-action-type').val();
      var action = { type: type, body: $('#rule-body').val() };
      if (type === 'sms') { action.to = $('#rule-to').val(); } else { action.title = $('#rule-title').val(); }
      api('POST', '/api/rules', {
        name: $('#rule-name').val(),
        trigger_type: $('#rule-trigger').val(),
        enabled: $('#rule-enabled').is(':checked'),
        calendar_id: global ? null : calendarId,
        actions: [action],
      }).done(function () {
        $('#rule-form')[0].reset();
        $('#rule-enabled').prop('checked', true);
        $('#rule-title-row').prop('hidden', false);
        $('#rule-to-row').prop('hidden', true);
        toast('Rule added.');
        loadRules();
      });
    });
  });
})();