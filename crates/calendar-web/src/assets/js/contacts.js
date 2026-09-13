/* Contacts page: address book management (personal books + read-only tenant
 * directory) and contact CRUD against /api/addressbooks + /api/contacts. */
(function () {
  'use strict';

  var currentBookId = null;
  var currentBookKind = 'personal';
  var allContacts = [];

  function loadBooks() {
    return api('GET', '/api/addressbooks').done(function (books) {
      var $list = $('#ab-list').empty();
      books.forEach(function (b) {
        var $item = $('<button type="button" class="list-group-item list-group-item-action d-flex justify-content-between align-items-center"></button>')
          .text(b.name)
          .toggleClass('active', b.id === currentBookId);
        if (b.kind === 'directory') {
          $item.append($('<span class="badge bg-secondary-lt">read-only</span>'));
        } else {
          var $del = $('<i class="bi bi-trash text-danger ms-2" title="Delete"></i>');
          $del.on('click', function (ev) {
            ev.stopPropagation();
            confirmDialog('Delete address book "' + b.name + '"? Its contacts go with it.').done(function () {
              api('DELETE', '/api/addressbooks/' + b.id).done(loadBooks);
            });
          });
          $item.append($del);
        }
        $item.on('click', function () {
          currentBookId = b.id;
          currentBookKind = b.kind;
          $('#ab-current-name').text(b.name);
          $('#ct-form').prop('hidden', b.kind === 'directory');
          loadContacts();
          loadBooks();
        });
        $list.append($item);
      });
      if (!currentBookId && books.length) {
        var personal = books.find(function (b) { return b.kind === 'personal'; }) || books[0];
        currentBookId = personal.id;
        currentBookKind = personal.kind;
        $('#ab-current-name').text(personal.name);
        $('#ct-form').prop('hidden', personal.kind === 'directory');
        loadContacts();
      }
    });
  }

  function renderRows(rows) {
    var $rows = $('#ct-rows').empty();
    rows.forEach(function (c) {
      var email = (c.emails && c.emails[0] && c.emails[0].email) || '';
      var tel = (c.tels && c.tels[0] && c.tels[0].number) || '';
      var $tr = $('<tr>')
        .append($('<td>').text(c.full_name))
        .append($('<td>').text(c.org || ''))
        .append($('<td>').text(email))
        .append($('<td>').text(tel));
      if (!c.directory) {
        var $del = $('<button class="btn btn-outline-danger btn-sm" type="button">Delete</button>');
        $del.on('click', function () {
          confirmDialog('Delete contact "' + c.full_name + '"?').done(function () {
            api('DELETE', '/api/contacts/' + c.id).done(loadContacts);
          });
        });
        $tr.append($('<td class="text-end">').append($del));
      } else {
        $tr.append($('<td>'));
      }
      $rows.append($tr);
    });
  }

  function loadContacts() {
    if (!currentBookId) { return; }
    return api('GET', '/api/addressbooks/' + currentBookId + '/contacts').done(function (rows) {
      allContacts = rows;
      applySearch();
    });
  }

  function applySearch() {
    var q = $('#ct-search').val().trim().toLowerCase();
    if (!q) { renderRows(allContacts); return; }
    renderRows(allContacts.filter(function (c) {
      return (c.full_name || '').toLowerCase().indexOf(q) !== -1
        || (c.org || '').toLowerCase().indexOf(q) !== -1
        || (c.emails || []).some(function (e) { return e.email.toLowerCase().indexOf(q) !== -1; });
    }));
  }

  $(function () {
    loadBooks();

    $('#ct-search').on('input', applySearch);

    $('#ab-new').on('click', function () {
      promptDialog('Address book name:').done(function (name) {
        if (!name) { return; }
        var slug = name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'book';
        api('POST', '/api/addressbooks', { slug: slug, name: name }).done(loadBooks);
      });
    });

    $('#ct-form').on('submit', function (ev) {
      ev.preventDefault();
      if (!currentBookId || currentBookKind === 'directory') { return; }
      var emails = [];
      var email = $('#ct-email').val().trim();
      if (email) { emails.push({ email: email, kind: 'work', is_primary: true }); }
      var tels = [];
      var tel = $('#ct-tel').val().trim();
      if (tel) { tels.push({ number: tel, is_mobile: $('#ct-mobile').is(':checked'), is_primary: true }); }
      api('POST', '/api/addressbooks/' + currentBookId + '/contacts', {
        full_name: $('#ct-name').val().trim(),
        org: $('#ct-org').val().trim() || null,
        emails: emails,
        tels: tels,
      }).done(function () {
        $('#ct-form')[0].reset();
        $('#ct-mobile').prop('checked', true);
        loadContacts();
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
