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
| Thunderbird 153.3.1 | ✅ | not recorded | ✅ | interop session, 2026-09-28 |
| Band Manager (eventmgr, CalDAV consumer) | ✅ | — | — | production use, ongoing |

- **DAVx⁵ + Android** — real-device sessions against a production server
  (current build as of 2026-09-28; DAVx⁵/Android versions not recorded).
  Exercised: account discovery, app-password credentials, two-way sync of
  events and contacts, recurring events with per-occurrence modifications
  and exceptions. A bug found in production use — overrides left with the
  master's UID after a "this and following" series split, making them
  permanently unwritable via CalDAV PUT — was fixed in commit `3bde372`
  (re-parent overrides' UID on series split) with a regression test.
- **Thunderbird 153.3.1** (Linux) — guided interop session 2026-09-28
  against a throwaway build of the current code (capture harness). Exercised:
  `.well-known` discovery, app-password Basic auth, CalDAV PUT/GET round-trip,
  timed event with VALARM, all-day event, weekly recurring series with
  single-occurrence edit and single-occurrence delete, VTODO tasks, VJOURNAL
  journals, cross-calendar move (Thunderbird's copy + delete-original flow —
  it prompts before removing the source by design). Bug found and fixed
  during the session: a cross-calendar move reusing the canonical
  `<uid>.ics` filename collided on the global events primary key and failed
  403 (fixed in `daf1095`, DB unit test + interop step). Thunderbird 153 no
  longer offers "Floating" times in the event editor, so floating-time E5
  could not be exercised from this client (covered by automated tests).
  Contacts via CardDAV not yet exercised.
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
- VTODO (first-class since ADR-015)
- VJOURNAL (first-class since ADR-015)
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
