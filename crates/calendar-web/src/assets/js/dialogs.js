/* Dialog + toast helpers over the vendored SweetAlert2 (sweetalert2.all.min.js
 * bundles its own styles). confirmDialog/promptDialog keep the jQuery-promise
 * signatures every caller already uses: they resolve only on OK, never on
 * cancel/dismiss, so .done() chains stay no-op on cancel like before. */
(function () {
  'use strict';

  var BUTTONS = {
    buttonsStyling: false,
    confirmButtonColor: undefined,
    customClass: {
      confirmButton: 'btn btn-primary',
      cancelButton: 'btn btn-secondary',
    },
    // ponytail: returnFocus off so a confirm opened from inside a Bootstrap
    // modal (attachment rows, discard-changes) doesn't fight Bootstrap's
    // focus restore when it closes.
    returnFocus: false,
  };

  window.confirmDialog = function (message, options) {
    var d = $.Deferred();
    Swal.fire($.extend({
      title: message,
      icon: 'warning',
      showCancelButton: true,
      confirmButtonText: 'OK',
      cancelButtonText: 'Cancel',
    }, BUTTONS, options || {})).then(function (r) {
      if (r.isConfirmed) { d.resolve(true); }
    });
    return d.promise();
  };

  window.promptDialog = function (message, defaultValue) {
    var d = $.Deferred();
    Swal.fire($.extend({
      title: message,
      input: 'text',
      inputValue: defaultValue || '',
      showCancelButton: true,
      confirmButtonText: 'OK',
      cancelButtonText: 'Cancel',
    }, BUTTONS)).then(function (r) {
      if (r.isConfirmed) { d.resolve(r.value || ''); }
    });
    return d.promise();
  };

  window.errorDialog = function (message) {
    return Swal.fire($.extend({
      icon: 'error',
      title: message || 'Something went wrong',
    }, BUTTONS));
  };

  window.toast = function (message, icon) {
    Swal.fire({
      toast: true,
      position: 'top-end',
      icon: icon || 'success',
      title: message,
      timer: 2500,
      timerProgressBar: true,
      showConfirmButton: false,
    });
  };
})();