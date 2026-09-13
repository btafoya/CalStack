/* Shared confirm/prompt modal replacing window.confirm/window.prompt with
 * in-app UI (native dialogs read as OS chrome, not the app, and can't be
 * themed). Requires jQuery + bootstrap.bundle.min.js already loaded. */
(function () {
  'use strict';

  function ensureModal() {
    var $el = $('#dialog-modal');
    if ($el.length) { return $el; }
    return $(
      '<div class="modal fade" id="dialog-modal" tabindex="-1" aria-hidden="true" data-bs-backdrop="static" data-bs-keyboard="false">' +
      '<div class="modal-dialog"><div class="modal-content">' +
      '<div class="modal-header"><h2 class="modal-title h6" id="dialog-modal-msg"></h2></div>' +
      '<div class="modal-body"><input class="form-control" id="dialog-modal-input" hidden></div>' +
      '<div class="modal-footer">' +
      '<button class="btn btn-secondary" type="button" data-bs-dismiss="modal">Cancel</button>' +
      '<button class="btn btn-primary" type="button" id="dialog-modal-ok">OK</button>' +
      '</div></div></div></div>'
    ).appendTo(document.body);
  }

  function showDialog(message, withInput, defaultValue) {
    var $el = ensureModal();
    var bsModal = bootstrap.Modal.getOrCreateInstance($el[0]);
    $('#dialog-modal-msg').text(message);
    var $input = $('#dialog-modal-input').prop('hidden', !withInput).val(withInput ? (defaultValue || '') : '');
    var dfd = $.Deferred();
    $('#dialog-modal-ok').off('click').on('click', function () {
      dfd.resolve(withInput ? $input.val() : true);
      bsModal.hide();
    });
    $el.off('shown.bs.modal').on('shown.bs.modal', function () {
      if (withInput) { $input.trigger('focus').trigger('select'); }
    });
    bsModal.show();
    return dfd.promise();
  }

  window.confirmDialog = function (message) { return showDialog(message, false); };
  window.promptDialog = function (message, defaultValue) { return showDialog(message, true, defaultValue); };
})();
