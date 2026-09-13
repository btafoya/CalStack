/* Calendar web app: jQuery 4 against /api. Progressive enhancement shell. */
(function () {
  'use strict';

  // ponytail: jQuery 4 dropped the deprecated $.now static; jquery-migrate
  // 3.x (1.x-3.x warnings only) doesn't restore it, and summernote-bs5 still
  // calls it internally. Shim just this one static rather than downgrading
  // jQuery or patching the vendored summernote bundle.
  if (!$.now) { $.now = Date.now; }

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
    currentAcl: [],
    currentShares: [],
    calendarActivated: false,
    categoryRegistry: [],
    eventCategories: [],
  };

  function api(method, url, data, extraHeaders) {
    var headers = method !== 'GET' ? { 'X-CSRF-Token': state.csrf } : {};
    if (extraHeaders) { $.extend(headers, extraHeaders); }
    return $.ajax({
      method: method,
      url: url,
      data: data !== undefined && data !== null ? JSON.stringify(data) : null,
      contentType: 'application/json',
      headers: headers,
    }).fail(function (xhr) {
      if (xhr.status === 401) { window.location.href = '/login'; return; }
      window.alert((xhr.responseJSON && xhr.responseJSON.error) || 'Request failed');
    });
  }

  // ============ categories (checkbox list over the registry) ============
  // Grid hex values for the Tabler palette keys (calendar grid needs real
  // colors, not the bg-*-lt CSS tokens the badges use).
  var CATEGORY_HEX = {
    blue: '#206bc4', azure: '#4299e1', indigo: '#4263eb', purple: '#ae3ec9',
    pink: '#d6336c', red: '#d63939', orange: '#f76707', yellow: '#f7b731',
    lime: '#74b816', green: '#2fb344', teal: '#0ca678', cyan: '#17a2b8',
  };

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

  // ============ calendars ============
  function updateRulesLink() {
    var cal = state.currentCalendar;
    $('#rules-link').attr('href', cal ? '/rules?calendar_id=' + cal.id : '/rules');
  }

  // Only render the calendar widget once a calendar is actually selected;
  // otherwise show a placeholder in its place.
  function updateCalendarVisibility() {
    var selected = !!state.currentCalendar;
    $('#calendar').prop('hidden', !selected);
    $('#calendar-empty').prop('hidden', selected);
  }

  function selectCalendar(cal) {
    state.currentCalendar = cal;
    $('#cal-list li').removeClass('active');
    $('#cal-list li[data-id="' + cal.id + '"]').addClass('active');
    updateRulesLink();
    updateCalendarVisibility();
    loadCategoryRegistry(cal.id);
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
        openEventModal('create', {
          start: partToLocalInput(data && data.start, '09:00'),
          end: partToLocalInput(data && data.end, '10:00'),
        });
      },
      // ponytail: editing/deleting a recurring occurrence acts on the whole
      // series (the shared master event) — per-occurrence exceptions need
      // their own RECURRENCE-ID UI, add when single-instance edits matter.
      onEdit: function (appointment) {
        var ev = state.eventCache[appointment.id];
        if (ev) { openEventModal('edit', ev); }
      },
      onDelete: function (appointment) {
        var ev = state.eventCache[appointment.id];
        if (ev && window.confirm('Delete "' + (ev.summary || 'this event') + '"?')) {
          deleteEvent(ev.id, ev.etag);
        }
      },
    });
    startAmPmObserver();
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
      renderCalList(list);
      updateRulesLink();
      updateCalendarVisibility();
    });
  }

  function renameCalendar(cal) {
    var name = window.prompt('Calendar name:', cal.name);
    if (!name || name === cal.name) { return; }
    api('PATCH', '/api/calendars/' + cal.id, { name: name }).done(loadCalendars);
  }

  function deleteCalendar(cal) {
    if (!window.confirm('Delete calendar "' + cal.name + '"? This cannot be undone.')) { return; }
    api('DELETE', '/api/calendars/' + cal.id).done(function () {
      if (state.currentCalendar && state.currentCalendar.id === cal.id) { state.currentCalendar = null; }
      loadCalendars();
    });
  }

  function renderCalList(list) {
    $('#cal-list').empty();
    list.forEach(function (cal) {
      var item = $('<li class="list-group-item list-group-item-action d-flex justify-content-between align-items-center">')
        .attr('data-id', cal.id);
      item.append($('<span>').text(cal.name + ' (' + cal.my_capability + ')'));
      item.on('click', function () { selectCalendar(cal); });
      if (cal.my_capability === 'owner' || cal.my_capability === 'read_write') {
        var btns = $('<span class="btn-group btn-group-sm">');
        btns.append(
          $('<button class="btn btn-outline-secondary" type="button" title="Rename"><i class="bi bi-pencil"></i></button>')
            .on('click', function (e) { e.stopPropagation(); renameCalendar(cal); })
        );
        if (cal.my_capability === 'owner') {
          btns.append(
            $('<button class="btn btn-outline-danger" type="button" title="Delete"><i class="bi bi-trash"></i></button>')
              .on('click', function (e) { e.stopPropagation(); deleteCalendar(cal); })
          );
        }
        item.append(btns);
      }
      $('#cal-list').append(item);
    });
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
      color: categoryColorHex(ev) || (state.currentCalendar && state.currentCalendar.color) || '#066fd1',
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
    return fetch('/api/calendars/' + cal.id + '/occurrences?' + params)
      .then(function (r) { return r.json(); })
      .then(function (rows) {
        return rows.map(function (row) {
          var ev = row.event || row;
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
          window.alert(
            meta.filename + '\n' +
            'Type: ' + meta.content_type + '\n' +
            'Size: ' + formatBytes(meta.byte_size) + '\n' +
            'SHA-256: ' + meta.sha256 + '\n' +
            'Uploaded: ' + meta.created_at
          );
        });
      });
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Delete</button>');
      btn.on('click', function () {
        api('DELETE', '/api/attachments/' + a.id).done(function () { loadAttachments(); });
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
        loadAttachments();
      });
    };
    reader.readAsDataURL(file);
  });

  function renderAttendees() {
    var list = $('#ev-attendees').empty();
    state.editingAttendees.forEach(function (a, i) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">');
      item.append($('<span>').text(a.display_name ? a.display_name + ' <' + a.email + '>' : a.email));
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
  // email entry) — search /api/contacts/autocomplete, click a result to add
  // it immediately.
  function hideAttendeeResults() {
    $('#ev-attendee-results').empty().prop('hidden', true);
  }

  function renderAttendeeResults(list) {
    var $results = $('#ev-attendee-results').empty();
    (list || []).forEach(function (c) {
      var email = c.emails && c.emails[0] && c.emails[0].email;
      if (!email) { return; }
      var already = state.editingAttendees.some(function (a) {
        return a.email.toLowerCase() === email.toLowerCase();
      });
      $('<button type="button" class="list-group-item list-group-item-action py-1"></button>')
        .toggleClass('disabled', already)
        .append($('<div>').text(c.full_name || email))
        .append($('<small class="text-body-secondary d-block">').text(email + (c.directory ? ' · directory' : '')))
        .on('click', function () {
          if (already) { return; }
          state.editingAttendees.push({
            email: email,
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

  function openEventModal(mode, payload) {
    $('#event-form')[0].reset();
    $('#ev-delete').prop('hidden', mode !== 'edit');
    $('#ev-attachments-section').prop('hidden', mode !== 'edit');
    state.editingAttendees = [];
    state.placeLocation = null;
    pickedPlace = null;
    hidePlaceMenu();
    if (mode === 'edit') {
      state.editingEventId = payload.id;
      state.editingEtag = payload.etag;
      state.editingRruleUnknown = !applyRruleToForm(payload.rrule);
      $('#ev-title').val(payload.summary || '');
      setTimeControls('start', isoToLocalInput(payload.starts_at || payload.start_date));
      setTimeControls('end', isoToLocalInput(payload.ends_at || payload.end_date));
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
          email: a.email, display_name: a.display_name || null,
          contact_id: a.contact_id || null, user_id: a.user_id || null,
        };
      });
      $('#ev-desc').summernote('code', payload.description_html || '');
      loadAttachments();
    } else {
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
    if (!state.editingRruleUnknown) { body.rrule = buildRrule(); }
    var req = state.editingEventId
      ? api('PATCH', '/api/events/' + state.editingEventId, body,
          state.editingEtag ? { 'If-Match': state.editingEtag } : {})
      : api('POST', '/api/calendars/' + state.currentCalendar.id + '/events', body);
    req.done(function () {
      modal('event-modal').hide();
      $('#calendar').bsCalendar('refresh');
    });
  }

  function deleteEvent(id, etag) {
    api('DELETE', '/api/events/' + id, null, etag ? { 'If-Match': etag } : {}).done(function () {
      modal('event-modal').hide();
      $('#calendar').bsCalendar('refresh');
    });
  }

  $('#event-form').on('submit', saveEvent);
  $('#ev-delete').on('click', function () {
    if (!state.editingEventId) { return; }
    if (!window.confirm('Delete this event?')) { return; }
    deleteEvent(state.editingEventId, state.editingEtag);
  });

  // ============ sharing / ACL ============
  function renderAcl() {
    var tbody = $('#acl-rows').empty();
    state.currentAcl.forEach(function (entry) {
      var row = $('<tr>');
      row.append($('<td>').text(entry.user_id));
      row.append($('<td>').text(entry.capability));
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
      .done(function () { loadAcl(); });
  }

  $('#acl-add').on('click', function () {
    var userId = $('#acl-user').val().trim();
    if (!userId) { return; }
    state.currentAcl.push({ user_id: userId, capability: $('#acl-cap').val(), can_manage_acl: false });
    saveAcl();
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
        window.prompt('Share link (copy now, shown only once):', url);
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

  // ============ account (password change) ============
  $('#account-btn').on('click', function () {
    $('#account-current-password, #account-new-password, #account-new-password-confirm').val('');
    $('#account-password-msg').text('');
    modal('account-modal').show();
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
        window.alert('Password changed. Your other sessions have been signed out.');
      });
  });

  // ============ subscriptions (read-only calendars shared by others) ============
  function renderSubscriptions(rows) {
    var list = $('#sub-list').empty();
    rows.forEach(function (s) {
      var item = $('<li class="list-group-item d-flex justify-content-between align-items-center">');
      item.append($('<span>').text(s.calendar_name));
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">&times;</button>');
      btn.on('click', function () {
        api('DELETE', '/api/subscriptions/' + s.id).done(loadSubscriptions);
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
      loadSubscriptions();
    });
  });

  // ============ search ============
  // ponytail: results are read-only (summary/time only); opening a hit in the
  // editor would need switching the selected calendar first — add if search
  // needs to jump straight into editing.
  function renderSearchResults(rows) {
    var list = $('#search-results').empty();
    if (!rows.length) { list.append($('<li class="list-group-item text-body-secondary">').text('No matches.')); }
    rows.forEach(function (e) {
      var when = e.starts_at || e.start_date || '';
      list.append(
        $('<li class="list-group-item">').text((e.summary || '(untitled)') + ' — ' + when)
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
      // Rules/Providers/Credentials/Admin are admin-only (pages redirect, APIs 403).
      if (user.is_admin) { $('#admin-nav-link, #rules-link, #providers-nav-link, #credentials-nav-link').prop('hidden', false); }
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
