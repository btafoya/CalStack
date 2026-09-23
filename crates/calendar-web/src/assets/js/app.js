/* Calendar web app: jQuery 4 against /api. Progressive enhancement shell. */
(function () {
  'use strict';

  // ponytail: jQuery 4 dropped the deprecated $.now static; jquery-migrate
  // 3.x (1.x-3.x warnings only) doesn't restore it, and summernote-bs5 still
  // calls it internally. Shim just this one static rather than downgrading
  // jQuery or patching the vendored summernote bundle.
  if (!$.now) { $.now = Date.now; }

  var urlParams = new URLSearchParams(window.location.search);
  var state = {
    calendars: [],
    currentCalendar: null,
    csrf: sessionStorage.getItem('csrf') || '',
    eventCache: {},
    editingEventId: null,
    editingEtag: null,
    editingAttendees: [],
    // ponytail: RRULE editing only understands FREQ/INTERVAL/UNTIL; an
    // existing rule using BYDAY/COUNT/etc is left alone (flag set, key
    // omitted from the save body) rather than risk mangling it.
    editingRruleUnknown: false,
    // Which save path saveEvent uses: 'edit' (PATCH the row), 'occurrence'
    // (POST a RECURRENCE-ID exception for this occurrence only) or 'split'
    // (truncate the series at this occurrence + continuation).
    editingMode: 'edit',
    editingOccurrence: null,
    currentAcl: [],
    currentShares: [],
    calendarActivated: false,
    calendarChosen: false,
    categoryRegistry: [],
    eventCategories: [],
    username: '',
    userPrefs: { notify_email: true, notify_sms: true, notify_push: true },
    userIsAdmin: false,
    // restore target from ?calendar=&tab= (also accepts legacy ?calendar_id=)
    wantCalendar: urlParams.get('calendar') || urlParams.get('calendar_id'),
    wantTab: urlParams.get('tab'),
  };

  // ponytail: disables whatever button/submit triggered a mutating call, to
  // stop double-submits — keyed off document.activeElement rather than
  // threading a button reference through every one of api()'s ~15 callers.
  // Misses calls fired without a focused button (rare here); add an explicit
  // $btn param if that starts mattering.
  function api(method, url, data, extraHeaders) {
    var headers = method !== 'GET' ? { 'X-CSRF-Token': state.csrf } : {};
    if (extraHeaders) { $.extend(headers, extraHeaders); }
    var $btn = method !== 'GET' ? $(document.activeElement).filter('button, input[type="submit"]') : $();
    $btn.prop('disabled', true);
    var raw = data instanceof Blob;
    return $.ajax({
      method: method,
      url: url,
      data: raw ? data : (data !== undefined && data !== null ? JSON.stringify(data) : null),
      contentType: raw ? 'text/calendar' : 'application/json',
      processData: !raw,
      headers: headers,
    }).always(function () {
      $btn.prop('disabled', false);
    }).fail(function (xhr) {
      if (xhr.status === 401) { window.location.href = '/login'; return; }
      errorDialog((xhr.responseJSON && xhr.responseJSON.error) || 'Request failed');
    });
  }

  // ============ categories (checkbox list over the registry) ============
  // Grid hex values for the Tabler palette keys (calendar grid needs real
  // colors, not the bg-*-lt CSS tokens the badges use).
  var CATEGORY_HEX = {
    blue: '#1554C0', azure: '#12AEE8', indigo: '#4263eb', purple: '#ae3ec9',
    pink: '#d6336c', red: '#d63939', orange: '#f76707', yellow: '#f7b731',
    lime: '#74b816', green: '#12C957', teal: '#0ca678', cyan: '#12AEE8',
  };

  // Calendar/subscription colors are user data: a hex string or one of the
  // category color names above.
  function calColorHex(color) {
    if (!color) { return null; }
    return color.charAt(0) === '#' ? color : (CATEGORY_HEX[color] || null);
  }

  function categoryColorHex(ev) {
    var details = ev.category_details || [];
    for (var i = 0; i < details.length; i++) {
      if (CATEGORY_HEX[details[i].color]) { return CATEGORY_HEX[details[i].color]; }
    }
    return null;
  }

  // Silent $.getJSON: an unreachable registry must not alert() on modal open.
  function loadCategoryRegistry(calendarId) {
    $.getJSON('/api/categories', { calendar_id: calendarId }).done(function (rows) {
      state.categoryRegistry = rows || [];
    });
  }

  // One checkbox per registry row, plus one for each imported tag that is not
  // in the registry (so imported events can be untagged). Checked = tagged.
  function renderCategoryCheckboxes() {
    var $box = $('#ev-categories-box').empty();
    var selected = {};
    (state.eventCategories || []).forEach(function (s) { selected[s] = true; });
    var items = (state.categoryRegistry || []).concat((state.eventCategories || [])
      .filter(function (s) {
        return !(state.categoryRegistry || []).some(function (r) { return r.slug === s; });
      }).map(function (slug) { return { slug: slug, name: slug, color: null }; }));
    items.forEach(function (row) {
      var $check = $('<div class="form-check">');
      var $input = $('<input type="checkbox" class="form-check-input" id="ev-cat-' + row.slug + '">')
        .prop('checked', !!selected[row.slug])
        .on('change', function () {
          toggleCategory(row.slug, $input.is(':checked'));
        });
      var $label = $('<label class="form-check-label" for="ev-cat-' + row.slug + '">');
      if (row.color) {
        $label.append($('<span class="d-inline-block rounded-circle me-1" style="width:10px;height:10px">')
          .css('background-color', CATEGORY_HEX[row.color] || '#adb5bd'));
      }
      $label.append(document.createTextNode(row.name));
      if (!row.color) { $label.addClass('text-body-secondary'); }
      $check.append($input, $label).appendTo($box);
    });
    $box.prop('hidden', $box.is(':empty'));
  }

  function toggleCategory(slug, on) {
    var i = state.eventCategories.indexOf(slug);
    if (on && i < 0) { state.eventCategories.push(slug); }
    if (!on && i >= 0) { state.eventCategories.splice(i, 1); }
    renderCategoryCheckboxes();
  }

  function modal(id) {
    return bootstrap.Modal.getOrCreateInstance(document.getElementById(id));
  }

  // Calendar/ACL capability values as user-facing labels; owner is the
  // default state so it gets no suffix at all.
  var CAP_TEXT = { read_write: 'read / write', read_only: 'read-only', free_busy: 'free/busy' };
  function capText(cap) { return CAP_TEXT[cap] || ''; }

  // ============ calendars ============
  // ============ calendar-scoped tabs ============
  // The six views of the selected calendar live as Bootstrap tab panes in the
  // main area; the sidebar selection is the only calendar selector. Pane
  // scripts expose load(cal) and skip reloads for an unchanged calendar.
  var PANE_LOADERS = {
    categories: function (cal) { CategoriesPane.load(cal); },
    tasks: function (cal) { TasksPane.load(cal); },
    journals: function (cal) { JournalsPane.load(cal); },
    rules: function (cal) { RulesPane.load(cal); },
  };

  function currentTab() {
    return $('#cal-tabs .nav-link.active').attr('data-tab') || 'calendar';
  }

  function showTab(name) {
    var $btn = $('#cal-tabs [data-tab="' + name + '"]');
    if (!$btn.length || $btn.prop('hidden')) {
      name = 'calendar';
      $btn = $('#cal-tabs [data-tab="calendar"]');
    }
    bootstrap.Tab.getOrCreateInstance($btn[0]).show();
  }

  function setUrl() {
    var p = new URLSearchParams();
    // Subscriptions have no addressable URL — only owned calendars persist.
    if (state.currentCalendar && !state.currentCalendar.subscriptionId) {
      p.set('calendar', state.currentCalendar.id);
    }
    var tab = currentTab();
    if (tab !== 'calendar') { p.set('tab', tab); }
    var qs = p.toString();
    window.history.replaceState(null, '', window.location.pathname + (qs ? '?' + qs : ''));
  }

  function refreshActivePane() {
    var cal = state.currentCalendar;
    var fn = cal && PANE_LOADERS[currentTab()];
    if (fn) { fn(cal); }
  }

  function updateTabs() {
    var cal = state.currentCalendar;
    $('#tab-bar-row').prop('hidden', !cal);
    $('#tab-btn-tasks, #tab-btn-journals').prop('hidden', !!(cal && cal.readOnly));
    $('#tab-btn-rules').prop('hidden', !(cal && !cal.readOnly && state.userIsAdmin));
    if (cal && $('#cal-tabs [data-tab="' + currentTab() + '"]').prop('hidden')) {
      showTab('calendar');
    }
  }

  $(document).on('shown.bs.tab', '#cal-tabs [data-bs-toggle="tab"]', function () {
    refreshActivePane();
    setUrl();
  });

  // Only render the calendar widget once a calendar is actually selected;
  // otherwise show a placeholder in its place.
  function updateCalendarVisibility() {
    var selected = !!state.currentCalendar;
    $('#calendar').prop('hidden', !selected);
    $('#calendar-empty').prop('hidden', selected);
  }

  function selectCalendar(cal) {
    state.currentCalendar = cal;
    $('#cal-list li, #sub-list li').removeClass('active');
    $('#cal-list li[data-id="' + cal.id + '"], #sub-list li[data-id="' + cal.id + '"]').addClass('active');
    updateTabs();
    updateCalendarVisibility();
    // Category colors on a subscription's events come pre-attached per-event
    // by the server (its owner's registry) — the client-side registry is
    // only for the create/edit modal's checkboxes, which a read-only
    // calendar never opens.
    state.categoryRegistry = cal.readOnly ? [] : state.categoryRegistry;
    if (!cal.readOnly) { loadCategoryRegistry(cal.id); }
    if (!state.calendarActivated) {
      // Constructing bs-calendar while #calendar is still hidden (no
      // calendar selected yet) bakes in a wrong internal event-fetch date
      // range that no refresh()/setToday()/navigation call afterwards ever
      // corrects (confirmed: grid renders the correct week, but every
      // fetch keeps targeting a different one). Deferring construction
      // until the container is actually shown avoids the bad state
      // entirely. Later calendar switches just refresh() the existing
      // instance to keep whatever period the user has navigated to.
      state.calendarActivated = true;
      initCalendarWidget();
    } else {
      $('#calendar').bsCalendar('refresh');
    }
    refreshActivePane();
    setUrl();
  }

  function initCalendarWidget() {
    $('#calendar').bsCalendar({
      url: function (requestData) { return eventsUrl(requestData); },
      startView: 'month',
      locale: 'en-US',
      showTasks: false,
      onAfterLoad: function () {
        convertCalendarTimesToAmPm();
        decorateEventPills();
      },
      onAdd: function (data) {
        if (state.currentCalendar && state.currentCalendar.readOnly) { return; }
        openEventModal('create', {
          start: partToLocalInput(data && data.start, '09:00'),
          end: partToLocalInput(data && data.end, '10:00'),
        });
      },
      // ponytail: the three-way series dialog (this / this-and-following /
      // all) replaced the whole-series-only shortcut; exception rows (their
      // own RECURRENCE-ID override) and single events still edit directly.
      onEdit: function (appointment) {
        if (state.currentCalendar && state.currentCalendar.readOnly) { return; }
        var ev = state.eventCache[appointment.id];
        if (!ev) { return; }
        if (ev.rrule && !ev.master_event_id) {
          seriesDialog('Edit "' + (ev.summary || 'this event') + '"').done(function (choice) {
            if (choice === 'this') { openEventModal('occurrence', ev); }
            else if (choice === 'following') { openEventModal('split', ev); }
            else if (choice === 'all') { openEventModal('edit', ev); }
          });
          return;
        }
        openEventModal('edit', ev);
      },
      onDelete: function (appointment) {
        if (state.currentCalendar && state.currentCalendar.readOnly) { return; }
        var ev = state.eventCache[appointment.id];
        if (!ev) { return; }
        if (ev.rrule && !ev.master_event_id) {
          seriesDialog('Delete "' + (ev.summary || 'this event') + '"?').done(function (choice) {
            if (choice === 'this') { cancelOccurrence(ev); }
            else if (choice === 'following') { truncateSeries(ev); }
            else if (choice === 'all') { deleteEvent(ev.id, ev.etag); }
          });
          return;
        }
        confirmDialog('Delete "' + (ev.summary || 'this event') + '"?').done(function () {
          deleteEvent(ev.id, ev.etag);
        });
      },
    });
    startAmPmObserver();
  }

  // ============ calendar create/edit ============
  // One modal for both: name + component set (ADR-015). The Journals/Tasks
  // pages are unusable until a calendar carries VJOURNAL/VTODO, so the
  // component checkboxes are part of the calendar editor, not a settings
  // page. All three default on for new calendars.
  var editingCal = null;

  function openCalendarModal(cal) {
    editingCal = cal || null;
    $('#calendar-modal-title').text(cal ? 'Edit calendar' : 'New calendar');
    $('#cal-name').val(cal ? cal.name : '');
    $('#cal-source').val(cal ? (cal.source_url || '') : '');
    ['vevent', 'vtodo', 'vjournal'].forEach(function (kind) {
      var wanted = cal ? cal.components.indexOf(kind.toUpperCase()) !== -1 : true;
      $('#cal-comp-' + kind).prop('checked', wanted);
    });
    modal('calendar-modal').show();
  }

  $('#add-cal-btn').on('click', function () { openCalendarModal(null); });

  $('#calendar-form').on('submit', function (ev) {
    ev.preventDefault();
    var name = $('#cal-name').val().trim();
    if (!name) { return; }
    var components = ['vevent', 'vtodo', 'vjournal'].map(function (kind) {
      return $('#cal-comp-' + kind).prop('checked') ? kind.toUpperCase() : null;
    }).filter(Boolean);
    if (!components.length) { errorDialog('Pick at least one content type.'); return; }
    var sourceUrl = $('#cal-source').val().trim();
    var req;
    if (editingCal) {
      var patch = { name: name, components: components };
      // Send the source field only when it changed (owner-only server-side).
      var oldSource = editingCal.source_url || '';
      if (sourceUrl !== oldSource) { patch.source_url = sourceUrl; }
      req = api('PATCH', '/api/calendars/' + editingCal.id, patch);
    } else {
      var slug = name.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
      if (!slug) { errorDialog('Enter a valid calendar name.'); return; }
      var body = { slug: slug, name: name, components: components };
      if (sourceUrl) { body.source_url = sourceUrl; }
      req = api('POST', '/api/calendars', body);
    }
    req.done(function () {
      toast(editingCal ? 'Calendar updated.' : 'Calendar created.');
      modal('calendar-modal').hide();
      loadCalendars();
    });
  });

  function loadCalendars() {
    return api('GET', '/api/calendars').done(function (list) {
      state.calendars = list;
      renderCalList(list);
      updateCalendarVisibility();
      // First load: honor ?calendar= from the URL, else open the first
      // calendar so the tab bar has a selection. Later reloads (after
      // create/rename/delete) leave the current selection alone.
      if (!state.calendarChosen) {
        state.calendarChosen = true;
        var want = state.wantCalendar
          ? list.find(function (c) { return c.id === state.wantCalendar; })
          : null;
        state.wantCalendar = null;
        if (want || list.length) { selectCalendar(want || list[0]); }
        if (state.wantTab) {
          var t = state.wantTab;
          state.wantTab = null;
          showTab(t);
        }
      }
    });
  }

  function deleteCalendar(cal) {
    confirmDialog('Delete calendar "' + cal.name + '"? This cannot be undone.').done(function () {
      api('DELETE', '/api/calendars/' + cal.id).done(function () {
        if (state.currentCalendar && state.currentCalendar.id === cal.id) { state.currentCalendar = null; }
        toast('Calendar deleted.');
        loadCalendars();
      });
    });
  }

  function renderCalList(list) {
    $('#cal-list').empty();
    list.forEach(function (cal) {
      var item = $('<li class="list-group-item list-group-item-action d-flex justify-content-between align-items-center">')
        .attr('data-id', cal.id);
      var dot = calColorHex(cal.color);
      if (dot) { item.append($('<span class="cal-dot" aria-hidden="true">').css('background-color', dot)); }
      item.append($('<span>').text(cal.name + (cal.read_only ? ' (subscribed)' : (capText(cal.my_capability) ? ' (' + capText(cal.my_capability) + ')' : ''))));
      item.on('click', function () { selectCalendar(cal); });
      var btns = $('<span class="btn-group btn-group-sm">');
      btns.append(
        $('<button class="btn btn-outline-secondary" type="button" title="Connection info"><i class="bi bi-plug"></i></button>')
          .on('click', function (e) { e.stopPropagation(); showConnInfo(cal); })
      );
      if (cal.my_capability === 'owner' || cal.my_capability === 'read_write') {
        btns.append(
          $('<button class="btn btn-outline-secondary" type="button" title="Edit (name and content types)"><i class="bi bi-pencil"></i></button>')
            .on('click', function (e) { e.stopPropagation(); openCalendarModal(cal); })
        );
        if (cal.my_capability === 'owner') {
          btns.append(
            $('<button class="btn btn-outline-danger" type="button" title="Delete"><i class="bi bi-trash"></i></button>')
              .on('click', function (e) { e.stopPropagation(); deleteCalendar(cal); })
          );
        }
      }
      item.append(btns);
      $('#cal-list').append(item);
    });
  }

  // CalDAV/CardDAV connection details. The {user} path segment is ignored by
  // the DAV handler (calendars match on slug within the caller's access), so
  // the logged-in username works for shared calendars too.
  function showConnInfo(cal) {
    var $modal = $('#conn-modal');
    var base = window.location.origin;
    var calUrl = function (c) { return base + '/calendars/' + state.username + '/' + c.slug + '/'; };
    $modal.find('#conn-server').val(base);
    $modal.find('#conn-username').val(state.username);
    $modal.find('#conn-caldav-root').val(base + '/calendars/' + state.username + '/');
    $modal.find('#conn-caldav-wk').val(base + '/.well-known/caldav');
    $modal.find('#conn-carddav').val(base + '/contacts/');
    $modal.find('#conn-carddav-wk').val(base + '/.well-known/carddav');
    var row = $modal.find('#conn-calendar-row');
    var list = $modal.find('#conn-cal-list');
    if (cal) {
      row.removeClass('d-none');
      list.addClass('d-none');
      $modal.find('#conn-calendar-url').val(calUrl(cal));
    } else {
      row.addClass('d-none');
      list.removeClass('d-none');
      var $rows = $('#conn-cal-rows').empty();
      state.calendars.forEach(function (c) {
        $rows.append(
          $('<div class="input-group input-group-sm mb-1">')
            .append($('<input class="form-control conn-copy" readonly>')
              .val(calUrl(c))
              .attr('title', c.name))
            .append($('<button class="btn btn-outline-secondary" type="button" title="Copy"><i class="bi bi-clipboard"></i></button>'))
        );
      });
    }
    $modal.modal('show');
  }

  // ============ date helpers (native Date, no library) ============
  function pad(n) { return n < 10 ? '0' + n : String(n); }

  // bs-calendar's onAdd gives {date: "YYYY-MM-DD", time: "HH:MM"|null} for
  // start/end (time is null from a bare "+" add with no slot/range picked)
  // -> datetime-local value, defaulting the missing time.
  function partToLocalInput(part, fallbackTime) {
    if (!part || !part.date) { return ''; }
    return part.date + 'T' + (part.time || fallbackTime);
  }

  // UTC ISO instant -> datetime-local value in the browser's local time zone.
  function isoToLocalInput(iso) {
    if (!iso) { return ''; }
    if (/^\d{4}-\d{2}-\d{2}$/.test(iso)) { return iso + 'T00:00'; }
    var d = new Date(iso);
    return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) +
      'T' + pad(d.getHours()) + ':' + pad(d.getMinutes());
  }

  // datetime-local value (local wall clock) -> UTC ISO instant for the API.
  function localInputToIso(value) {
    return value ? new Date(value).toISOString() : null;
  }

  // ============ AM/PM start/end time controls ============
  // Native type="datetime-local"/"time" pickers render 12h vs 24h per the
  // browser/OS locale, not per-page — there's no attribute to force AM/PM.
  // These build our own date+hour+minute+AM/PM controls; #ev-start/#ev-end
  // stay hidden inputs holding the same "YYYY-MM-DDTHH:MM" value the rest
  // of the code already reads/writes, so save/load logic is untouched.
  function populateTimeSelectOptions() {
    ['start', 'end'].forEach(function (prefix) {
      var hourSel = $('#ev-' + prefix + '-hour').empty();
      for (var h = 1; h <= 12; h++) { hourSel.append($('<option>').val(pad(h)).text(pad(h))); }
      var minSel = $('#ev-' + prefix + '-min').empty();
      for (var m = 0; m < 60; m++) { minSel.append($('<option>').val(pad(m)).text(pad(m))); }
    });
  }

  function setTimeControls(prefix, value) {
    $('#ev-' + prefix).val(value || '');
    var d = value ? new Date(value) : null;
    var valid = d && !isNaN(d.getTime());
    $('#ev-' + prefix + '-date').val(valid ? value.slice(0, 10) : '');
    var h24 = valid ? d.getHours() : 9;
    $('#ev-' + prefix + '-hour').val(pad(h24 % 12 || 12));
    $('#ev-' + prefix + '-min').val(valid ? pad(d.getMinutes()) : '00');
    $('#ev-' + prefix + '-ampm').val(h24 >= 12 ? 'PM' : 'AM');
  }

  function syncTimeControls(prefix) {
    var date = $('#ev-' + prefix + '-date').val();
    var h12 = parseInt($('#ev-' + prefix + '-hour').val(), 10) || 12;
    var min = $('#ev-' + prefix + '-min').val() || '00';
    var h24 = $('#ev-' + prefix + '-ampm').val() === 'PM' ? (h12 % 12) + 12 : h12 % 12;
    $('#ev-' + prefix).val(date ? date + 'T' + pad(h24) + ':' + min : '');
  }

  populateTimeSelectOptions();
  ['start', 'end'].forEach(function (prefix) {
    $('#ev-' + prefix + '-date, #ev-' + prefix + '-hour, #ev-' + prefix + '-min, #ev-' + prefix + '-ampm')
      .on('change', function () { syncTimeControls(prefix); });
  });

  // ============ bs-calendar data feed ============
  function toAppointment(ev, occurrence) {
    var start, end;
    var durationMs = ev.starts_at && ev.ends_at ? (new Date(ev.ends_at) - new Date(ev.starts_at)) : 0;
    if (occurrence && occurrence.kind === 'timed') {
      var at = new Date(occurrence.at);
      start = at;
      end = new Date(at.getTime() + durationMs);
    } else if (occurrence && occurrence.kind === 'all_day') {
      start = new Date(occurrence.date + 'T00:00:00');
      end = start;
    } else {
      start = new Date(ev.starts_at || ev.start_date);
      end = new Date(ev.ends_at || ev.end_date || ev.starts_at || ev.start_date);
    }
    function fmt(d) {
      return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) + ' ' +
        pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds());
    }
    return {
      id: ev.id,
      title: ev.summary || '(untitled)',
      start: fmt(start),
      end: fmt(end),
      allDay: !!ev.start_date,
      // First registered category wins the event color; untagged events keep
      // the calendar color.
      color: categoryColorHex(ev) || (state.currentCalendar && state.currentCalendar.color) || '#1554C0',
    };
  }

  function toIso(value) {
    if (!value) { return value; }
    var text = value.replace(' ', 'T');
    if (/^\d{4}-\d{2}-\d{2}$/.test(text)) { return text + 'T00:00:00Z'; }
    return /Z$/.test(text) ? text : text + 'Z';
  }

  // bs-calendar reports a view's range as {start, end} where `end` is the
  // last VISIBLE day itself (e.g. day view: start === end), not one day
  // past it. The API takes a half-open [from, to) window, so a bare date
  // used as-is excludes every event on that last day. Advance it by one
  // day so the window actually covers it.
  function toIsoExclusiveEnd(value) {
    if (value && /^\d{4}-\d{2}-\d{2}$/.test(value)) {
      var d = new Date(value + 'T00:00:00Z');
      d.setUTCDate(d.getUTCDate() + 1);
      return d.toISOString();
    }
    return toIso(value);
  }

  // bs-calendar 2.4.0's week view passes a wrong fromDate/toDate to url()
  // (verified: consistently off by 1-2 weeks regardless of construction
  // options or setDate()/setToday() calls) while still rendering the
  // correct day-header cells. Read the actually-displayed week straight
  // from those headers instead of trusting the plugin's own range.
  function weekViewDateRange(requestData) {
    if (requestData.view === 'week') {
      var dates = $('#calendar .wc-day-header[data-date]').map(function () {
        return $(this).attr('data-date');
      }).get().sort();
      if (dates.length) { return { from: dates[0], to: dates[dates.length - 1] }; }
    }
    return { from: requestData.fromDate, to: requestData.toDate };
  }

  function eventsUrl(requestData) {
    var cal = state.currentCalendar;
    if (!cal) { return Promise.resolve([]); }
    var range = weekViewDateRange(requestData);
    var params = new URLSearchParams({
      from: toIso(range.from),
      to: toIsoExclusiveEnd(range.to),
    });
    var url = cal.subscriptionId
      ? '/api/subscriptions/' + cal.subscriptionId + '/occurrences?' + params
      : '/api/calendars/' + cal.id + '/occurrences?' + params;
    return fetch(url)
      .then(function (r) { return r.json(); })
      .then(function (rows) {
        return rows.map(function (row) {
          var ev = row.event || row;
          // The occurrence slot (which day of a series was clicked) and
          // whether this row is already a RECURRENCE-ID exception drive the
          // series dialog; the event view itself never carries them.
          ev._occ = row.occurrence || null;
          ev._isException = !!row.is_exception;
          state.eventCache[ev.id] = ev;
          return toAppointment(ev, row.occurrence);
        });
      });
  }

  // ============ event create/edit/delete ============
  function formatBytes(n) {
    if (n < 1024) { return n + ' B'; }
    if (n < 1024 * 1024) { return (n / 1024).toFixed(1) + ' KB'; }
    return (n / (1024 * 1024)).toFixed(1) + ' MB';
  }

  function renderAttachments(rows) {
    var list = $('#ev-attachments').empty();
    rows.forEach(function (a) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">');
      item.append($('<a target="_blank" rel="noopener">').attr('href', '/api/attachments/' + a.id)
        .text(a.filename + ' (' + formatBytes(a.byte_size) + ')'));
      var btnGroup = $('<span>');
      var infoBtn = $('<button class="btn btn-sm btn-outline-secondary me-1" type="button" title="Details">' +
        '<i class="bi bi-info-circle"></i></button>');
      infoBtn.on('click', function () {
        api('GET', '/api/attachments/' + a.id + '/meta').done(function (meta) {
          Swal.fire({
            icon: 'info',
            title: meta.filename,
            html: 'Type: ' + $('<span>').text(meta.content_type).html() + '<br>'
              + 'Size: ' + formatBytes(meta.byte_size) + '<br>'
              + 'SHA-256: <code>' + $('<span>').text(meta.sha256).html() + '</code><br>'
              + 'Uploaded: ' + $('<span>').text(meta.created_at).html(),
          });
        });
      });
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Delete</button>');
      btn.on('click', function () {
        confirmDialog('Delete attachment "' + a.filename + '"?').done(function () {
          api('DELETE', '/api/attachments/' + a.id).done(function () {
            toast('Attachment deleted.');
            loadAttachments();
          });
        });
      });
      btnGroup.append(infoBtn).append(btn);
      item.append(btnGroup);
      list.append(item);
    });
  }

  function loadAttachments() {
    if (!state.currentCalendar || !state.editingEventId) { return; }
    api('GET', '/api/calendars/' + state.currentCalendar.id + '/events/' + state.editingEventId + '/attachments')
      .done(renderAttachments);
  }

  $('#ev-attach-file').on('change', function () {
    var file = this.files[0];
    if (!file || !state.currentCalendar || !state.editingEventId) { return; }
    var reader = new FileReader();
    reader.onload = function () {
      var base64 = reader.result.split(',')[1];
      api('POST', '/api/calendars/' + state.currentCalendar.id + '/events/' + state.editingEventId + '/attachments', {
        filename: file.name,
        content_type: file.type || 'application/octet-stream',
        data: base64,
      }).done(function () {
        $('#ev-attach-file').val('');
        toast('Attachment uploaded.');
        loadAttachments();
      });
    };
    reader.readAsDataURL(file);
  });

  function renderAttendees() {
    var list = $('#ev-attendees').empty();
    state.editingAttendees.forEach(function (a, i) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">');
      var reach = a.email ? a.email : a.telephone;
      item.append($('<span>').text(a.display_name ? a.display_name + ' <' + reach + '>' : reach));
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Remove</button>');
      btn.on('click', function () {
        state.editingAttendees.splice(i, 1);
        renderAttendees();
      });
      item.append(btn);
      list.append(item);
    });
  }

  // Attendees come only from contacts/the tenant directory (no freeform
  // entry) — search /api/contacts/autocomplete, click a result to add it
  // immediately. Emailless contacts with a phone number are SMS attendees.
  function hideAttendeeResults() {
    $('#ev-attendee-results').empty().prop('hidden', true);
  }

  function renderAttendeeResults(list) {
    var $results = $('#ev-attendee-results').empty();
    (list || []).forEach(function (c) {
      var email = c.emails && c.emails[0] && c.emails[0].email;
      var tel = c.tels && c.tels[0] && c.tels[0].number;
      if (!email && !tel) { return; }
      var reach = email || tel;
      var already = state.editingAttendees.some(function (a) {
        return (a.email && email && a.email.toLowerCase() === email.toLowerCase())
          || (a.telephone && tel && a.telephone === tel);
      });
      $('<button type="button" class="list-group-item list-group-item-action py-1"></button>')
        .toggleClass('disabled', already)
        .append($('<div>').text(c.full_name || reach))
        .append($('<small class="text-body-secondary d-block">').text(
          reach + (c.directory ? ' · directory' : '') + (email ? '' : ' · sms')))
        .on('click', function () {
          if (already) { return; }
          state.editingAttendees.push({
            email: email || null,
            telephone: email ? null : tel,
            display_name: c.full_name || null,
            contact_id: c.directory ? null : c.id,
            user_id: c.directory ? c.id : null,
          });
          $('#ev-attendee-search').val('');
          hideAttendeeResults();
          renderAttendees();
        })
        .appendTo($results);
    });
    $results.prop('hidden', $results.children().length === 0);
  }

  var attendeeSearchTimer = null;
  $('#ev-attendee-search').on('input', function () {
    var q = $(this).val().trim();
    window.clearTimeout(attendeeSearchTimer);
    if (q.length < 2) { hideAttendeeResults(); return; }
    attendeeSearchTimer = window.setTimeout(function () {
      api('GET', '/api/contacts/autocomplete?q=' + encodeURIComponent(q)).done(renderAttendeeResults);
    }, 200);
  });
  $(document).on('click', function (e) {
    if (!$(e.target).closest('#ev-attendee-search, #ev-attendee-results').length) { hideAttendeeResults(); }
  });

  $('#ev-repeat').on('change', function () {
    var show = !!$(this).val();
    $('#ev-repeat-interval-row, #ev-repeat-until-row').prop('hidden', !show);
  });

  // Only a FREQ/INTERVAL/UNTIL rule can round-trip through the simple
  // picker; returns false (and blanks the picker) for anything richer, so
  // saveEvent knows to leave the underlying RRULE untouched.
  function applyRruleToForm(rrule) {
    $('#ev-repeat-interval-row, #ev-repeat-until-row').prop('hidden', true);
    if (!rrule) {
      $('#ev-repeat').val('');
      return true;
    }
    var parts = {};
    rrule.split(';').forEach(function (p) {
      var kv = p.split('=');
      parts[kv[0]] = kv[1];
    });
    var known = ['FREQ', 'INTERVAL', 'UNTIL'];
    var onlyKnown = Object.keys(parts).every(function (k) { return known.indexOf(k) !== -1; });
    if (!onlyKnown || !parts.FREQ) {
      $('#ev-repeat').val('');
      return false;
    }
    $('#ev-repeat').val(parts.FREQ);
    $('#ev-repeat-interval').val(parts.INTERVAL || 1);
    $('#ev-repeat-until').val(parts.UNTIL
      ? parts.UNTIL.slice(0, 4) + '-' + parts.UNTIL.slice(4, 6) + '-' + parts.UNTIL.slice(6, 8)
      : '');
    $('#ev-repeat-interval-row, #ev-repeat-until-row').prop('hidden', false);
    return true;
  }

  function buildRrule() {
    var freq = $('#ev-repeat').val();
    if (!freq) { return null; }
    var parts = ['FREQ=' + freq];
    var interval = parseInt($('#ev-repeat-interval').val(), 10);
    if (interval > 1) { parts.push('INTERVAL=' + interval); }
    var until = $('#ev-repeat-until').val();
    if (until) { parts.push('UNTIL=' + until.replace(/-/g, '') + 'T235959Z'); }
    return parts.join(';');
  }

  // Which part of a recurring series a click on one occurrence touches.
  // Resolves 'this' | 'following' | 'all', or null on a bare dismiss (Esc /
  // backdrop) so callers no-op — only the explicit "All events" button picks
  // the whole series.
  function seriesDialog(title) {
    var d = $.Deferred();
    Swal.fire($.extend({}, BUTTONS, {
      title: title,
      showDenyButton: true,
      showCancelButton: true,
      confirmButtonText: 'This event',
      denyButtonText: 'This and following',
      cancelButtonText: 'All events',
      customClass: $.extend({}, BUTTONS.customClass, { denyButton: 'btn btn-secondary' }),
    })).then(function (r) {
      if (r.isConfirmed) { d.resolve('this'); }
      else if (r.isDenied) { d.resolve('following'); }
      else if (r.dismiss === Swal.DismissReason.cancel) { d.resolve('all'); }
      else { d.resolve(null); }
    });
    return d.promise();
  }

  // The occurrence slot as start/end instants the API accepts: the master's
  // duration stretched over the clicked day.
  function occurrenceSlot(ev, occ) {
    var durationMs = ev.starts_at && ev.ends_at ? (new Date(ev.ends_at) - new Date(ev.starts_at)) : 0;
    if (occ && occ.kind === 'timed') {
      var at = new Date(occ.at);
      return { timed: true, start: occ.at, end: new Date(at.getTime() + durationMs).toISOString() };
    }
    var date = (occ && occ.date) || '';
    var days = ev.start_date && ev.end_date
      ? Math.round((Date.parse(ev.end_date) - Date.parse(ev.start_date)) / 86400000) : 0;
    var shifted = new Date(Date.parse(date + 'T00:00:00Z'));
    shifted.setUTCDate(shifted.getUTCDate() + days);
    return { timed: false, start: date, end: shifted.toISOString().slice(0, 10) };
  }

  // "Delete this occurrence": a STATUS:CANCELLED override carrying the
  // master's own fields, keyed to the clicked day.
  function cancelOccurrence(ev) {
    var occ = ev._occ;
    if (!occ) { return; }
    var slot = occurrenceSlot(ev, occ);
    var body = {
      uid: ev.uid,
      summary: ev.summary || '(untitled)',
      description_html: ev.description_html || null,
      description_text: ev.description_text || null,
      url: ev.url || null,
      status: 'CANCELLED',
      class: ev.class || null,
      transp: ev.transp || null,
      categories: ev.categories || [],
      location: ev.location || null,
      master_event_id: ev.id,
    };
    if (slot.timed) {
      body.all_day = false;
      body.starts_at = slot.start;
      body.ends_at = slot.end;
      body.recurrence_id_at = occ.at;
    } else {
      body.all_day = true;
      body.start_date = slot.start;
      body.end_date = slot.end;
      body.recurrence_id_date = occ.date;
    }
    api('POST', '/api/calendars/' + state.currentCalendar.id + '/events', body).done(function () {
      toast('Occurrence cancelled.');
      $('#calendar').bsCalendar('refresh');
    });
  }

  // "Delete this and following": truncate the series at the clicked day.
  function truncateSeries(ev) {
    var occ = ev._occ;
    if (!occ) { return; }
    var body = { truncate_rest: true, summary: ev.summary || '(untitled)' };
    if (occ.kind === 'timed') { body.recurrence_id_at = occ.at; }
    else { body.recurrence_id_date = occ.date; }
    api('POST', '/api/events/' + ev.id + '/split', body).done(function () {
      toast('Future occurrences deleted.');
      $('#calendar').bsCalendar('refresh');
    });
  }

  function openEventModal(mode, payload) {
    $('#event-form')[0].reset();
    state.eventDirty = false;
    state.editingMode = mode;
    state.editingOccurrence = null;
    $('#event-modal-title').text(
      mode === 'occurrence' ? 'Edit this occurrence'
        : mode === 'split' ? 'Edit from here on'
        : 'Event');
    $('#ev-delete').prop('hidden', mode !== 'edit');
    $('#ev-attachments-section').prop('hidden', mode !== 'edit');
    // A single-occurrence edit can't touch the series rule; the picker is
    // meaningless there. 'split' keeps it (the continuation carries it).
    $('#ev-repeat-row, #ev-repeat-interval-row, #ev-repeat-until-row')
      .prop('hidden', mode === 'occurrence');
    state.editingAttendees = [];
    state.placeLocation = null;
    pickedPlace = null;
    hidePlaceMenu();
    if (mode !== 'create') {
      state.editingEventId = payload.id;
      state.editingEtag = mode === 'edit' ? payload.etag : null;
      // 'occurrence' saves an exception — it never sends a rule; 'split'
      // prefills the master's rule so the continuation can carry a change.
      state.editingRruleUnknown = !applyRruleToForm(
        mode === 'occurrence' ? null : payload.rrule);
      state.editingOccurrence = mode === 'edit' ? null : (payload._occ || null);
      $('#ev-title').val(payload.summary || '');
      // The occurrence's own slot (the clicked day), not the series DTSTART.
      var slot = mode === 'edit' ? null : occurrenceSlot(payload, payload._occ);
      setTimeControls('start', slot
        ? isoToLocalInput(slot.start)
        : isoToLocalInput(payload.starts_at || payload.start_date));
      setTimeControls('end', slot
        ? isoToLocalInput(slot.end)
        : isoToLocalInput(payload.ends_at || payload.end_date));
      $('#ev-all-day').prop('checked', !!payload.all_day);
      $('#ev-url').val(payload.url || '');
      $('#ev-status').val(payload.status || '');
      $('#ev-class').val(payload.class || '');
      $('#ev-transp').val(payload.transp || '');
      state.eventCategories = (payload.categories || []).slice();
      var loc = payload.location || {};
      $('#ev-location').val(locationDisplayText(loc));
      // Carrying a place-picked location through: reuse its structured fields
      // unless the user edits the text afterwards.
      if (loc.provider_place_id) {
        state.placeLocation = $.extend({}, loc);
        pickedText = locationDisplayText(loc);
      }
      state.editingAttendees = (payload.attendees || []).map(function (a) {
        return {
          email: a.email || null, telephone: a.telephone || null,
          display_name: a.display_name || null,
          contact_id: a.contact_id || null, user_id: a.user_id || null,
        };
      });
      $('#ev-desc').summernote('code', payload.description_html || '');
      loadAttachments();
      // Open "More options" if editing an event that already uses one of the
      // fields collapsed in there, so its config isn't hidden by surprise.
      $('#ev-more-options').prop('open', !!(payload.status || payload.class || payload.transp || payload.rrule));
    } else {
      $('#ev-more-options').prop('open', false);
      state.editingEventId = null;
      state.editingEtag = null;
      state.editingRruleUnknown = false;
      state.eventCategories = [];
      applyRruleToForm(null);
      setTimeControls('start', payload.start || '');
      setTimeControls('end', payload.end || '');
      $('#ev-desc').summernote('code', '');
    }
    renderAttendees();
    renderCategoryCheckboxes();
    modal('event-modal').show();
  }

  // ============ place autocomplete (server-side Google proxy) ============
  // Silent $.getJSON: typing shouldn't alert() when the proxy is unconfigured.
  var placeTimer = null;
  var pickedPlace = null;
  // The field text as the picker wrote it; typing anything else drops the
  // structured place and the text becomes a plain free-text location.
  var pickedText = null;

  function hidePlaceMenu() {
    $('#ev-places-menu').empty().prop('hidden', true);
  }

  // What the single Location field shows for a structured place: name and
  // full address (Google's formatted_address alone often drops the name).
  function locationDisplayText(loc) {
    var name = loc.display_name || '';
    var address = loc.formatted_address || '';
    if (name && address && name !== address) { return name + ' — ' + address; }
    return name || address;
  }

  $('#ev-location').on('input', function () {
    hidePlaceMenu();
    if (pickedPlace && $(this).val() !== pickedText) {
      state.placeLocation = null;
      pickedPlace = null;
      pickedText = null;
    }
    var q = $(this).val();
    if (q.length < 2) { return; }
    clearTimeout(placeTimer);
    placeTimer = setTimeout(function () {
      $.getJSON('/api/places/autocomplete', { q: q }).done(function (list) {
        if (!list || !list.length) { return; }
        var $menu = $('#ev-places-menu').empty();
        list.forEach(function (item) {
          $menu.append($('<a href="#" class="list-group-item list-group-item-action py-1">')
            .text(item.label)
            .data('placeId', item.place_id));
        });
        $menu.prop('hidden', false);
      });
    }, 300);
  });

  $('#ev-places-menu').on('click', 'a', function (ev) {
    ev.preventDefault();
    var id = $(this).data('placeId');
    hidePlaceMenu();
    $.getJSON('/api/places/' + encodeURIComponent(id)).done(function (loc) {
      state.placeLocation = loc;
      pickedPlace = loc;
      // Show name and full address; both stay in the structured fields.
      pickedText = locationDisplayText(loc);
      $('#ev-location').val(pickedText);
    });
  });

  function saveEvent(e) {
    e.preventDefault();
    if (!state.currentCalendar) { return; }
    var html = $('#ev-desc').summernote('isEmpty') ? null : $('#ev-desc').summernote('code');
    var allDay = $('#ev-all-day').is(':checked');
    var locationText = $('#ev-location').val();
    var body = {
      summary: $('#ev-title').val(),
      description_html: html,
      description_text: html ? $('<div>').html(html).text() : null,
      url: $('#ev-url').val() || null,
      status: $('#ev-status').val() || null,
      class: $('#ev-class').val() || null,
      transp: $('#ev-transp').val() || null,
      categories: state.eventCategories,
      attendees: state.editingAttendees,
      // Picked place: structured fields (name + full address) as Google
      // returned them; the text is just the visible address. Otherwise the
      // text IS the location (free text, no structured parts).
      location: state.placeLocation
        ? $.extend({}, state.placeLocation)
        : locationText
          ? { display_name: locationText }
          : null,
    };
    if (allDay) {
      body.all_day = true;
      body.start_date = $('#ev-start').val().slice(0, 10);
      body.end_date = $('#ev-end').val().slice(0, 10);
    } else {
      body.all_day = false;
      body.starts_at = localInputToIso($('#ev-start').val());
      body.ends_at = localInputToIso($('#ev-end').val());
    }
    // An exception body carries no rule; a split body carries the rule the
    // continuation should use (omitted when the picker can't rebuild it —
    // the server then keeps the master's rule).
    if (state.editingMode !== 'occurrence' && !state.editingRruleUnknown) {
      body.rrule = buildRrule();
    }
    if (state.editingMode === 'occurrence') {
      // A RECURRENCE-ID exception for just this occurrence; the server
      // derives the wall-clock RECURRENCE-ID from the occurrence instant.
      var master = state.eventCache[state.editingEventId];
      body.master_event_id = state.editingEventId;
      body.uid = master ? master.uid : undefined;
      if (state.editingOccurrence) {
        if (state.editingOccurrence.kind === 'timed') {
          body.recurrence_id_at = state.editingOccurrence.at;
        } else {
          body.recurrence_id_date = state.editingOccurrence.date;
        }
      }
      req = api('POST', '/api/calendars/' + state.currentCalendar.id + '/events', body);
    } else if (state.editingMode === 'split') {
      if (state.editingOccurrence) {
        if (state.editingOccurrence.kind === 'timed') {
          body.recurrence_id_at = state.editingOccurrence.at;
        } else {
          body.recurrence_id_date = state.editingOccurrence.date;
        }
      }
      req = api('POST', '/api/events/' + state.editingEventId + '/split', body);
    } else if (state.editingEventId) {
      req = api('PATCH', '/api/events/' + state.editingEventId, body,
        state.editingEtag ? { 'If-Match': state.editingEtag } : {});
    } else {
      req = api('POST', '/api/calendars/' + state.currentCalendar.id + '/events', body);
    }
    req.done(function () {
      state.eventDirty = false;
      modal('event-modal').hide();
      toast('Event saved.');
      $('#calendar').bsCalendar('refresh');
    });
  }

  function deleteEvent(id, etag) {
    api('DELETE', '/api/events/' + id, null, etag ? { 'If-Match': etag } : {}).done(function () {
      // Clear before hide so the dirty-check guard doesn't re-prompt after
      // the delete was already confirmed.
      state.eventDirty = false;
      modal('event-modal').hide();
      toast('Event deleted.');
      $('#calendar').bsCalendar('refresh');
    });
  }

  // ponytail: no per-field diffing (form fields have no name attrs for
  // serialize()) — any input/change inside the form is close enough to
  // "dirty" to guard against losing a half-filled event.
  $('#event-form').on('input change', 'input, select, textarea', function () {
    state.eventDirty = true;
  });
  $('#event-modal').on('hide.bs.modal', function (e) {
    if (state.eventDirty) {
      e.preventDefault();
      confirmDialog('Discard unsaved changes?').done(function () {
        state.eventDirty = false;
        modal('event-modal').hide();
      });
    }
  });
  $('#event-form').on('submit', saveEvent);
  $('#ev-delete').on('click', function () {
    if (!state.editingEventId) { return; }
    confirmDialog('Delete this event?').done(function () {
      deleteEvent(state.editingEventId, state.editingEtag);
    });
  });

  // ============ sharing / ACL ============
  function renderAcl() {
    var tbody = $('#acl-rows').empty();
    state.currentAcl.forEach(function (entry) {
      var row = $('<tr>');
      row.append($('<td>').text(entry.user_id));
      row.append($('<td>').text(capText(entry.capability) || entry.capability));
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Remove</button>');
      btn.on('click', function () {
        state.currentAcl = state.currentAcl.filter(function (e) { return e.user_id !== entry.user_id; });
        saveAcl();
      });
      row.append($('<td>').append(btn));
      tbody.append(row);
    });
  }

  function loadAcl() {
    return api('GET', '/api/calendars/' + state.currentCalendar.id + '/acl').done(function (rows) {
      state.currentAcl = rows;
      renderAcl();
    });
  }

  function saveAcl() {
    return api('PUT', '/api/calendars/' + state.currentCalendar.id + '/acl', { entries: state.currentAcl })
      .done(function () {
        toast('Access updated.');
        loadAcl();
      });
  }

  // ACL additions go through the tenant directory, not a raw user UUID: the
  // same autocomplete the attendee search uses, filtered to directory rows
  // (their ids are user ids).
  var aclUserId = null;
  var aclUserTimer = null;
  function hideAclResults() {
    $('#acl-user-results').empty().prop('hidden', true);
  }
  $('#acl-user').on('input', function () {
    aclUserId = null;
    var q = $(this).val().trim();
    window.clearTimeout(aclUserTimer);
    if (q.length < 2) { hideAclResults(); return; }
    aclUserTimer = window.setTimeout(function () {
      api('GET', '/api/contacts/autocomplete?q=' + encodeURIComponent(q)).done(function (list) {
        var $results = $('#acl-user-results').empty();
        (list || []).filter(function (c) { return c.directory; }).forEach(function (c) {
          var reach = (c.emails && c.emails[0] && c.emails[0].email) || c.id;
          $('<button type="button" class="list-group-item list-group-item-action py-1"></button>')
            .append($('<div>').text(c.full_name || reach))
            .append($('<small class="text-body-secondary d-block">').text(reach + ' · directory'))
            .on('click', function () {
              aclUserId = c.id;
              $('#acl-user').val(c.full_name || reach);
              hideAclResults();
            })
            .appendTo($results);
        });
        if (!$results.children().length) {
          $results.append('<div class="list-group-item text-body-secondary py-1">No directory users match.</div>');
        }
        $results.prop('hidden', false);
      });
    }, 200);
  });
  $(document).on('click', function (e) {
    if (!$(e.target).closest('#acl-user, #acl-user-results').length) { hideAclResults(); }
  });

  $('#acl-add').on('click', function () {
    if (!aclUserId) { return; }
    state.currentAcl.push({ user_id: aclUserId, capability: $('#acl-cap').val(), can_manage_acl: false });
    saveAcl();
    aclUserId = null;
    $('#acl-user').val('');
  });

  function renderShares() {
    var out = $('#share-out').empty();
    if (!state.currentShares.length) { return; }
    var list = $('<ul class="list-group">');
    state.currentShares.forEach(function (s) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">');
      item.append($('<span>').text((s.allows_caldav ? 'CalDAV link' : 'Public link') + ' — created ' + s.created_at));
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Revoke</button>');
      btn.on('click', function () {
        api('DELETE', '/api/calendars/' + state.currentCalendar.id + '/shares/' + s.id).done(loadShares);
      });
      item.append(btn);
      list.append(item);
    });
    out.append(list);
  }

  function loadShares() {
    return api('GET', '/api/calendars/' + state.currentCalendar.id + '/shares').done(function (rows) {
      state.currentShares = rows;
      renderShares();
    });
  }

  function createShare(allowsCaldav) {
    api('POST', '/api/calendars/' + state.currentCalendar.id + '/shares', { allows_caldav: allowsCaldav })
      .done(function (share) {
        var url = window.location.origin + '/share/' + share.token + '/calendar.ics';
        Swal.fire({
          icon: 'info',
          title: 'Copy the link now — shown only once',
          input: 'text',
          inputValue: url,
          didOpen: function (popup) { popup.querySelector('input').select(); },
        });
        loadShares();
      });
  }

  $('#share-create').on('click', function () { createShare(false); });
  $('#share-create-caldav').on('click', function () { createShare(true); });

  $('#share-btn').on('click', function () {
    if (!state.currentCalendar) { return; }
    loadAcl();
    loadShares();
    modal('share-modal').show();
  });

  // ============ ICS import / export for the current calendar ============
  $('#export-btn').on('click', function () {
    if (!state.currentCalendar) { return; }
    var a = document.createElement('a');
    a.href = '/api/calendars/' + state.currentCalendar.id + '/export.ics';
    a.download = state.currentCalendar.slug + '.ics';
    document.body.appendChild(a);
    a.click();
    a.remove();
  });
  $('#import-btn').on('click', function () {
    if (!state.currentCalendar) { return; }
    if (state.currentCalendar.read_only) { errorDialog('This calendar is fed from a remote source and is read-only.'); return; }
    $('#ics-import-input').val('').trigger('click');
  });
  $('#ics-import-input').on('change', function () {
    var file = this.files[0];
    var cal = state.currentCalendar;
    if (!file || !cal) { return; }
    api('POST', '/api/calendars/' + cal.id + '/import', file).done(function (r) {
      var msg = r.imported + ' imported, ' + r.skipped + ' skipped';
      if (r.rejected.length) { msg += ', ' + r.rejected.length + ' rejected'; }
      toast(msg + '.');
      refreshActivePane();
    });
  });

  // ============ account (password change) ============
  $('#account-btn').on('click', function () {
    $('#account-current-password, #account-new-password, #account-new-password-confirm').val('');
    $('#account-password-msg').text('');
    $('#notify-msg').text('');
    $('#notify-email').prop('checked', state.userPrefs.notify_email);
    $('#notify-sms').prop('checked', state.userPrefs.notify_sms);
    $('#notify-push').prop('checked', state.userPrefs.notify_push);
    modal('account-modal').show();
  });

  $('#notify-prefs-save').on('click', function () {
    api('POST', '/api/auth/notify-prefs', {
      notify_email: $('#notify-email').prop('checked'),
      notify_sms: $('#notify-sms').prop('checked'),
      notify_push: $('#notify-push').prop('checked'),
    }).done(function () {
      state.userPrefs = {
        notify_email: $('#notify-email').prop('checked'),
        notify_sms: $('#notify-sms').prop('checked'),
        notify_push: $('#notify-push').prop('checked'),
      };
      $('#notify-msg').text('Saved.');
    });
  });

  // Web Push: register the service worker and subscribe with the tenant's
  // VAPID public key (dormant until a webpush provider exists).
  function urlBase64ToUint8Array(b64) {
    var padding = '='.repeat((4 - (b64.length % 4)) % 4);
    var raw = atob(b64.replace(/-/g, '+').replace(/_/g, '/') + padding);
    return Uint8Array.from(raw.split('').map(function (c) { return c.charCodeAt(0); }));
  }
  $('#push-enable-btn').on('click', function () {
    var $msg = $('#notify-msg').text('');
    if (!('serviceWorker' in navigator) || !('PushManager' in window)) {
      $msg.addClass('text-danger').text('This browser does not support push notifications.');
      return;
    }
    navigator.serviceWorker.register('/sw.js').then(function (reg) {
      return api('GET', '/api/push/public-key').then(function (key) {
        if (!key) {
          $msg.addClass('text-danger').text('No push provider configured yet (admins: add one on the Providers page).');
          throw new Error('no key');
        }
        return reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlBase64ToUint8Array(key),
        });
      });
    }).then(function (sub) {
      var json = sub.toJSON();
      return api('POST', '/api/push/subscriptions', json).then(function () {
        $msg.text('Push enabled on this device.');
      });
    }).catch(function (err) {
      if (String(err.message) !== 'no key') {
        $msg.addClass('text-danger').text('Could not enable push: ' + err.message);
      }
    });
  });

  // ============ DAV connection info ============
  $('#conn-info-btn').on('click', function () { showConnInfo(null); });
  $('#conn-modal').on('click', '.input-group button', function () {
    var input = $(this).siblings('.conn-copy')[0];
    var $icon = $(input).siblings('button').find('i');
    var done = function () {
      $icon.attr('class', 'bi bi-check2');
      setTimeout(function () { $icon.attr('class', 'bi bi-clipboard'); }, 1200);
    };
    // Clipboard API needs a secure context; fall back to selecting for copy.
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(input.value).then(done);
    } else { input.select(); document.execCommand('copy'); done(); }
  });

  $('#account-password-save').on('click', function () {
    var current = $('#account-current-password').val();
    var next = $('#account-new-password').val();
    var confirm = $('#account-new-password-confirm').val();
    if (next.length < 8) {
      $('#account-password-msg').text('New password must be at least 8 characters.');
      return;
    }
    if (next !== confirm) {
      $('#account-password-msg').text('New password and confirmation do not match.');
      return;
    }
    api('POST', '/api/auth/password', { current_password: current, new_password: next })
      .done(function () {
        modal('account-modal').hide();
        Swal.fire({
          icon: 'success',
          title: 'Password changed. Your other sessions have been signed out.',
        });
      });
  });

  // ============ subscriptions (read-only calendars shared by others) ============
  // A subscription behaves like a calendar in the main view (click to see its
  // events) but is never in state.calendars and carries no ACL — selectCalendar
  // takes a synthetic cal-shaped object for it, tagged readOnly so add/edit/
  // delete and the rules link know to refuse it.
  function subToCal(s) {
    return {
      id: 'sub:' + s.id, subscriptionId: s.id, name: s.calendar_name,
      color: s.color, readOnly: true,
    };
  }

  function renderSubscriptions(rows) {
    var list = $('#sub-list').empty();
    rows.forEach(function (s) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">')
        .attr('data-id', 'sub:' + s.id);
      var dot = calColorHex(s.color);
      if (dot) { item.append($('<span class="cal-dot" aria-hidden="true">').css('background-color', dot)); }
      var label = $('<span>').text(s.calendar_name);
      if (!s.live) {
        item.addClass('text-muted');
        label.append($('<span class="badge bg-warning text-dark ms-2">').text('removed by owner'));
      } else {
        item.addClass('list-group-item-action').css('cursor', 'pointer');
        item.on('click', function () { selectCalendar(subToCal(s)); });
      }
      item.append(label);
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">&times;</button>')
        .attr('title', s.live ? 'Unsubscribe' : 'Remove');
      btn.on('click', function (e) {
        e.stopPropagation();
        api('DELETE', '/api/subscriptions/' + s.id).done(function () {
          if (state.currentCalendar && state.currentCalendar.subscriptionId === s.id) {
            state.currentCalendar = null;
            updateTabs();
            updateCalendarVisibility();
          }
          toast('Unsubscribed.');
          loadSubscriptions();
        });
      });
      item.append(btn);
      list.append(item);
    });
  }

  function loadSubscriptions() {
    return api('GET', '/api/subscriptions').done(renderSubscriptions);
  }

  $('#sub-add-btn').on('click', function () {
    var token = $('#sub-token').val().trim();
    if (!token) { return; }
    api('POST', '/api/subscriptions', { share_token: token }).done(function () {
      $('#sub-token').val('');
      toast('Subscribed.');
      loadSubscriptions();
    });
  });

  // ============ search ============
  // ponytail: results are read-only (summary/time only); opening a hit in the
  // editor would need switching the selected calendar first — add if search
  // needs to jump straight into editing.
  // Search hits are read-only; dates render in the browser's locale.
  function fmtSearchWhen(e) {
    if (e.starts_at) { return new Date(e.starts_at).toLocaleString(); }
    if (e.start_date) { return new Date(e.start_date + 'T00:00:00').toLocaleDateString(); }
    return '';
  }

  function renderSearchResults(rows) {
    var list = $('#search-results').empty();
    if (!rows.length) { list.append($('<li class="list-group-item text-body-secondary">').text('No matches.')); }
    rows.forEach(function (e) {
      list.append(
        $('<li class="list-group-item">').text((e.summary || '(untitled)') + ' — ' + fmtSearchWhen(e))
      );
    });
  }

  $('#search-go').on('click', function () {
    var q = $('#search-q').val().trim();
    if (!q) { return; }
    api('GET', '/api/search?q=' + encodeURIComponent(q)).done(renderSearchResults);
  });
  $('#search-q').on('keydown', function (e) {
    if (e.key === 'Enter') { e.preventDefault(); $('#search-go').trigger('click'); }
  });
  $('#search-btn').on('click', function () {
    $('#search-results').empty();
    modal('search-modal').show();
  });

  // bs-calendar's day/week hour-axis and "now" indicator render fixed
  // 24-hour labels with no locale/format option; rewrite them to 12-hour
  // AM/PM after each render (locale: 'en-US' below covers its other,
  // toLocaleTimeString-based popups/tooltips).
  function convertCalendarTimesToAmPm() {
    var re = /^([01]?\d|2[0-3]):([0-5]\d)$/;
    $('#calendar').find('*').addBack().contents().each(function () {
      if (this.nodeType !== 3) { return; }
      var text = this.nodeValue;
      var trimmed = text.trim();
      var m = re.exec(trimmed);
      if (!m) { return; }
      var h = parseInt(m[1], 10);
      var suffix = h >= 12 ? 'PM' : 'AM';
      var h12 = h % 12 || 12;
      this.nodeValue = text.replace(trimmed, h12 + ':' + m[2] + ' ' + suffix);
    });
  }

  // ============ event pill status markers ============
  // STATUS/CLASS/TRANSP at a glance: cancelled = faded + strikethrough,
  // tentative = dashed outline, transparent ("free") = hollow pill with a
  // ring in the pill's own color, private/confidential = lock icon.
  function decorateEventPills() {
    $('#calendar [data-appointment]').each(function () {
      var pill = $(this);
      var appt = pill.data('appointment');
      var ev = appt && state.eventCache[appt.id];
      if (!ev) { return; }
      var wasFree = pill.hasClass('ev-free');
      // Capture the pill's own color BEFORE the ev-free class clears it.
      var bg = wasFree ? null : pill.css('background-color');
      var status = (ev.status || '').toLowerCase();
      pill.toggleClass('ev-cancelled', status === 'cancelled')
        .toggleClass('ev-tentative', status === 'tentative')
        .toggleClass('ev-free', (ev.transp || '').toLowerCase() === 'transparent')
        .toggleClass('ev-private', ['private', 'confidential'].indexOf((ev.class || '').toLowerCase()) !== -1);
      if (bg && bg !== 'rgba(0, 0, 0, 0)') {
        pill.css('box-shadow', pill.hasClass('ev-free') ? 'inset 0 0 0 2px ' + bg : '');
      }
      if (pill.hasClass('ev-private') && !pill.children('.ev-lock').length) {
        pill.prepend('<i class="bi bi-lock ev-lock" aria-hidden="true"></i>');
      }
    });
  }

  // The "current time" indicator re-renders on its own timer, independent
  // of onAfterLoad, so a one-shot hook misses it. A MutationObserver catches
  // every render path uniformly; the regex above only matches bare 24h
  // text, so re-running it against already-converted text is a no-op
  // (no infinite loop from observing our own writes).
  var amPmObserverStarted = false;
  function startAmPmObserver() {
    if (amPmObserverStarted) { return; }
    var el = document.getElementById('calendar');
    if (!el || !window.MutationObserver) { return; }
    amPmObserverStarted = true;
    var scheduled = false;
    new MutationObserver(function () {
      if (scheduled) { return; }
      scheduled = true;
      requestAnimationFrame(function () {
        scheduled = false;
        convertCalendarTimesToAmPm();
        decorateEventPills();
      });
    }).observe(el, { childList: true, subtree: true, characterData: true });
  }

  // ============ init ============
  $(function () {
    if (!window.jQuery) { return; }
    $('#ev-desc').summernote({ height: 150 });

    api('GET', '/api/auth/me').done(function (user) {
      state.username = user.username || '';
      state.userIsAdmin = !!user.is_admin;
      state.userPrefs = {
        notify_email: user.notify_email !== false,
        notify_sms: user.notify_sms !== false,
        notify_push: user.notify_push !== false,
      };
      // Providers/Credentials/Admin are admin-only (pages redirect, APIs 403).
      if (state.userIsAdmin) { $('#admin-nav-link, #providers-nav-link, #credentials-nav-link').prop('hidden', false); }
      updateTabs();
    });
    loadSubscriptions();
    loadCalendars();

    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () {
        window.location.href = '/login';
      });
    });
  });
})();
