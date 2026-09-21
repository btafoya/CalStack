/* Credentials page: API tokens + CalDAV app passwords + 2FA (TOTP) + passkeys. */
$(function () {
  function showSecret(value) {
    $('#secret-value').text(value);
    $('#secret-banner').removeClass('d-none');
    window.scrollTo(0, 0);
  }

  function esc(text) {
    return $('<span>').text(text).html();
  }

  function fmtDate(value) {
    return value ? value.replace('T', ' ').slice(0, 16) : '—';
  }

  function revokeBtn(kind, id) {
    return '<button class="btn btn-outline-danger btn-sm" data-kind="' + kind + '" data-id="' + id + '">Revoke</button>';
  }

  function renderTokens(list) {
    $('#token-rows').html(list.map(function (t) {
      var scopes = (t.scopes && t.scopes.length) ? t.scopes.join(', ') : 'full';
      return '<tr><td>' + esc(t.name) + '</td><td>' + esc(scopes) + '</td><td>'
        + esc(fmtDate(t.created_at)) + '</td><td>' + esc(fmtDate(t.expires_at)) + '</td><td>'
        + revokeBtn('token', t.id) + '</td></tr>';
    }).join('') || '<tr><td colspan="5" class="text-body-secondary">No tokens.</td></tr>');
  }

  function renderPasswords(list) {
    $('#ap-rows').html(list.map(function (p) {
      return '<tr><td>' + esc(p.name) + '</td><td>' + esc(fmtDate(p.created_at))
        + '</td><td>' + esc(fmtDate(p.last_used_at)) + '</td><td>' + esc(fmtDate(p.expires_at))
        + '</td><td>' + revokeBtn('app-password', p.id) + '</td></tr>';
    }).join('') || '<tr><td colspan="5" class="text-body-secondary">No app passwords.</td></tr>');
  }

  function renderPasskeys(list) {
    $('#pk-rows').html(list.map(function (p) {
      return '<tr><td>' + esc(p.name || 'Passkey') + '</td><td>' + esc(fmtDate(p.created_at))
        + '</td><td>' + esc(fmtDate(p.last_used_at)) + '</td><td>' + revokeBtn('passkey', p.id)
        + '</td></tr>';
    }).join('') || '<tr><td colspan="4" class="text-body-secondary">No passkeys.</td></tr>');
  }

  // ============ TOTP 2FA ============

  function loadTotp() {
    api('GET', '/api/auth/totp').done(function (resp) {
      $('#totp-status').text('Two-factor authentication is ' +
        (resp.enabled ? 'enabled.' : 'disabled.'));
      $('#totp-setup-btn').prop('hidden', resp.enabled);
      $('#totp-disable-btn').prop('hidden', !resp.enabled);
      if (!resp.enabled) {
        $('#totp-setup-panel, #totp-recovery-panel').addClass('d-none');
      }
    });
  }

  $('#totp-setup-btn').on('click', function () {
    api('POST', '/api/auth/totp/setup').done(function (resp) {
      $('#totp-secret').val(resp.secret_base32);
      $('#totp-url').val(resp.otpauth_url);
      $('#totp-code').val('');
      $('#totp-recovery-panel').addClass('d-none');
      $('#totp-setup-panel').removeClass('d-none');
      $('#totp-code').trigger('focus');
    });
  });

  $('#totp-verify-btn').on('click', function () {
    api('POST', '/api/auth/totp/verify', { code: $('#totp-code').val() }).done(function (resp) {
      $('#totp-setup-panel').addClass('d-none');
      $('#totp-recovery-codes').val(resp.recovery_codes.join('\n'));
      $('#totp-recovery-panel').removeClass('d-none');
      $('#totp-recovery-panel')[0].scrollIntoView({ block: 'center' });
      toast('Two-factor authentication enabled.');
      loadTotp();
    });
  });

  $('#totp-disable-btn').on('click', function () {
    confirmDialog('Disable two-factor authentication? Your recovery codes will stop working.').done(function () {
      api('DELETE', '/api/auth/totp').done(function () {
        toast('Two-factor authentication disabled.');
        loadTotp();
      });
    });
  });

  // ============ WebAuthn passkeys ============

  function b64uToBuf(value) {
    var s = value.replace(/-/g, '+').replace(/_/g, '/');
    while (s.length % 4) { s += '='; }
    var bin = atob(s);
    var bytes = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) { bytes[i] = bin.charCodeAt(i); }
    return bytes.buffer;
  }

  function bufToB64u(buffer) {
    var bytes = new Uint8Array(buffer);
    var bin = '';
    for (var i = 0; i < bytes.length; i++) { bin += String.fromCharCode(bytes[i]); }
    return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }

  function webauthnSupported() {
    return !!(window.PublicKeyCredential && navigator.credentials && navigator.credentials.create);
  }

  function showPkNote(text) {
    $('#pk-note').text(text).removeClass('d-none');
  }

  $('#passkey-form').on('submit', function (ev) {
    ev.preventDefault();
    if (!webauthnSupported()) {
      showPkNote('This browser does not support passkeys (WebAuthn).');
      return;
    }
    // silent: a server without WEBAUTHN_RP_ID 500s here; show it as a note,
    // not an alert.
    api('POST', '/api/auth/webauthn/register/start', null, { silent: true }).done(function (resp) {
      var pk = resp.challenge.publicKey;
      pk.challenge = b64uToBuf(pk.challenge);
      pk.user.id = b64uToBuf(pk.user.id);
      (pk.excludeCredentials || []).forEach(function (c) { c.id = b64uToBuf(c.id); });
      navigator.credentials.create(resp.challenge).then(function (cred) {
        return api('POST', '/api/auth/webauthn/register/finish', {
          challenge_id: resp.challenge_id,
          name: $('#pk-name').val(),
          credential: {
            id: cred.id,
            rawId: bufToB64u(cred.rawId),
            type: cred.type,
            response: {
              attestationObject: bufToB64u(cred.response.attestationObject),
              clientDataJSON: bufToB64u(cred.response.clientDataJSON),
            },
          },
        }, { silent: true });
      }).then(function () {
        $('#pk-name').val('');
        load();
      }).catch(function (err) {
        showPkNote(err && err.status
          ? ((err.responseJSON && err.responseJSON.error) || 'Passkey registration failed.')
          : 'Passkey registration was cancelled or failed'
            + (err && err.name ? ' (' + err.name + ').' : '.'));
      });
    }).fail(function (xhr) {
      showPkNote((xhr.responseJSON && xhr.responseJSON.error)
        || 'Passkeys are not available on this server.');
    });
  });

  // ============ shared ============

  function load() {
    api('GET', '/api/auth/tokens').done(renderTokens);
    api('GET', '/api/auth/app-passwords').done(renderPasswords);
    api('GET', '/api/auth/webauthn').done(renderPasskeys);
    loadTotp();
  }

  $('#token-form').on('submit', function (ev) {
    ev.preventDefault();
    var expires = $('#token-expires').val();
    api('POST', '/api/auth/tokens', {
      name: $('#token-name').val(),
      scopes: $('#token-readonly').prop('checked') ? ['read'] : [],
      expires_at: expires ? new Date(expires + 'T23:59:59').toISOString() : null,
    }).done(function (resp) {
      $('#token-name').val('');
      $('#token-readonly').prop('checked', false);
      $('#token-expires').val('');
      showSecret(resp.secret);
      load();
    });
  });

  $('#app-password-form').on('submit', function (ev) {
    ev.preventDefault();
    api('POST', '/api/auth/app-passwords', { name: $('#ap-name').val() }).done(function (resp) {
      $('#ap-name').val('');
      showSecret(resp.password);
      load();
    });
  });

  $(document).on('click', '[data-kind][data-id]', function () {
    var kind = $(this).data('kind');
    var id = $(this).data('id');
    var message = kind === 'passkey'
      ? 'Remove this passkey? You will no longer be able to sign in with it.'
      : 'Revoke this credential? Apps using it will stop working.';
    confirmDialog(message).done(function () {
      var url = kind === 'passkey' ? '/api/auth/webauthn/' + id : '/api/auth/' + kind + 's/' + id;
      api('DELETE', url).done(function () {
        toast(kind === 'passkey' ? 'Passkey removed.' : 'Credential revoked.');
        load();
      });
    });
  });

  $('#account-btn').on('click', function () { window.location.href = '/'; });
  $('#logout-btn').on('click', function () {
    api('POST', '/api/auth/logout').done(function () { window.location.href = '/login'; });
  });

  if (!webauthnSupported()) {
    $('#passkey-form').addClass('d-none');
    showPkNote('This browser does not support passkeys (WebAuthn).');
  }

  load();
});