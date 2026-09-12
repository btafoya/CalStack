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
    currentAcl: [],
    currentShares: [],
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

  function modal(id) {
    return bootstrap.Modal.getOrCreateInstance(document.getElementById(id));
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
    var d = new Date(iso);
    return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) +
      'T' + pad(d.getHours()) + ':' + pad(d.getMinutes());
  }

  // datetime-local value (local wall clock) -> UTC ISO instant for the API.
  function localInputToIso(value) {
    return value ? new Date(value).toISOString() : null;
  }

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
      var btn = $('<button class="btn btn-sm btn-outline-danger" type="button">Delete</button>');
      btn.on('click', function () {
        api('DELETE', '/api/attachments/' + a.id).done(function () { loadAttachments(); });
      });
      item.append(btn);
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

  function openEventModal(mode, payload) {
    $('#event-form')[0].reset();
    $('#ev-delete').prop('hidden', mode !== 'edit');
    $('#ev-attachments-section').prop('hidden', mode !== 'edit');
    if (mode === 'edit') {
      state.editingEventId = payload.id;
      state.editingEtag = payload.etag;
      $('#ev-title').val(payload.summary || '');
      $('#ev-start').val(isoToLocalInput(payload.starts_at));
      $('#ev-end').val(isoToLocalInput(payload.ends_at));
      $('#ev-desc').summernote('code', payload.description_html || '');
      loadAttachments();
    } else {
      state.editingEventId = null;
      state.editingEtag = null;
      $('#ev-start').val(payload.start || '');
      $('#ev-end').val(payload.end || '');
      $('#ev-desc').summernote('code', '');
    }
    modal('event-modal').show();
  }

  function saveEvent(e) {
    e.preventDefault();
    if (!state.currentCalendar) { return; }
    var html = $('#ev-desc').summernote('isEmpty') ? null : $('#ev-desc').summernote('code');
    var body = {
      summary: $('#ev-title').val(),
      starts_at: localInputToIso($('#ev-start').val()),
      ends_at: localInputToIso($('#ev-end').val()),
      description_html: html,
      description_text: html ? $('<div>').html(html).text() : null,
    };
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

  // ============ init ============
  $(function () {
    if (!window.jQuery) { return; }
    $('#ev-desc').summernote({ height: 150 });

    api('GET', '/api/auth/me').done(function (user) {
      if (user.is_admin) { $('#admin-nav-link').prop('hidden', false); }
    });
    loadSubscriptions();
    loadCalendars().done(function () {
      $('#calendar').bsCalendar({
        url: function (requestData) { return eventsUrl(requestData); },
        startView: 'week',
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
    });

    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () {
        window.location.href = '/login';
      });
    });
  });
})();
