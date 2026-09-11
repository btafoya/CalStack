# Claude Code Instructions

## Project

Build the Lightweight Calendar Server described in `docs/PRD.md`.

This is an independent project. Do not associate it with unrelated repositories or projects.

## Non-negotiable architecture

- Rust.
- One production executable.
- PostgreSQL is the only external runtime dependency.
- No Docker requirement.
- No Redis.
- No Elasticsearch/Meilisearch.
- No filesystem data directory.
- Attachments are stored in PostgreSQL.
- Configuration comes from environment variables only.
- Web UI static assets are embedded in the executable.
- MIT license.
- OpenAPI is a first-class public application API.
- CalDAV is a first-class interoperability protocol.
- PostgreSQL is the canonical normalized event store; do not use opaque ICS blobs as the primary data model.
- `docs/DECISIONS.md` ADRs are binding. Settled: tenants with multi-tenant users, Postmark inbound iMIP webhook, capped bytea attachments, VTODO rejected 403, hand-rolled recurrence with expansion horizon, web UI = Bootstrap 5.3 + jQuery 4 + vendored bs-calendar (no CDN, no build step). Do not re-ask these.
- Workspace deps still missing but mandated by the docs — add at their checklist stage, not sooner: `dav-server-rs`, `ammonia`, `jquery-migrate` asset.
- Integration testing: throwaway Postgres via `/usr/lib/postgresql/16/bin` (initdb + pg_ctl, custom socket dir + port), binary via `setsid`, drive with curl. Changing an applied migration → drop/recreate the test DB (sqlx checksum).
- `icalendar` 0.17 parser rejects RFC 5545 folded lines — parse ICS only through `calendar-caldav::parse_ics` (it unfolds first), never the crate parser directly.

## Implementation loop

For every meaningful feature:

1. Read the relevant requirements and existing code.
2. Design the smallest maintainable implementation.
3. Write/update unit tests before or alongside implementation.
4. Implement.
5. Run `cargo fmt --all -- --check`.
6. Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
7. Run `cargo test --workspace --all-features`.
8. Run integration tests against a real PostgreSQL instance.
9. Run protocol/interoperability tests for affected CalDAV/iCalendar behavior.
10. Inspect failures; fix root causes rather than weakening tests.
11. Repeat until all required checks pass.
12. Update documentation/API schema/migrations when behavior changes.
13. Update codegraph index `codegraph index`

Never declare a feature complete merely because it compiles.

## Compatibility priorities

Prefer standards-compliant behavior first, then documented compatibility shims where real clients require them.

Target:
- RFC 4918 WebDAV
- RFC 3744 WebDAV ACL
- RFC 4791 CalDAV
- RFC 5545 iCalendar
- RFC 5546 iTIP
- RFC 6047 iMIP
- RFC 6578 WebDAV sync
- RFC 6638 CalDAV scheduling
- RFC 7986 calendar extensions
- RFC 9073 newer iCalendar extensions where practical
- RFC 8984 JSCalendar as an API/domain-adjacent future capability, without making it a CalDAV dependency

Test representative Apple, Android/DAVx5, Thunderbird and Windows CalDAV clients where feasible. Do not claim native support for clients that do not implement CalDAV.

## Data rules

- UUIDs are stable opaque public identifiers.
- Calendar slug is unique within an account/tenant namespace.
- Calendar names may repeat.
- Preserve IANA timezone identity; normalize instants for querying.
- Recurrence is full-fidelity: RRULE, RDATE, EXDATE, RECURRENCE-ID and exceptions.
- Attendees are independent from calendar ACL membership.
- Calendar ACLs support multiple owners and future group principals.
- Event visibility supports public/private/confidential/free-busy semantics.
- Public sharing is revocable and may expire.
- Rich HTML is sanitized; plain text is generated for compatible clients/search.
- Locations use a Google Places-style structured model while retaining standard iCalendar LOCATION output.
- Attachments live in PostgreSQL.
- Audit history is lightweight, not an unlimited event-version store.
- Deleted resources are soft-deleted for configurable retention, then purged.

## Security

Never log passwords, tokens, passkeys, OTP secrets, OAuth-like credentials, SMTP credentials, Twilio credentials, or attachment contents.

Use Argon2id for passwords. Use WebAuthn for passkeys. Encrypt sensitive application data optionally using an environment-provided key. API tokens are stored hashed, scoped, revocable, and optionally expiring.

## API

OpenAPI must cover:
users, authentication, passkeys, 2FA, tokens, calendars, ACLs, events, recurrence, attendees, locations, attachments, subscriptions, public shares, reminders, rules, notification providers, webhooks, audit records, search and sync/change history.

Do not expose server configuration mutation through the normal application API.

## Database

Use SQL migrations. Provide both:
- embedded/default automatic migration behavior
- an explicit migration CLI command

Design the data layer so read replicas can be added later without coupling domain logic to PostgreSQL connection details.

## Performance

Measure before optimizing. Favor:
- prepared statements
- appropriate PostgreSQL indexes
- bounded connection pools
- streaming large resources
- incremental CalDAV sync
- database-backed job locking
- minimal allocations in hot protocol paths

Avoid unnecessary abstraction layers.

## Done criteria

A release is not complete until:
- clean build
- format/clippy/tests pass
- migrations apply to an empty PostgreSQL database
- migration upgrade tests pass
- OpenAPI schema is generated and validated
- CalDAV discovery works
- CRUD round-trips through API and CalDAV
- sync-token changes work
- recurring events and exceptions round-trip
- ACL enforcement is tested
- public feeds are tested
- attachments round-trip
- reminders/jobs survive process restart
- notification actions are idempotent
- security tests cover authentication/authorization boundaries
- backup/export and restore/import are tested
- CI passes
