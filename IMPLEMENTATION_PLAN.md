# Implementation Plan

Handoff status as of 2026-09-11. Stages follow `docs/IMPLEMENTATION_CHECKLIST.md`.
Approach: smallest working version per stage that passes
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, and `cargo test --workspace`, then live
verification against a throwaway PostgreSQL.

Current state: 17 test suites green, workspace fmt/clippy clean, all shipped
stages verified end-to-end over HTTP against real PostgreSQL 16.

## What exists and is verified

- Single binary (`calendar-server`): `serve` (default), `migrate`, `check`.
  Config from env only (`DATABASE_URL`, `DATABASE_MAX_CONNECTIONS`,
  `BIND_ADDR`, `SESSION_TTL_HOURS`; see `.env.example`). Embedded sqlx
  migrations run at startup.
- Schema `migrations/0001_initial.sql`: 33 tables — tenants/tenant_members,
  users, sessions, api_tokens, app_passwords, webauthn_credentials,
  totp_secrets, calendars + calendar_acl, timezones (custom VTIMEZONEs),
  events (masters + exceptions in one table), event_attendees, event_alarms,
  attachments (capped bytea), change_log, public_shares, durable_jobs, rules,
  rule_executions, notification_providers, notifications, webhooks,
  webhook_deliveries, web_push_subscriptions, schedule_messages, subscriptions
  (inbound share subscriptions), audit_log, locations.
- Auth: register → argon2id password + personal tenant bootstrap; login →
  HttpOnly SameSite=Lax session cookie + CSRF token (session mutations need
  `X-CSRF-Token`); Bearer API tokens (sha256 stored, secret shown once); app
  passwords with CalDAV-style Basic auth (sha256 lookup fast path + argon2
  verify). All verified live including 401 paths and logout revocation.
- Calendars: CRUD + ACL engine (owner/read_write/read_only/free_busy,
  `CalendarCapability::satisfies` ordering, ≥1 owner enforced via
  `AclSet::validate`), ACL read/replace. Guard 404s absent rows, 403s
  insufficient capability.
- Events: CRUD with `If-Match` ETags (409 on stale), attendees, recurring
  masters + RECURRENCE-ID exceptions. `change_log` entry + `calendars.ctag`
  bump in the same transaction as every mutation.
- Recurrence engine `calendar-core/src/recurrence.rs`: hand-rolled RRULE
  (DAILY/WEEKLY/MONTHLY/YEARLY, INTERVAL, COUNT, UNTIL, BYDAY with monthly
  ordinals, BYMONTHDAY, BYMONTH), wall-clock expansion, DST-correct,
  200k-step horizon cap, RDATE/EXDATE. 30 unit tests.
- `GET /calendars/{id}/occurrences`: expansion + exception overlay verified
  live (exception replaces its occurrence by RECURRENCE-ID wall-clock match).
- iCalendar `calendar-caldav`: `events_to_ics` / `parse_ics` round-trip
  (TZID/VALUE=DATE, RRULE/RDATE/EXDATE, RECURRENCE-ID, ORGANIZER/ATTENDEE
  parameters, X-ALT-DESC). VTODO parse → error (ADR-011).

## Stage status

1. **Workspace/tooling/configuration** — Complete.
2. **PostgreSQL schema and migrations** — Complete (validated on empty DB;
   invariant smoke tests for uid/occurrence uniqueness).
3. **Domain types and validation** — Complete (calendar-core; 38 unit tests
   across the workspace).
4. **Authentication** — Mostly complete. Passwords, sessions+CSRF, API
   tokens, app passwords (Basic) done and verified. **Remaining: TOTP
   (needs HMAC-SHA1 + base32 — use a crate, not hand-rolled crypto) and
   WebAuthn (`webauthn-rs` already a workspace dep; needs challenge-state
   storage).**
5. **Calendar CRUD and ACL engine** — Complete.
6. **Event CRUD and optimistic concurrency** — Complete.
7. **iCalendar parser/serializer and recurrence engine** — Complete.
8. **dav-server-rs PostgreSQL adapter** — Not started. Dep not yet added.
   ARCHITECTURE.md: guarded resource adapter, not LocalFs/MemFs.
9. **CalDAV discovery, calendar-query, multiget, sync-token** — Not started.
   `change_log.seq` is the sync token; `calendars.ctag` exists.
10. **Scheduling/iTIP/iMIP** — Not started. `schedule_messages` table exists
    for outbound dedupe + inbound Postmark webhook (ADR-009).
11. **VALARM and durable scheduler** — Not started. `durable_jobs` table
    ready (priority/max_attempts/locked_until, ready+stuck partial indexes).
12. **Attachments and streaming** — Not started. `attachments` table exists;
    cap is env-configurable, app-enforced (ADR-010).
13. **Search** — Partial. `events.search_vector` (tsvector, generated) + GIN
    exist; no query path. Attendee/location/category search via joins.
14. **Public shares and subscriptions** — Not started. `public_shares`
    (sha256 token, `allows_caldav`) and `subscriptions` tables ready.
15. **OpenAPI generation** — Not started. `calendar-api` crate is a stub.
16. **Rules and notification providers** — Not started. `calendar-rules`
    and `calendar-notify` are stubs; tables ready.
17. **Web UI** — Not started. Bootstrap 5.3 + jQuery 4 + vendored bs-calendar
    (PRD §18; `calendar-web` crate is a stub).
18. **Backup/export/import** — Not started. CLI subcommands pending.
19. **Audit and retention/purge** — Not started. `audit_log` +
    `rule_executions` retention indexes exist; purge job not written.
20. **Interop suite and hardening** — Not started. `tests/interop/` empty;
    golden fixtures land here.

## Environment notes (this machine)

- Port 8080 and PostgreSQL 5433 are held by unrelated local services. Use
  `BIND_ADDR` and a throwaway Postgres cluster.
- Test harness: `/usr/lib/postgresql/16/bin/initdb -D /tmp/pgtest` +
  `pg_ctl -o "-p 55432 -k /tmp/pgtest"`, run binary with `setsid`, drive
  with curl. Recreating the DB is required after touching an applied
  migration (sqlx checksums).
- `icalendar` 0.17 parser rejects RFC 5545 folded lines; `parse_ics`
  unfolds first — never call the crate parser directly.

## Sequencing for remaining work

Suggested order for the next sessions: 8 → 9 (CalDAV on dav-server-rs,
round-trips through the existing repos), 11 (jobs/alarm scheduler), 12 → 14
(attachments, shares), 15 (OpenAPI), then 10/13/16/19, 17 (web UI) once the
API surface is stable, 18 + 20 last.