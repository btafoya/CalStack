# DAVx⁵ (Android)

DAVx⁵ syncs Daymark calendars, address books and tasks into the Android
Calendar/Contacts/Tasks providers, so any Android calendar or contacts app
works against Daymark.

> Status: **events and contacts are tested in production use** (two-way sync,
> recurring events with per-occurrence exceptions; see
> [COMPATIBILITY.md](../COMPATIBILITY.md) for the recorded configuration).
> Task (VTODO) sync via DAVx⁵ is not recorded as tested yet. The protocol
> level (`.well-known` discovery, app-password auth, sync-collection) is
> covered by the automated interop suite.

## Setup

1. Create an **app password** for the device: sign in to the web UI →
   **Credentials** → app passwords (or `POST /api/auth/app-passwords`). Use the
   app password, not your login password.
2. In DAVx⁵: **+ → "Login with URL and user name"**.
   - Base URL: `https://<your-host>/`
   - User name: your Daymark username
   - Password: the **app password**
3. DAVx⁵ discovers CalDAV calendars, CardDAV address books and VTODO task
   lists from the well-known links; pick what to sync.
4. Enable calendar/contact/task sync and grant DAVx⁵ the Android permissions
   when prompted.

## Notes

- Daymark advertises `VEVENT`, `VTODO` and `VJOURNAL` components. Task lists
  need the calendar to have Tasks enabled (calendar edit dialog in the web
  UI); Android task apps such as Tasks.org and jtx Board read VTODO through
  DAVx⁵.
- Recurring events with exceptions (`RECURRENCE-ID`) round-trip.
- A public share token with `allows_caldav` also works as a read-only CalDAV
  credential: use the **token as the username** (any password) for a
  read-only, revocable subscription.