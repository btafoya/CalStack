# Lightweight Calendar Server

A fast, self-hosted, single-binary calendar server written in Rust with PostgreSQL as its only external dependency.

## Goals

- Full CalDAV/iCalendar interoperability.
- Android, iOS, macOS, Windows and Linux CalDAV-client compatibility.
- PostgreSQL-backed normalized event model.
- OpenAPI CRUD API covering the complete calendar domain.
- Multiple calendar owners/members with read-only, read-write, free/busy and ACL-management capabilities.
- Public read-only calendars, `.ics` feeds and revocable share tokens.
- Full recurrence and scheduling semantics.
- External attendees with email and optional SMS notifications.
- Server-side VALARM execution.
- Rich HTML event descriptions suitable for pasted email/poster content.
- Structured Google Places-style event locations.
- PostgreSQL full-text search.
- PostgreSQL-backed durable jobs; no Redis/queue service.
- Attachments stored entirely in PostgreSQL.
- Local authentication, passkeys/WebAuthn, TOTP 2FA, scoped API tokens and CalDAV app passwords.
- Lightweight embedded web UI.
- Optional rules/actions, Web Push, Postmark/SMTP and Twilio.
- No application data directory: runtime state is PostgreSQL.
- MIT licensed.

## Deployment

The intended production topology is:

    calendar-server binary
             |
         PostgreSQL

Configuration is environment-variable-only. Static web assets are embedded in the binary.

## Status

This repository is an implementation-ready Claude Code project specification and Rust workspace scaffold. The implementation loop in `CLAUDE.md` is deliberately explicit so Claude Code can build and verify the system incrementally rather than producing an untested monolith.

See `docs/ARCHITECTURE.md`, `docs/PRD.md`, and `docs/COMPATIBILITY.md`.
