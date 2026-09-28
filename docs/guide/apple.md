# Apple Calendar & Contacts (iOS / macOS)

> Status: the protocol behavior (`.well-known` discovery, app-password auth,
> sync-collection) is covered by the automated interop suite; Apple clients
> have not been recorded as a tested configuration yet. See
> [COMPATIBILITY.md](../COMPATIBILITY.md).

## Setup

1. Create an **app password**: web UI → **Credentials** → app passwords. Apple
   clients send Basic auth; use the app password, not your login password.
2. iOS: Settings → Apps → Calendar → **Calendar Accounts** → Add Account →
   Other → **CalDAV Account**. macOS: System Settings → Internet Accounts →
   Add Other Account → CalDAV.
   - Server: `your-host` (the bare hostname is enough — iOS/macOS use
     `.well-known/caldav` discovery)
   - User Name: your Daymark username
   - Password: the **app password**
3. Contacts, same flow with **CardDAV Account** (server `your-host`,
   `.well-known/carddav` points at `/contacts`).

## Notes

- Apple clients send `Accept-Encoding` and expect ETag-consistent responses;
  both are implemented.
- Recurring events: one-occurrence edits and deletions create
  `RECURRENCE-ID` overrides that round-trip.
- Apple Reminders uses VTODO; enable Tasks on the target calendar (calendar
  edit dialog in the web UI).
- Plain HTTP is generally refused by iOS/macOS; put Daymark behind TLS.