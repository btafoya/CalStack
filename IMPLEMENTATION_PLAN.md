# Implementation Plan

All 20 checklist stages (`docs/IMPLEMENTATION_CHECKLIST.md`) are complete.
Gate: `make verify` (fmt, check, clippy -D warnings, tests) plus
`make interop` (live end-to-end suite against a throwaway PostgreSQL 16).

## Shipped surface

- Single binary `calendar-server`: `serve` (default, with in-process durable
  job worker), `migrate`, `check`, `backup`, `restore`.
- JSON API under `/api/*` (OpenAPI 3.1 document at `/api/openapi.json`).
- CalDAV at `/calendars/*` (dav-server-rs guarded PostgreSQL adapter):
  discovery (well-known, current-user-principal, calendar-home-set),
  MKCALENDAR, PUT/GET/DELETE, PROPFIND, calendar-query, calendar-multiget,
  sync-collection (RFC 6578) and free-busy-query (RFC 4791) REPORTs,
  VTODO PUT → 403 with CalDAV error body (ADR-011), ETag If-Match.
- Auth: passwords (Argon2id), sessions + CSRF, API tokens, app passwords
  (CalDAV Basic), TOTP 2FA (envelope-encrypted seeds, recovery codes),
  WebAuthn passkeys (in-memory challenge state).
- Calendars/ACLs, events (masters + RECURRENCE-ID exceptions), full recurrence
  expansion with exception overlay, VALARM parse/serialize + reminders via the
  durable job queue (deduped notifications).
- Attachments: capped bytea (ADR-010), API upload/download guarded by ACL.
- Search: tsvector + attendee/location/category joins; change stream endpoint.
- Public shares: hashed tokens, anonymous `.ics` feeds (PRIVATE/CONFIDENTIAL
  and attendee PII withheld), revocation + expiry; inbound subscriptions.
- Scheduling: outbound iTIP REQUEST recording with dedupe, `imip_send` job
  (Postmark HTTP or generic SMTP via lettre), Postmark inbound webhook for
  REPLY RSVPs (ADR-009).
- Rules (ADR-007): trigger → conditions → actions with execution records;
  notification providers (postmark/smtp/twilio kinds) with envelope-encrypted
  configs. Web Push remains an interface stub (`webpush` kind reserved).
- Retention: `retention_purge` job (soft-deleted events, expired sessions,
  challenges, read notifications); audit trail endpoint (admin).
- Backup: portable JSON export/import including attachments (verified
  round-trip into a fresh database); pg_dump remains the full-fidelity path.
- Web UI (stage 17): embedded Bootstrap 5.3 + jQuery 4 + jQuery Migrate +
  Bootstrap Icons + bs-calendar 2.4.0, no CDN, no build step; login page,
  calendar list, week view, event editor with sanitized rich paste.

## Known ceilings (deliberate, ponytail-marked in code)

- dav-server's `supported-calendar-component-set` advertises VTODO (hardcoded
  in the crate); our PUT path rejects VTODOs per ADR-011.
- calendar-query REPORT filtering is comp-filter only (no time-range
  push-down); clients receive a superset and filter locally.
- Calendar-query/multiget read_dir scans a ±2/5-year window per request;
  add SQL push-down if a profiled calendar needs it.
- Rules conditions DSL and web-push delivery are minimal.
- CalDAV MOVE/COPY return 501; resource URLs are uuid-keyed and stable.

## Environment notes (this machine)

- Port 8080 and PostgreSQL 5433 are held by unrelated local services.
- Test harness: `tests/interop/run.sh` boots its own PostgreSQL on 55433.
- `icalendar` 0.17 parser rejects RFC 5545 folded lines; `parse_ics` unfolds
  first — never call the crate parser directly.