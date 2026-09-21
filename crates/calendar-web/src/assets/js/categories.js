/* Categories pane: manage the category registry against /api/categories. On
 * the index page it lives in a tab; the sidebar's selected calendar sets the
 * scope (app.js calls CategoriesPane.load on tab show / calendar switch).
 * Subscriptions have no categories of their own — they fall back to the
 * tenant-wide view. */
(function () {
  'use strict';

  // Must match calendar_core::CATEGORY_COLORS (Tabler keys).
  var COLORS = ['blue', 'azure', 'indigo', 'purple', 'pink', 'red', 'orange',
    'yellow', 'lime', 'green', 'teal', 'cyan'];

  var calendarId = null;

  function updateScopeUi() {
    $('#cat-scope-note').text(calendarId
      ? "Showing tenant-wide categories plus this calendar's own."
      : 'Showing tenant-wide categories plus categories on calendars you own.');
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
        api('PATCH', '/api/categories/' + row.id, { color: $color.val() }).done(function () {
          toast('Color updated.');
          loadCategories();
        });
      });
      var $rename = $('<button class="btn btn-outline-secondary btn-sm" type="button">Rename</button>');
      $rename.on('click', function () {
        promptDialog('New slug (existing events in scope are re-tagged):', row.slug).done(function (slug) {
          if (!slug || slug === row.slug) { return; }
          api('PATCH', '/api/categories/' + row.id, { slug: slug }).done(function () {
            toast('Category renamed.');
            loadCategories();
          });
        });
      });
      var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
      $del.on('click', function () {
        confirmDialog('Delete category "' + row.name + '"? Events keep the tag as free text.').done(function () {
          api('DELETE', '/api/categories/' + row.id).done(function () {
            toast('Category deleted.');
            loadCategories();
          });
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

  window.CategoriesPane = {
    _cal: null,
    load: function (cal) {
      var cid = cal.readOnly ? null : cal.id;
      if (this._cal === cid) { return; }
      this._cal = cid;
      calendarId = cid;
      updateScopeUi();
      loadCategories();
    },
  };

  $(function () {
    COLORS.forEach(function (c) {
      $('<option>').val(c).text(c).appendTo($('#cat-color'));
    });
    updateScopeUi();
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
        toast('Category added.');
        loadCategories();
      });
    });
  });
})();