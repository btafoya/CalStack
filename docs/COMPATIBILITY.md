# Compatibility Matrix and Test Strategy

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
