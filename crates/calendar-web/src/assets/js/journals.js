/* Journals page: dated VJOURNAL entries grouped by month plus an undated
 * notes section (docs/TASKS_JOURNALS_DESIGN.md section 8, the D9 small case)
 * against /api/calendars/{id}/journals and /api/journals. */
(function () {
  'use strict';

  var calId = null;
  var journals = [];
  var editing = null; // journal loaded into the editor modal, or null

  function dateKey(j) {
    // lexical sort key; undated sorts into the undated section anyway
    return j.starts_at ? j.starts_at.slice(0, 10) : (j.start_date || '');
  }

  function loadJournals() {
    if (!calId) { return $.Deferred().resolve(); }
    $('#journal-empty').prop('hidden', true);
    return api('GET', '/api/calendars/' + calId + '/journals').done(function (rows) {
      journals = rows;
      render();
    });
  }

  // Tab pane on the index page: app.js calls load() on tab show and
  // calendar switch; the sidebar selection is the calendar selector.
  window.JournalsPane = {
    _cal: null,
    load: function (cal) {
      if (this._cal === cal.id) { return; }
      this._cal = cal.id;
      if ((cal.components || []).indexOf('VJOURNAL') === -1) {
        calId = null;
        $('#journal-list').empty();
        $('#journal-empty').prop('hidden', false)
          .text('This calendar does not accept journals. Edit the calendar (pencil button in the sidebar) and enable Journals.');
        return;
      }
      calId = cal.id;
      loadJournals();
    },
  };

  function monthLabel(key) {
    var parts = key.split('-');
    return new Date(parseInt(parts[0], 10), parseInt(parts[1], 10) - 1, 1)
      .toLocaleDateString(undefined, { month: 'long', year: 'numeric' });
  }

  function fmtDate(j) {
    return j.starts_at
      ? new Date(j.starts_at).toLocaleDateString()
      : (j.start_date ? new Date(j.start_date + 'T00:00:00').toLocaleDateString() : '');
  }

  function render() {
    var q = $('#journal-search').val().trim().toLowerCase();
    var rows = journals.filter(function (j) {
      return !q || (j.summary || '').toLowerCase().indexOf(q) !== -1;
    });
    var dated = rows.filter(function (j) { return dateKey(j); }).sort(function (a, b) {
      return dateKey(b).localeCompare(dateKey(a)) || String(b.created_at).localeCompare(String(a.created_at));
    });
    var undated = rows.filter(function (j) { return !dateKey(j); });
    var $list = $('#journal-list').empty();
    var byMonth = {};
    dated.forEach(function (j) {
      var key = dateKey(j).slice(0, 7);
      (byMonth[key] = byMonth[key] || []).push(j);
    });
    Object.keys(byMonth).sort().reverse().forEach(function (key) {
      $list.append($('<li class="list-group-item bg-body-secondary fw-semibold"></li>')
        .text(monthLabel(key)));
      byMonth[key].forEach(function (j) { $list.append(journalRow(j)); });
    });
    if (undated.length) {
      $list.append($('<li class="list-group-item bg-body-secondary fw-semibold"></li>')
        .text('Undated notes'));
      undated.forEach(function (j) { $list.append(journalRow(j)); });
    }
    if (!rows.length) {
      $list.append($('<li class="list-group-item text-body-secondary">No journals.</li>'));
    }
  }

  function journalRow(j) {
    var $row = $('<li class="list-group-item"></li>');
    var $head = $('<div class="d-flex align-items-center gap-2"></div>');
    var $summary = $('<button type="button" class="btn btn-link p-0 text-start fw-semibold"></button>')
      .text(j.summary || '(no summary)');
    $summary.on('click', function () { openEditor(j); });
    $head.append($('<div class="flex-grow-1">').append($summary));
    if (j.status) {
      $head.append($('<span class="badge bg-secondary-lt"></span>').text(j.status));
    }
    if (dateKey(j)) {
      $head.append($('<span class="text-body-secondary small"></span>').text(fmtDate(j)));
    }
    var $del = $('<button type="button" class="btn btn-outline-danger btn-sm" aria-label="Delete"><i class="bi bi-trash"></i></button>');
    $del.on('click', function () {
      confirmDialog('Delete journal "' + (j.summary || '') + '"?').done(function () {
        api('DELETE', '/api/journals/' + j.id).done(loadJournals);
      });
    });
    $head.append($del);
    $row.append($head);
    var text = (j.description_text || '').trim();
    if (text) {
      $row.append($('<div class="text-body-secondary small"></div>')
        .text(text.length > 160 ? text.slice(0, 160) + '…' : text));
    }
    return $row;
  }

  function openEditor(j) {
    editing = j || null;
    $('#journal-modal-title').text(j ? 'Edit journal' : 'New journal');
    $('#jv-summary').val(j ? j.summary : '');
    $('#jv-desc').val(j && j.description_text || '');
    $('#jv-date').val(j && j.start_date || '');
    $('#jv-status').val(j && j.status || '');
    $('#jv-delete').prop('hidden', !j);
    bootstrap.Modal.getOrCreateInstance($('#journal-modal')[0]).show();
  }

  function save() {
    var body = {
      summary: $('#jv-summary').val().trim(),
      description_text: $('#jv-desc').val().trim() || null,
      status: $('#jv-status').val() || null,
    };
    if ($('#jv-date').val()) { body.start_date = $('#jv-date').val(); }
    if (editing) {
      return api('PATCH', '/api/journals/' + editing.id, body);
    }
    return api('POST', '/api/calendars/' + calId + '/journals', body);
  }

  $(function () {
    $('#journal-search').on('input', render);

    $('#journal-new-btn').on('click', function () { openEditor(null); });

    $('#journal-form').on('submit', function (ev) {
      ev.preventDefault();
      var $form = $(this);
      if ($form.data('busy')) { return; }
      $form.data('busy', true);
      save().always(function () { $form.data('busy', false); })
      .done(function () {
        bootstrap.Modal.getOrCreateInstance($('#journal-modal')[0]).hide();
        toast('Journal saved.');
        loadJournals();
      });
    });

    $('#jv-delete').on('click', function () {
      if (!editing) { return; }
      confirmDialog('Delete journal "' + (editing.summary || '') + '"?').done(function () {
        api('DELETE', '/api/journals/' + editing.id).done(function () {
          bootstrap.Modal.getOrCreateInstance($('#journal-modal')[0]).hide();
          toast('Journal deleted.');
          loadJournals();
        });
      });
    });

  });
})();