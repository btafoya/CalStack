# CardDAV Requirements

Requirements discovery for full CardDAV support. Status: requirements settled via brainstorm 2026-09-12; not yet designed or implemented.

## Goal

Give every user a real address book that CardDAV clients (Apple Contacts, DAVx5, Thunderbird) can sync, and wire contacts into event scheduling so attendees are picked from contacts instead of typed freehand. Contact phone numbers (especially mobile) are first-class because a Twilio notification channel (push, SMS, email — in that priority order) is planned to read from them.

## Client targets

- Primary (must work end-to-end, including test coverage): Apple Contacts (macOS/iOS), DAVx5 (Android).
- Opportunistic: Thunderbird. Fix if cheap; do not block on it.
- Implied protocol surface: `/.well-known/carddav` discovery, principal `addressbook-home-set`, PROPFIND resourcetype + displayname, ETag/If-Match, `addressbook-multiget` + `addressbook-query` REPORTs, `sync-collection` REPORT with sync-token, vCard 3.0 primary with 4.0 advertised in `supported-address-data`.

## Address book scope: Both (decided)

Two kinds of collections per user:

1. **Tenant directory book (read-only, auto-provisioned)** — one vCard per tenant user (or per visible tenant user; see open questions). Lets any user find colleagues in Apple Contacts/DAVx5 and gives the web UI a canonical attendee source. Not writable via CardDAV; managed by account administration.
2. **Personal address books (editable)** — user-created, full CRUD via CardDAV and web UI, mirroring the calendars page model (create/rename/delete books, per-book ACL later if needed). One default book provisioned with the account.

## Data model: normalized + raw vCard (decided)

`contacts` table with core normalized columns (fn, n components, org, emails with type/primary, tels with type/primary — mobile flagged for Twilio routing, addresses, photo ref), plus the raw vCard text stored verbatim for round-trip fidelity to clients that send properties we don't model. Mirrors the events design rule: canonical normalized store, no opaque blobs as the data model, but raw payload preserved.

- PHOTO: capped `bytea`, same size-cap pattern as event attachments. Apple/DAVx5 photo sync must work; stripping photos causes visible data loss.
- vCard groups (`KIND:group` / `X-ADDRESSBOOKSERVER-KIND:group`) used by Apple Contacts and DAVx5 for contact groups: see open questions.
- Address books themselves get a table + slugs; slug unique within account/tenant namespace like calendar slugs.

## Attendee linkage: loose refs (decided)

Attendee rows gain nullable `contact_id` / `user_id`. ICS output and invite emails snapshot name/email from the contact at write time; deletion of a contact does not cascade into events. Attendees remain independent strings otherwise (PRD rule stays intact). Web UI attendee picker resolves from tenant directory + personal books.

## Notification interplay (new requirement)

Contacts carry typed phone numbers so the planned notification channel chain can route: push (app/session), SMS via Twilio (requires a mobile-classified number), email via configured provider. Reminder/notification preference per contact or per attendee is out of scope for CardDAV itself; only the data (typed TELs, email) must exist.

## Web UI (all selected)

1. Contacts page — list/search/create/edit/delete using existing page+handler+ASSETS pattern, `api()` helper.
2. Attendee autocomplete in event editor — typeahead over contacts + tenant directory.
3. Address book management page — create/delete/rename books, like the calendars page.

## Settled answers (design stage 2026-09-12)

1. vCard output: emit **3.0 always**; advertise 4.0 in `supported-address-data` for clients that opt in. No content negotiation.
2. Directory book: **all tenant users**, visible to every tenant user. No visibility filtering, no admin gating.
3. Groups: **normalize membership** into a members table (contact/user refs where resolvable, raw member URNs otherwise) — Apple Contacts and DAVx5 create groups silently on normal use and rewrite whole addressbooks on sync, so round-trip fidelity is required.
4. Deletion: **soft-delete + configurable retention**, reusing the existing event soft-delete/purge machinery. One mechanism, one purge job.
5. Sync-token: **reuse the per-collection token/ctag mechanism** built for calendars. Every addressbook — personal and directory — participates in sync-collection.
6. (folded into 5: directory book is a normal synced collection, not a static login-refreshed one)
7. Auth/scopes: **reuse the existing read/write/full token scopes and DavAuth path**. No new scope names.

## Non-goals

- No vCard-to-JSCalendar-style mapping; contacts are vCard-native only.
- No global/system contact dedup or merge tooling.
- No new external dependencies beyond enabling `dav-server` `carddav` feature (already in 0.11) and a vCard parser/writer crate (to be chosen in design; hand-rolled per the recurrence precedent is an option if `icalendar`/`vcard` crates fall short on RFC 6350 fidelity — note the existing rule: ICS parsing only through `calendar-caldav::parse_ics`, an analogous rule should apply to vCard parsing).
- Twilio/push notification implementation itself — separate feature; this feature only guarantees the data.