/* Tasks page: VTODO lists per calendar (docs/TASKS_JOURNALS_DESIGN.md section
 * 8) against /api/calendars/{id}/tasks and /api/tasks. Open tasks first,
 * subtasks indented under their parent. Recurring tasks cannot be completed
 * from here yet: single-occurrence completion needs an occurrence, which the
 * calendar view will supply later. */
(function () {
  'use strict';

  var calId = null;
  var tasks = [];
  var editing = null; // task loaded into the edit modal, or null

  function isDone(t) {
    return !!t.completed_at || t.status === 'COMPLETED' || t.status === 'CANCELLED';
  }

  // due key for ordering; undated sorts last, all-day due compares as its date
  function dueKey(t) {
    if (t.due_at) { return t.due_at; }
    if (t.due_date) { return t.due_date; }
    return '9999';
  }

  function cmp(a, b) {
    var da = isDone(a) ? 1 : 0;
    var db = isDone(b) ? 1 : 0;
    if (da !== db) { return da - db; }
    if (da) { return String(b.completed_at || '').localeCompare(String(a.completed_at || '')); }
    return dueKey(a).localeCompare(dueKey(b));
  }

  function loadTasks() {
    if (!calId) { return $.Deferred().resolve(); }
    $('#task-empty').prop('hidden', true);
    return api('GET', '/api/calendars/' + calId + '/tasks').done(function (rows) {
      tasks = rows;
      render();
    });
  }

  // Tab pane on the index page: the sidebar selection is the calendar
  // selector; app.js calls load() on tab show and calendar switch.
  window.TasksPane = {
    _cal: null,
    load: function (cal) {
      if (this._cal === cal.id) { return; }
      this._cal = cal.id;
      if ((cal.components || []).indexOf('VTODO') === -1) {
        calId = null;
        $('#task-list').empty();
        $('#task-empty').prop('hidden', false)
          .text('This calendar does not accept tasks. Edit the calendar (pencil button in the sidebar) and enable Tasks.');
        return;
      }
      calId = cal.id;
      loadTasks();
    },
  };

  function render() {
    var q = $('#task-search').val().trim().toLowerCase();
    var mode = $('#task-status-filter').val();
    var rows = tasks.filter(function (t) {
      if (mode === 'open' && isDone(t)) { return false; }
      if (mode === 'done' && !isDone(t)) { return false; }
      if (q && (t.summary || '').toLowerCase().indexOf(q) === -1) { return false; }
      return true;
    });
    var byUid = {};
    rows.forEach(function (t) { byUid[t.uid] = t; });
    function children(uid) {
      return rows.filter(function (t) { return t.parent_uid === uid; }).sort(cmp);
    }
    var $list = $('#task-list').empty();
    function addTree(t, depth) {
      $list.append(taskRow(t, depth));
      children(t.uid).forEach(function (c) { addTree(c, depth + 1); });
    }
    rows.filter(function (t) { return !t.parent_uid || !byUid[t.parent_uid]; })
      .sort(cmp)
      .forEach(function (t) { addTree(t, 0); });
    if (!rows.length) {
      $list.append($('<li class="list-group-item text-body-secondary">No tasks.</li>'));
    }
  }

  function fmtDue(t) {
    if (t.due_at) { return new Date(t.due_at).toLocaleString(); }
    if (t.due_date) { return new Date(t.due_date + 'T00:00:00').toLocaleDateString(); }
    return '';
  }

  function taskRow(t, depth) {
    var $row = $('<li class="list-group-item d-flex align-items-center gap-2"></li>')
      .css('margin-left', depth ? '1.5rem' : '0');
    var $check = $('<input type="checkbox" class="form-check-input mt-0" aria-label="Completed">')
      .prop('checked', isDone(t))
      .prop('disabled', !!t.rrule)
      .attr('title', t.rrule ? 'Recurring tasks cannot be completed here yet' : 'Toggle completed');
    $check.on('change', function () {
      api('POST', '/api/tasks/' + t.id + '/' + ($check.prop('checked') ? 'complete' : 'reopen'))
        .done(loadTasks);
    });
    $row.append($check);
    var $summary = $('<button type="button" class="btn btn-link p-0 text-start'
      + (isDone(t) ? ' text-decoration-line-through text-body-secondary' : '') + '"></button>')
      .text(t.summary || '(no summary)');
    $summary.on('click', function () { openEditor(t); });
    $row.append($('<div class="flex-grow-1">').append($summary));
    if (t.rrule) {
      $row.append('<span class="badge text-bg-secondary">recurring</span>');
    }
    if (t.is_overdue) {
      $row.append('<span class="badge text-bg-danger">overdue</span>');
    }
    if (t.due_at || t.due_date) {
      $row.append($('<span class="text-body-secondary small"></span>').text(fmtDue(t)));
    }
    if (t.percent_complete != null && t.percent_complete > 0 && !isDone(t)) {
      $row.append($('<span class="badge bg-secondary-lt"></span>').text(t.percent_complete + '%'));
    }
    if (t.subtasks_count > 0) {
      $row.append($('<span class="badge bg-secondary-lt"></span>').text(t.subtasks_count + ' subtasks'));
    }
    var $del = $('<button type="button" class="btn btn-outline-danger btn-sm" aria-label="Delete"><i class="bi bi-trash"></i></button>');
    $del.on('click', function () {
      confirmDialog('Delete task "' + (t.summary || '') + '"? Its subtasks go with it.').done(function () {
        api('DELETE', '/api/tasks/' + t.id).done(loadTasks);
      });
    });
    $row.append($del);
    return $row;
  }

  function toLocalInput(iso) {
    var d = new Date(iso);
    function p(n) { return (n < 10 ? '0' : '') + n; }
    return d.getFullYear() + '-' + p(d.getMonth() + 1) + '-' + p(d.getDate())
      + 'T' + p(d.getHours()) + ':' + p(d.getMinutes());
  }

  function syncDueMode() {
    var allDay = $('#tk-all-day').prop('checked');
    $('#tk-due-date').prop('hidden', !allDay);
    $('#tk-due-at').prop('hidden', allDay);
  }

  function openEditor(t) {
    editing = t || null;
    $('#task-modal-title').text(t ? 'Edit task' : 'New task');
    $('#tk-summary').val(t ? t.summary : '');
    $('#tk-desc').val(t && t.description_text || '');
    $('#tk-all-day').prop('checked', !!(t && t.due_date));
    $('#tk-due-date').val(t && t.due_date || '');
    $('#tk-due-at').val(t && t.due_at ? toLocalInput(t.due_at) : '');
    syncDueMode();
    $('#tk-priority').val(t && t.priority != null ? t.priority : '');
    $('#tk-status').val(t && t.status || '');
    $('#tk-percent').val(t && t.percent_complete != null ? t.percent_complete : '');
    $('#tk-categories').val(t && (t.categories || []).join(', ') || '');
    fillParentPicker(t);
    $('#tk-delete').prop('hidden', !t);
    bootstrap.Modal.getOrCreateInstance($('#task-modal')[0]).show();
  }

  // UIDs in this calendar that sit under the given task (itself included) —
  // none of them may become its parent, or the tree gets a cycle.
  function descendantsOf(t) {
    var blocked = {};
    blocked[t.uid] = true;
    var frontier = [t.uid];
    while (frontier.length) {
      var uid = frontier.pop();
      tasks.forEach(function (x) {
        if (x.parent_uid === uid && !blocked[x.uid]) {
          blocked[x.uid] = true;
          frontier.push(x.uid);
        }
      });
    }
    return blocked;
  }

  function fillParentPicker(t) {
    var blocked = t ? descendantsOf(t) : {};
    var $sel = $('#tk-parent-uid').empty().append('<option value="">(no parent)</option>');
    tasks.forEach(function (x) {
      if (blocked[x.uid]) { return; }
      $sel.append($('<option>').val(x.uid).text(x.summary || '(no summary)'));
    });
    if (t && t.parent_uid && !tasks.some(function (x) { return x.uid === t.parent_uid; })) {
      // Parent lives in another calendar (or was filtered out); keep it
      // selectable so saving doesn't silently detach it.
      $sel.append($('<option>').val(t.parent_uid).text(t.parent_uid));
    }
    $sel.val(t && t.parent_uid || '');
  }

  function readNumber($el, max) {
    var v = $el.val().trim();
    if (v === '') { return null; }
    var n = parseInt(v, 10);
    if (isNaN(n) || n < 0 || n > max) { return null; }
    return n;
  }

  function saveEditor() {
    if (!editing) { return $.Deferred().resolve(); }
    var body = {
      summary: $('#tk-summary').val().trim(),
      description_text: $('#tk-desc').val().trim() || null,
      priority: readNumber($('#tk-priority'), 9),
      status: $('#tk-status').val() || null,
      percent_complete: readNumber($('#tk-percent'), 100),
      categories: $('#tk-categories').val().split(',').map(function (s) {
        return s.trim();
      }).filter(Boolean),
      parent_uid: $('#tk-parent-uid').val().trim() || null,
    };
    if ($('#tk-all-day').prop('checked')) {
      if ($('#tk-due-date').val()) { body.due_date = $('#tk-due-date').val(); }
    } else if ($('#tk-due-at').val()) {
      body.due_at = new Date($('#tk-due-at').val()).toISOString();
    }
    api('PATCH', '/api/tasks/' + editing.id, body).done(function () {
      bootstrap.Modal.getOrCreateInstance($('#task-modal')[0]).hide();
      toast('Task saved.');
      loadTasks();
    });
  }

  $(function () {
    $('#task-search').on('input', render);
    $('#task-status-filter').on('change', render);
    $('#tk-all-day').on('change', syncDueMode);

    // quick add: summary only, into the selected calendar
    $('#task-quick-add').on('submit', function (ev) {
      ev.preventDefault();
      var summary = $('#task-quick-summary').val().trim();
      if (!summary || !calId) { return; }
      var $form = $(this);
      if ($form.data('busy')) { return; }
      $form.data('busy', true);
      api('POST', '/api/calendars/' + calId + '/tasks', { summary: summary })
        .always(function () { $form.data('busy', false); })
        .done(function () {
          $('#task-quick-summary').val('');
          toast('Task added.');
          loadTasks();
        });
    });

    $('#task-form').on('submit', function (ev) {
      ev.preventDefault();
      var $form = $(this);
      if ($form.data('busy')) { return; }
      $form.data('busy', true);
      saveEditor().always(function () { $form.data('busy', false); });
    });

    $('#tk-delete').on('click', function () {
      if (!editing) { return; }
      confirmDialog('Delete task "' + (editing.summary || '') + '"? Its subtasks go with it.').done(function () {
        api('DELETE', '/api/tasks/' + editing.id).done(function () {
          bootstrap.Modal.getOrCreateInstance($('#task-modal')[0]).hide();
          toast('Task deleted.');
          loadTasks();
        });
      });
    });
  });
})();