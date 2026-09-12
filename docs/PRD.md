# Product Requirements Document

## 1. Product

A lightweight, fast, self-hosted calendar server implemented as a single Rust binary backed only by PostgreSQL.

## 2. Core use case

Provide a standards-compatible calendar service that works with existing CalDAV clients while exposing a clean modern OpenAPI API for other applications.

## 3. Core entities

- Tenant
- User
- Tenant membership (users may belong to multiple tenants)
- Credential
- Calendar
- Calendar membership/ACL
- Event
- Event attendee
- Event organizer
- Recurrence rule
- Event exception
- Location
- Attachment
- Alarm
- Subscription
- Public share
- API token
- Rule
- Rule execution
- Notification
- Webhook
- Audit entry
- Durable job
- Sync/change record

## 4. Calendar permissions

Calendars support multiple owners.

Effective capabilities:
- owner
- read/write
- read-only
- free/busy-only
- ACL/share management as an independent capability

ACL representation must be principal-based so groups can be introduced later.

Cross-user free/busy lookup and the RFC 4791 free-busy-query REPORT must respect these visibility capabilities.

## 5. Public sharing

A calendar may be publicly readable without an account.

Public capabilities:
- read-only web calendar/event views
- `.ics` subscription
- optional read-only CalDAV access using revocable share tokens

Public shares support expiration and revocation.

Privacy rules prevent accidental exposure of private attendee/contact data.

## 6. Events

Support:
- title/summary
- rich HTML description
- generated plain text
- start/end or duration
- all-day events
- IANA timezone
- status
- priority
- categories
- organizer
- attendees
- location
- URL
- attachments
- alarms
- recurrence
- exceptions
- transparency/free-busy behavior
- visibility/class
- sequence/ETag/change metadata

Use stable UUID identifiers.

VTODO is not supported. PUT of a VTODO resource returns 403 with a CalDAV error body.

## 7. Rich content

Users can paste rich content from email or posters.

Store sanitized HTML as canonical rich description and generate:
- plain text
- iCalendar-compatible description
- HTML representations where client support exists

Do not store unsafe scripts or active content.

## 8. Locations

Location is structured around Google Places-style concepts:
- provider
- provider place ID
- display name
- formatted address
- street address
- address components
- locality
- administrative region
- postal code
- country
- latitude
- longitude
- optional website/phone
- raw provider metadata where appropriate

Do not require Google at runtime. A location can exist without a provider ID.

Optionally, a server-configured Google Places API (New) key (`GOOGLE_MAPS_API_KEY`) powers place autocomplete in the web UI through an authenticated server-side proxy; the key is never exposed to browsers. Without the key, locations are free text.

Serialize compatible standard iCalendar location fields.

## 9. Attendees and scheduling

Support internal and external attendees.

Internal:
- user identity
- RSVP
- scheduling state

External:
- email
- optional telephone
- display name
- RSVP
- scheduling state

Support invitations, responses, updates, cancellations, recurring-series scheduling and exceptions.

Required calendar scheduling mail is handled by the notification subsystem. Custom messages are handled by rules.

Inbound iMIP replies from external attendees are ingested through the Postmark inbound webhook. Generic SMTP remains outbound-only.

Optional SMS can be sent for external attendees.

## 10. Recurrence

Full support:
RRULE, RDATE, EXDATE, RECURRENCE-ID, DTSTART and exception events.

Do not flatten recurrence into individual rows as the canonical representation.

Use occurrence expansion only when required for querying, conflict detection, reminders or feeds.

Recurrence expansion is hand-rolled (own iterator over the RFC 5545 subset), with a configurable expansion horizon so unbounded RRULEs cannot cause unbounded work.

Client-supplied VTIMEZONE definitions that do not match the tzdb are stored and honored for expansion and serialization.

## 11. Sync

Implement:
- ETags
- CTag where needed
- WebDAV sync tokens
- incremental collection changes
- deleted-resource reporting
- stable resource URLs

Also expose optional application change streams through OpenAPI, with transport adapters for SSE/WebSocket.

## 12. Search

PostgreSQL-native search over:
- summary
- sanitized/plain description
- attendees
- location fields
- categories
- relevant metadata

No external search service.

## 13. Attachments

Full binary data is stored in PostgreSQL as `bytea`, with a configurable per-attachment size cap (default 50 MB). Uploads exceeding the cap are rejected; no Large Object storage path exists.

Metadata:
- UUID
- filename
- MIME type
- byte size
- checksum
- event relationship
- created timestamp

## 14. Authentication

Local accounts.

Support:
- password authentication
- WebAuthn/passkeys
- TOTP 2FA
- scoped API bearer tokens (scopes: `read` = GET only, `write` = all methods and implies `read`, `full` or empty = unrestricted; enforced by HTTP-verb router middleware)
- client-compatible app passwords (never scoped)

The web UI authenticates with DB-backed sessions (HttpOnly cookie) and CSRF protection.

Never require WebAuthn for CalDAV clients that cannot perform it.

## 15. Notifications

Providers:
- Postmark
- generic SMTP
- Twilio
- Web Push

Design native Apple/Android push adapters as future providers.

## 16. Rules

Initial engine is intentionally simple:
triggers -> optional conditions -> actions.

Initial triggers:
- event created
- event updated
- event deleted
- attendee invited
- RSVP changed
- alarm due
- calendar shared
- public share created/revoked
- webhook/API-triggered event

Initial actions:
- email
- SMS
- webhook
- create notification
- future push

Architecture must allow future branching, delays and richer variables.

## 17. Background jobs

PostgreSQL is the durable queue/scheduler.

Use leases/advisory locks or equivalent PostgreSQL locking to ensure one worker owns a job.

Jobs must be idempotent.

## 18. Web UI

Embedded static assets.

Frontend stack is fixed: Bootstrap 5.3 for all styling/components and jQuery 4 for ALL JavaScript logic. No other framework, library or build step. Assets are served from the executable (no CDN at runtime). Use `bootstrap.bundle.min.js` (includes Popper) with jQuery 4's current APIs — deprecated pre-4 utilities (`$.trim`, `$.proxy`, `$.isArray`, etc.) are gone and must not be used.

The calendar UI is https://github.com/ThomasDev-de/bs-calendar (MIT, v2.4.0, Bootstrap 5, seven views, drag-create/move/resize, localization). Its dist files are vendored into embedded assets, never loaded from CDN. Run it under jQuery 4 with the jQuery Migrate plugin (3.x support) since bs-calendar targets jQuery ^3; Migrate stays a compatibility shim only — new UI code uses jQuery 4 APIs directly. Also vendor Bootstrap Icons, which bs-calendar depends on.

Server-rendered/progressive-enhancement approach:
- login/account
- calendar list
- event list/detail/editor
- rich paste editor
- sharing/ACL management
- reminders
- rules
- administration

Do not build a heavyweight SPA unless required by actual UX needs.

## 19. API

Full OpenAPI CRUD and operations.

The OpenAPI 3.1 document is served at `/api/openapi.json`. A vendored Swagger UI at `/docs` renders it for interactive exploration; both are unauthenticated, like the JSON document itself.

The API is not a thin wrapper around CalDAV. It exposes the normalized domain model directly.

## 20. Backup

Support:
- PostgreSQL-native backup guidance
- portable application export/import
- CLI full backup/restore including attachments

## 21. Deletion

Soft-delete calendar resources as required for synchronization, retaining them for configurable retention before purge.

## 22. Audit

Record actor, timestamp, object, operation and compact change metadata.

Do not create unlimited event versions.

## 23. Deployment

Single executable:
`calendar-server`

Environment variables only.

No required folders for runtime data.

PostgreSQL contains application state and attachment bytes.
