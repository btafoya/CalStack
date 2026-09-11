/* Calendar web app: jQuery 4 against /api. Progressive enhancement shell. */
(function () {
  'use strict';

  var state = { calendars: [], currentCalendar: null, csrf: sessionStorage.getItem('csrf') || '' };

  function api(method, url, data) {
    return $.ajax({
      method: method,
      url: url,
      data: data ? JSON.stringify(data) : null,
      contentType: 'application/json',
      headers: method !== 'GET' ? { 'X-CSRF-Token': state.csrf } : {},
    }).fail(function (xhr) {
      if (xhr.status === 401) { window.location.href = '/login'; return; }
      window.alert((xhr.responseJSON && xhr.responseJSON.error) || 'Request failed');
    });
  }

  function fmt(value) {
    return value ? value.replace(/[: ]/g, function (c) {
      return c === ' ' ? 'T' : '-'; // local input formatting below
    }) : value;
  }

  // ============ calendars ============
  function updateRulesLink() {
    var cal = state.currentCalendar;
    $('#rules-link').attr('href', cal ? '/rules?calendar_id=' + cal.id : '/rules');
  }

  function selectCalendar(cal) {
    state.currentCalendar = cal;
    $('#cal-list li').removeClass('active');
    $('#cal-list li[data-id="' + cal.id + '"]').addClass('active');
    updateRulesLink();
    $('#calendar').bsCalendar('refresh');
  }

  $('#add-cal-btn').on('click', function () {
    var name = window.prompt('New calendar name:');
    if (!name) { return; }
    var slug = name.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
    if (!slug) { window.alert('Enter a valid calendar name.'); return; }
    api('POST', '/api/calendars', { slug: slug, name: name }).done(loadCalendars);
  });

  function loadCalendars() {
    return api('GET', '/api/calendars').done(function (list) {
      state.calendars = list;
      if (!state.currentCalendar && list.length) { state.currentCalendar = list[0]; }
      renderCalList(list);
      updateRulesLink();
    });
  }

  function renderCalList(list) {
    $('#cal-list').empty();
    list.forEach(function (cal) {
      var item = $('<li class="list-group-item list-group-item-action">')
        .attr('data-id', cal.id)
        .text(cal.name + ' (' + cal.my_capability + ')');
      item.on('click', function () { selectCalendar(cal); });
      $('#cal-list').append(item);
    });
  }

  // ============ bs-calendar data feed ============
  function toAppointment(ev) {
    return {
      id: ev.id,
      title: ev.summary || '(untitled)',
      start: (ev.starts_at || ev.start_date).replace('T', ' ').substring(0, 19),
      end: ev.ends_at
        ? ev.ends_at.replace('T', ' ').substring(0, 19)
        : (ev.end_date || ev.start_date) + ' 23:59:00',
      color: (state.currentCalendar && state.currentCalendar.color) || '#0d6efd',
    };
  }

  function toIso(value) {
    if (!value) { return value; }
    var text = value.replace(' ', 'T');
    if (/^\d{4}-\d{2}-\d{2}$/.test(text)) { return text + 'T00:00:00Z'; }
    return /Z$/.test(text) ? text : text + 'Z';
  }

  function eventsUrl(requestData) {
    var cal = state.currentCalendar;
    if (!cal) { return Promise.resolve([]); }
    var params = new URLSearchParams({
      from: toIso(requestData.fromDate),
      to: toIso(requestData.toDate),
    });
    return fetch('/api/calendars/' + cal.id + '/occurrences?' + params)
      .then(function (r) { return r.json(); })
      .then(function (rows) {
        return rows.map(function (row) {
          var ev = row.event || row;
          return toAppointment(ev);
        });
      });
  }

  // ============ init ============
  $(function () {
    if (!window.jQuery) { return; }
    api('GET', '/api/auth/me').done(function (user) {
      if (user.is_admin) { $('#admin-nav-link').prop('hidden', false); }
    });
    loadCalendars().done(function () {
      $('#calendar').bsCalendar({
        url: function (requestData) { return eventsUrl(requestData); },
        startView: 'week',
      });
    });

    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () {
        window.location.href = '/login';
      });
    });
  });
})();