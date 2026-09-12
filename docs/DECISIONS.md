# Architecture Decisions

## ADR-001 PostgreSQL is the only external dependency

The application is intentionally one executable plus PostgreSQL. Background jobs, search, attachments and durable change state all use PostgreSQL.

## ADR-002 Normalized PostgreSQL calendar model

Calendar objects are represented relationally. iCalendar is a wire/storage interchange representation, not the canonical database format.

## ADR-003 Calendar ACLs are principal-based

Calendars support multiple owners and membership permissions. The schema must allow future group principals.

## ADR-004 Public sharing uses explicit share tokens

Anonymous access must be revocable and optionally expiring. Never infer public access from an unguessable URL alone without a database-backed share object.

## ADR-005 Rich HTML is canonical for pasted content

Sanitized HTML preserves the user's intent when pasting email/poster content. Plain text is derived for compatibility and indexing.

## ADR-006 Attachments are PostgreSQL byte data

This keeps deployment data-free on the application filesystem and makes portable backup/restore possible.

## ADR-007 Rules are initially simple

Start with triggers, conditions and actions. Keep the execution model extensible for later branching/delays.

## ADR-008 Tenants and multi-tenant users

Tenants are organizational namespaces (calendar slug uniqueness, shared calendars). Users can belong to multiple tenants via a membership table present from the first migration. ACL resolution walks tenant membership.

## ADR-009 Inbound iMIP uses Postmark inbound webhook

External attendee replies are parsed by Postmark's inbound stream and POSTed to a server webhook endpoint. Generic SMTP remains outbound-only. No IMAP polling, no own-MTA intake.

## ADR-010 Attachments are capped bytea

Attachments are `bytea` with a configurable size cap (default 50 MB). No `pg_largeobject` path: large-object streaming is not worth its backup and driver burden at calendar-attachment scale. The "stream large attachments" requirement is dropped.

## ADR-011 VTODO is rejected, not tolerated

PUT of a VTODO returns 403 with a CalDAV error body. No opaque blob fallback: a hidden VTODO store defers complexity without adding capability, and VTODO support should arrive as real support or not at all.

## ADR-012 Hand-rolled recurrence engine

Recurrence expansion is an own iterator over the RFC 5545 subset (freq/interval/BY* rules), not the `rrule` crate: full control over EXDATE/RECURRENCE-ID interplay and DST behavior, since recurrence correctness is core to a calendar server. All expansion runs under a configurable horizon bound so unbounded RRULEs cannot cause unbounded work. Client-supplied VTIMEZONE definitions are stored and honored.

## ADR-013 Rules are optionally calendar-scoped

`rules.calendar_id` is a nullable FK to `calendars`, not a required one: NULL keeps the original ADR-007 tenant-wide behavior, a value scopes the rule to one calendar. Creating a calendar-scoped rule requires Owner capability on that calendar. `run_rules` matches a calendar's own rules plus every tenant-wide (NULL) rule, so existing global rules keep firing unchanged.

## ADR-014 Place autocomplete is a server-side proxy, never Maps JS

Google Places integration (web UI event form autocomplete) runs entirely server-side: the browser calls authenticated `/api/places/autocomplete` and `/api/places/{place_id}` endpoints, which proxy the Places API (New) REST endpoints using the optional `GOOGLE_MAPS_API_KEY` env var. The key never reaches the browser, the web UI loads no third-party script (the no-CDN rule holds), and unset key means the feature is off with free-text locations unchanged. Structured place data maps onto the existing Google Places-style `locations` model; locations without a provider ID remain fully valid.
