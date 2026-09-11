# Architecture

## Layering

calendar-server
- HTTP routing
- OpenAPI
- CalDAV/WebDAV
- authentication middleware
- embedded web UI

calendar-core
- domain entities
- permissions
- recurrence
- scheduling state
- validation
- event serialization interfaces

calendar-db
- PostgreSQL repositories
- transactions
- migrations
- search
- jobs
- attachment streaming
- change journal

calendar-auth
- passwords
- WebAuthn
- TOTP
- API tokens
- app passwords

calendar-caldav
- CalDAV resource mapping
- dav-server-rs adapter
- iCalendar parse/serialize
- WebDAV sync/ETag/ACL integration

calendar-api
- OpenAPI models and handlers

calendar-rules
- triggers, conditions and actions

calendar-notify
- SMTP/Postmark/Twilio/Web Push provider interfaces

calendar-web
- embedded UI assets and server integration

## dav-server-rs integration

Use `dav-server-rs` as the WebDAV/CalDAV protocol foundation where it fits, implementing a PostgreSQL-backed guarded filesystem/resource adapter rather than using LocalFs/MemFs for persistent calendar data.

Do not force the calendar domain into a filesystem-shaped database abstraction where it harms correctness. The adapter should translate DAV operations into domain/repository operations.

## Canonical event model

PostgreSQL normalized records are authoritative.

iCalendar is a protocol representation.

The application must preserve semantics necessary to round-trip supported RFC fields, but must not make arbitrary `.ics` blobs the primary storage format.

## Transaction boundary

Mutations that affect event state, sync journal, audit records and durable jobs must use PostgreSQL transactions.

A successful event mutation must atomically create its change record.

## Change journal

Every sync-visible resource mutation creates a monotonic per-collection/server change sequence.

A sync token identifies a point in that sequence.

Old changes cannot be purged until their retention policy permits it; otherwise a client must receive a signal requiring full resynchronization.

## Resource URLs

Use human-friendly calendar slugs and UUID event resources:

`/calendars/{account}/{calendar-slug}/{event-uuid}.ics`

The exact DAV principal/home-set structure must follow CalDAV discovery conventions.

## Concurrency

Use ETags/If-Match semantics to prevent lost updates.

OpenAPI update operations should expose optimistic concurrency using ETags/version fields.

## Jobs

Durable jobs live in PostgreSQL. Workers run in-process.

Scheduled jobs use `run_at`.
Leased jobs use `locked_until` and worker identity.
Retries use exponential backoff with a maximum.

## Security boundaries

Authenticate once at the HTTP layer where possible, but authorization must be checked at the domain/resource boundary so OpenAPI and CalDAV enforce identical permissions.
