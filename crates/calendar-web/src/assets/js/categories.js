/* Categories page: manage the category registry against /api/categories. */
(function () {
  'use strict';

  // Must match calendar_core::CATEGORY_COLORS (Tabler keys).
  var COLORS = ['blue', 'azure', 'indigo', 'purple', 'pink', 'red', 'orange',
    'yellow', 'lime', 'green', 'teal', 'cyan'];

  var calendarId = new URLSearchParams(window.location.search).get('calendar_id');

  function updateScopeUi() {
    var url = new URL(window.location);
    if (calendarId) { url.searchParams.set('calendar_id', calendarId); } else { url.searchParams.delete('calendar_id'); }
    window.history.replaceState(null, '', url);
    $('#cat-scope-note').text(calendarId
      ? 'Showing tenant-wide categories plus this calendar\'s own.'
      : 'Showing tenant-wide categories plus categories on calendars you own.');
  }

  function loadCalendarOptions() {
    return api('GET', '/api/calendars').done(function (list) {
      var $select = $('#cat-calendar-select');
      list.forEach(function (cal) {
        $('<option>').val(cal.id).text(cal.name).appendTo($select);
      });
      if (calendarId) { $select.val(calendarId); }
    });
  }

  function renderRows(rows) {
    var $rows = $('#cat-rows').empty();
    rows.forEach(function (row) {
      var scope = row.calendar_id ? 'This calendar' : 'All calendars';
      var $color = $('<select class="form-select form-select-sm w-auto"></select>');
      COLORS.forEach(function (c) {
        $('<option>').val(c).text(c).prop('selected', c === row.color).appendTo($color);
      });
      $color.on('change', function () {
        api('PATCH', '/api/categories/' + row.id, { color: $color.val() }).done(loadCategories);
      });
      var $rename = $('<button class="btn btn-outline-secondary btn-sm" type="button">Rename</button>');
      $rename.on('click', function () {
        promptDialog('New slug (existing events in scope are re-tagged):', row.slug).done(function (slug) {
          if (!slug || slug === row.slug) { return; }
          api('PATCH', '/api/categories/' + row.id, { slug: slug }).done(loadCategories);
        });
      });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () {
        confirmDialog('Delete category "' + row.name + '"? Events keep the tag as free text.').done(function () {
          api('DELETE', '/api/categories/' + row.id).done(loadCategories);
        });
      });
      $('<tr>')
        .append($('<td>').append($('<span class="badge">').addClass('bg-' + row.color + '-lt').text(row.name)))
        .append($('<td>').text(row.name))
        .append($('<td>').text(row.slug))
        .append($('<td>').text(scope))
        .append($('<td class="text-end">').append($rename).append(' ').append($del))
        .appendTo($rows);
    });
  }

  function loadCategories() {
    var url = '/api/categories' + (calendarId ? '?calendar_id=' + calendarId : '');
    return api('GET', url).done(renderRows);
  }

  $(function () {
    updateScopeUi();
    COLORS.forEach(function (c) {
      $('<option>').val(c).text(c).appendTo($('#cat-color'));
    });
    loadCalendarOptions();
    loadCategories();
    $('#cat-calendar-select').on('change', function () {
      calendarId = $(this).val() || null;
      updateScopeUi();
      loadCategories();
    });
    $('#cat-form').on('submit', function (ev) {
      ev.preventDefault();
      var global = !calendarId || $('#cat-global').is(':checked');
      api('POST', '/api/categories', {
        calendar_id: global ? null : calendarId,
        slug: $('#cat-slug').val().trim(),
        name: $('#cat-name').val().trim(),
        color: $('#cat-color').val(),
      }).done(function () {
        $('#cat-form')[0].reset();
        loadCategories();
      });
    });

    api('GET', '/api/auth/me').done(function (user) {
      if (user.is_admin) { $('#admin-nav-link, #rules-link, #providers-nav-link, #credentials-nav-link').prop('hidden', false); }
    });
    $('#account-btn').on('click', function () { window.location.href = '/'; });
    $('#logout-btn').on('click', function () {
      api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
    });
  });
})();