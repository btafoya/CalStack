# Compatibility Matrix and Test Strategy

## Results status (updated 2026-09-28)

Protocol-level behavior is covered end-to-end by `tests/interop/run.sh`
(69 automated steps: discovery, CRUD, REPORTs, sync-token, free-busy, ACL,
ETag handling). The capture harness ([INTEROP_CAPTURE.md](INTEROP_CAPTURE.md))
produces per-request evidence for client sessions.

### Tested configurations

| Client | Calendar | Contacts | Tasks | Notes |
|---|---:|---:|---:|---|
| DAVx⁵ + Android | ✅ | ✅ | not recorded | production use, ongoing |
| Band Manager (eventmgr, CalDAV consumer) | ✅ | — | — | production use, ongoing |

- **DAVx⁵ + Android** — real-device sessions against a production server
  (current build as of 2026-09-28; DAVx⁵/Android versions not recorded).
  Exercised: account discovery, app-password credentials, two-way sync of
  events and contacts, recurring events with per-occurrence modifications
  and exceptions. A bug found in production use — overrides left with the
  master's UID after a "this and following" series split, making them
  permanently unwritable via CalDAV PUT — was fixed in commit `3bde372`
  (re-parent overrides' UID on series split) with a regression test.
- **Band Manager (eventmgr)** — a Go application consuming Daymark
  calendars over CalDAV (display-name-keyed multi-calendar adoption,
  LOCATION property) as its active calendar provider, in production use.
  Private sibling project; source not public.

Everything else below remains a **test plan**, not results. Record new rows
with client version, server version, date, and the scenario list actually
exercised. Untested stays blank/unmarked — no protocol-in-theory claims
(also README rule).

## Target clients

### Apple
- iOS Calendar
- macOS Calendar

Test:
- account discovery
- calendar list
- create/update/delete
- recurring events
- attendees
- alarms
- public subscriptions

### Android
Primary reference client: DAVx5 + Android Calendar Provider.

Test:
- discovery
- credentials/app password
- two-way sync
- deletions
- recurrence/exceptions
- timezone handling
- alarms

### Linux
Test Thunderbird and other CalDAV clients.

### Windows
Test CalDAV-capable Windows applications. Do not promise native support for Windows calendar products that do not implement third-party CalDAV.

## Protocol tests

Automate:
- OPTIONS
- PROPFIND
- REPORT
- GET
- PUT
- DELETE
- ETag/If-Match
- sync-token
- calendar-query
- calendar-multiget
- free-busy-query (RFC 4791 section 9.1.1)
- ACL behavior
- principal discovery
- calendar home discovery

## iCalendar tests

Round-trip:
- VEVENT
- VTODO only if later explicitly added
- RRULE
- RDATE
- EXDATE
- RECURRENCE-ID
- VALARM
- ATTENDEE
- ORGANIZER
- VTIMEZONE
- GEO
- LOCATION
- URL
- ATTACH
- CLASS
- TRANSP
- STATUS
- SEQUENCE
- CATEGORIES

Reject malformed input safely and return appropriate CalDAV errors.

## Golden fixtures

Store representative `.ics` fixtures under `tests/interop/fixtures/`.

Do not copy proprietary/private calendar data into the repository.
