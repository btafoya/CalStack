/* Rules page: list/create/delete/toggle rules against /api/rules. */
(function () {
  'use strict';

  var calendarId = new URLSearchParams(window.location.search).get('calendar_id');

  function updateScopeUi() {
    var url = new URL(window.location);
    if (calendarId) { url.searchParams.set('calendar_id', calendarId); } else { url.searchParams.delete('calendar_id'); }
    window.history.replaceState(null, '', url);
    if (calendarId) {
      $('#rules-scope-note').text('Showing rules for this calendar, plus any that apply to all calendars.');
      $('#rule-global-row').show();
    } else {
      $('#rules-scope-note').text('No calendar selected: showing rules that apply to all calendars. Pick one above to add a calendar-specific rule.');
      $('#rule-global-row').hide();
    }
  }

  function loadCalendarOptions() {
    return api('GET', '/api/calendars').done(function (list) {
      var $select = $('#rule-calendar-select');
      list.forEach(function (cal) {
        $('<option>').val(cal.id).text(cal.name).appendTo($select);
      });
      if (calendarId) { $select.val(calendarId); }
    });
  }

  function renderRules(rules) {
    var $rows = $('#rules-rows').empty();
    rules.forEach(function (rule) {
      var actions = (rule.actions || []).map(function (a) { return a.type; }).join(', ') || '(none)';
      var scope = rule.calendar_id ? 'This calendar' : 'All calendars';
      var $enabled = $('<input type="checkbox" class="form-check-input">').prop('checked', rule.enabled);
      $enabled.on('change', function () {
        api('PATCH', '/api/rules/' + rule.id, { enabled: $enabled.is(':checked') });
      });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () { api('DELETE', '/api/rules/' + rule.id).done(loadRules); });
      $('<tr>')
        .append($('<td>').text(rule.name))
        .append($('<td>').text(rule.trigger_type))
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

  $(function () {
    updateScopeUi();
    loadCalendarOptions();
    loadRules();
    $('#rule-calendar-select').on('change', function () {
      calendarId = $(this).val() || null;
      updateScopeUi();
      loadRules();
    });
    $('#rule-action-type').on('change', function () {
      var isSms = $(this).val() === 'sms';
      $('#rule-title-row').prop('hidden', isSms);
      $('#rule-title').prop('required', !isSms);
      $('#rule-to-row').prop('hidden', !isSms);
    });
    $('#rule-form').on('submit', function (ev) {
      ev.preventDefault();
      var global = !calendarId || $('#rule-global').is(':checked');
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
        $('#rule-title-row').prop('hidden', false);
        $('#rule-to-row').prop('hidden', true);
        loadRules();
      });
    });

    $('#account-btn').on('click', function () { window.location.href = '/'; });
    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
    });
  });
})();
