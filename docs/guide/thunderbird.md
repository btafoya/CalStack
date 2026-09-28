# Thunderbird

> Status: **tested in a guided interop session** (Thunderbird 153.3.1,
> 2026-09-28): discovery, app-password auth, events including recurrence with
> single-occurrence edits, tasks, journals, and cross-calendar moves all
> work; see [COMPATIBILITY.md](../COMPATIBILITY.md). CardDAV contacts not yet
> exercised.

## Setup

1. Create an **app password**: web UI → **Credentials** → app passwords. Use
   it for all DAV connections.
2. Calendar: **File → New → Calendar → On the Network → CalDAV**.
   - Location: `https://<your-host>/` (Thunderbird follows `.well-known`
     discovery)
   - Username: your Daymark username
   - Check "Cache" (offline support)
3. Address book: **Address Book → New Address Book → CardDAV**,
   URL `https://<your-host>/contacts/<username>/<book-slug>/` or just
   `https://<your-host>/` and let discovery resolve it.

## Notes

- Thunderbird sends folded lines and timezone definitions Daymark parses
  explicitly (custom `VTIMEZONE` definitions are stored per calendar and
  honored during recurrence expansion — custom tzids never silently become
  UTC).
- Tasks: enable Tasks on the calendar to see VTODO entries in Thunderbird's
  Tasks tab.
- Journals: Thunderbird's calendar can hold VJOURNAL entries; enable Journals
  on the Daymark calendar.