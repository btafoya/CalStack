# Tasks (VTODO) and Journals (VJOURNAL) Requirements

Requirements discovery for full VTODO and VJOURNAL support. Status: brainstorm 2026-09-19, all decisions below settled with the user (decision 10 re-decided after a spike showed its premise was wrong, see its note). Designed in `docs/TASKS_JOURNALS_DESIGN.md`, which also records spike findings and deviations. Supersedes ADR-011.

## Goal

Make CalStack a real CalDAV task and journal store: tasks and journals sync with Apple Reminders, Tasks.org, jtx Board and Thunderbird, and are first-class in the API and web UI. ADR-011 ("VTODO is rejected") said support must arrive "as real support or not at all"; this is that support, extended to VJOURNAL.

## Client targets (all primary, with interop test coverage)

- Apple Reminders (iOS/macOS): VTODO-only lists created by MKCALENDAR with an explicit component set, X-APPLE-SORT-ORDER, alarms. Correction 2026-09-19: subtasks via RELATED-TO were listed here, but Reminders treats third-party CalDAV accounts as legacy lists since iOS 13 (moderate-low confidence); expect flat lists and verify in fixtures.
- Tasks.org via DAVx5: VTODO with subtasks, recurrence, priority, start/due.
- jtx Board via DAVx5: VJOURNAL (journals, and notes with no date) plus VTODO. Only known VJOURNAL consumer.
- Thunderbird: VTODO in calendars. VJOURNAL behaviour unconfirmed; verify, don't assume.

## Decisions (settled)

1. **Scope: both, fully, including web UI for each.** No phasing. (Alternative offered and declined: VTODO first, VJOURNAL second. VJOURNAL has one known client; delivered anyway.)
2. **Collections: per-collection component set.** Each collection declares any combination of VEVENT, VTODO, VJOURNAL. Set via MKCALENDAR body and the web UI, stored, advertised correctly, and enforced on PUT (a component outside the collection's set is rejected with the CalDAV `supported-calendar-component` precondition error).
3. **Migration default: all three, existing and new.** Every existing calendar starts as a mixed collection. Accepted consequence: existing calendars will show up as reminder/task lists in Reminders and Tasks.org until their owner narrows the set. (Alternative offered and declined: existing = VEVENT only.)
4. **Server-implemented task behaviour (all four selected):**
   - Recurring tasks: RRULE on VTODO, server computes the next occurrence on completion, reusing the recurrence engine (ADR-012).
   - Subtask hierarchy: `RELATED-TO;RELTYPE=PARENT` modelled; API and web UI show and edit trees.
   - Task reminders: VALARM on tasks fires through the existing durable job queue and notification providers.
   - Assignment via iTIP: ATTENDEE on tasks with REQUEST/REPLY invitations and inbound iMIP replies. Largest single item; staged last.
5. **Resource filenames: accept any filename, now.** The client's chosen href is stored per resource and resolved by href or UID, replacing the UUID-only `Location::Object` parse. Applies to events too, so existing events need a backfilled href (`{id}.ics`). (Alternative offered and declined: keep UUID-only until interop capture shows a client needs otherwise.)
6. **Rules: task and journal triggers.** Task created/completed/due and journal created/updated, reusing the existing condition and action model.
7. **Calendar view: tasks with DUE and dated journals both render in bs-calendar, toggleable per collection.** Undated notes and undated tasks never appear there.
8. **Parent tasks: completing a parent leaves children untouched; deleting a parent soft-deletes its subtasks.** Cascaded deletes are reported to sync clients like any other deletion. Unverified: whether Apple Reminders and Tasks.org cascade the same way; a client that disagrees is a sync-conflict risk, so capture it in interop testing.
9. **Component removal: refused while the collection still holds items of that type.** The error states how many items block it.
10. **iTIP assignment: full parity with events.** Outbound REQUEST, CANCEL and updates; inbound REPLY via the Postmark webhook (ADR-009); same-server assignees get the task in their default task collection automatically.
    **Correction 2026-09-19:** this was chosen on a false premise. Events today send only email REQUESTs (no CANCEL, no updates path) and never copy an event into a same-server attendee's calendar. **Re-decided: internal delivery, CANCEL and updates for both tasks and events** (design section 6 and 12.1). Adds requirement F21: an attendee's RSVP (email or internal) must be visible to the organizer's CalDAV clients via etag and sync.

## Findings that constrain design

- **`events` cannot host tasks.** It requires a start (`starts_at` xor `start_date`), has `organizer_email NOT NULL` and a status CHECK of TENTATIVE/CONFIRMED/CANCELLED. A VTODO may have no DTSTART, no DUE and no organizer, and uses NEEDS-ACTION/IN-PROCESS/COMPLETED/CANCELLED; VJOURNAL uses DRAFT/FINAL/CANCELLED. Loosening those constraints weakens event guarantees. Storage shape is a `/sc:design` decision.
- **dav-server hardcodes `supported-calendar-component-set`** to VEVENT, VTODO, VJOURNAL, VFREEBUSY for every calendar collection (upstream `handle_props.rs`; confidence moderate, pinned 0.11 not checked locally). Per-collection sets therefore need a PROPFIND override or upstream change. Side effect today: CalStack already advertises tasks and 403s them.
- **Already extensible:** `change_log.resource_type` (default `'event'`), per-calendar ctag, ACLs, etags, soft delete and purge, categories, recurrence engine.
- **VEVENT-only assumptions to revisit:** `parse_ics` returns `Vec<ParsedEvent>`; PUT requires exactly one VEVENT (`adapter.rs:706`); `Location::Object` requires a UUID filename; 403 gate `dav.rs:57`; `retention_purge`, backup export, search, public feeds, alarm worker, rules triggers, iTIP scheduling are event-only; web calendar view is bs-calendar with no task view.
- **Do not copy the CardDAV sync shortcut.** `sync_collection_addressbook` returns a full snapshot each time (a `ponytail:` debt). Tasks and journals share calendars, so they use the incremental `change_log` path.

## Functional requirements

### CalDAV
- F1. MKCALENDAR honours `supported-calendar-component-set`; PROPFIND returns the collection's real set.
- F2. PUT/GET/DELETE of VTODO and VJOURNAL resources with ETag/If-Match; PUT of a component outside the collection's set is rejected per decision 2.
- F3. `calendar-query`, `calendar-multiget` and `sync-collection` cover VTODO and VJOURNAL. `comp-filter` on VTODO works, including the VTODO `time-range` rules of RFC 4791 section 9.9.
- F4. Round-trip fidelity for the properties the target clients send, including unknown and X- properties (X-APPLE-SORT-ORDER and client-specific ones). Mechanism is a design choice; no data loss is the requirement.
- F5. Free-busy excludes tasks and journals.

### Tasks (VTODO)
- F6. Fields: SUMMARY, DESCRIPTION, DTSTART, DUE, DURATION, COMPLETED, PERCENT-COMPLETE, STATUS, PRIORITY, CLASS, CATEGORIES, URL, LOCATION, RRULE/RDATE/EXDATE/RECURRENCE-ID, RELATED-TO, VALARM, ATTENDEE/ORGANIZER, SEQUENCE. DTSTART, DUE and organizer optional.
- F7. Recurrence per decision 4, including per-occurrence overrides via RECURRENCE-ID.
- F8. Subtask trees per decision 4; parent completion and deletion follow decision 8.
- F9. Reminders per decision 4; relative triggers anchor to DUE or DTSTART.
- F10. Assignment per decision 4: outbound REQUEST, inbound REPLY via the existing Postmark webhook (ADR-009).

### Journals (VJOURNAL)
- F11. Fields: SUMMARY, DESCRIPTION (RFC allows multiple; preserve all), DTSTART (optional: undated notes are valid), STATUS (DRAFT/FINAL/CANCELLED), CLASS, CATEGORIES, RELATED-TO, RRULE, ATTENDEE/ORGANIZER.
- F12. No VALARM on journals (not permitted by RFC 5545).

### API and web UI
- F13. OpenAPI endpoints for tasks and journals (CRUD, list/filter, complete, reparent), same auth scopes and ACL rules as events. No new scope names.
- F14. Web UI: tasks view (list, filter by status/due/priority/category, subtask tree, quick-add, complete toggle, recurrence editor, reminders); journals view (chronological feed, undated notes list, rich-text editor); component-set checkboxes in calendar management.
- F15. Search covers tasks and journals. Backup/restore covers them.
- F16. Public share feeds include tasks and journals subject to the existing CLASS privacy rules (private/confidential withheld).
- F17. Rules triggers for task created/completed/due and journal created/updated (decision 6).
- F18. Calendar view renders dated tasks and dated journals with a per-collection toggle (decision 7).
- F19. Removing a component from a collection is refused while items of that type remain (decision 9).
- F20. Resources are addressable by any client-chosen filename (decision 5); existing events keep working at `{id}.ics`.
- F21. An attendee's RSVP (email or internal) is visible to the organizer's CalDAV clients through etag and sync (decision 10).
- F22. Events: one CalDAV resource may hold a recurring event plus its RECURRENCE-ID overrides (added 2026-09-19, design 12.6).
- F23. Events accept floating date-times and round-trip them unchanged (added 2026-09-19, design 12.6).
- F24. Client interop fixtures are captured from live Reminders, Tasks.org, jtx Board and Thunderbird against a dev instance before task storage is built (design 12.7).

## Non-functional requirements

- N1. PostgreSQL remains the only dependency; no new external services. No new crates unless design justifies one.
- N2. Normalized storage is canonical (ADR-002); iCalendar is the wire format. No opaque blob as the data model.
- N3. Task/journal mutations, their `change_log` rows and ctag bump are one transaction (ARCHITECTURE.md).
- N4. Authorization enforced at the resource boundary so API and CalDAV agree.
- N5. Soft delete plus the existing purge job. One retention mechanism.
- N6. ICS parsing stays behind `calendar-caldav::parse_ics`; extend it, don't bypass it.
- N7. Sync must stay incremental: a change to one task must not force a full resync.

## Acceptance criteria (sample)

- MKCALENDAR with `VTODO`-only set yields a collection whose PROPFIND reports exactly VTODO; PUT of a VEVENT into it returns 403 with the `supported-calendar-component` error body.
- PUT a VTODO with no DTSTART, no DUE, no ORGANIZER; GET returns it with unchanged unknown properties and a stable ETag.
- Completing a recurring task in the web UI creates the next occurrence and leaves an ETag/sync-visible change for clients.
- A VALARM on a task due in the future fires once through the job queue and produces a notification.
- Subtask created in Tasks.org appears as a child in the web UI and API; reparenting in the web UI is visible to DAVx5 after sync.
- A VJOURNAL with two DESCRIPTIONs and a VJOURNAL with no DTSTART both round-trip.
- Free-busy over a calendar with tasks and journals returns no busy time from them.
- Interop matrix passes for the four primary clients.

## Assumptions (defaulted, override if wrong)

- A1. Journals get no iTIP (task assignment only).
- A2. Task and journal descriptions follow ADR-005 (sanitized HTML canonical, plain text derived).
- A3. ACL, token scopes, DavAuth path, and calendar-level `free_busy` capability reused unchanged.
- A4. A same-server assignee's "default task collection" (decision 10) is their first collection that includes VTODO; design may refine.

## Open questions

None. Residual items are verifications, listed under spikes.

## Spikes before design

1. PROPFIND a calendar on a running instance to confirm the hardcoded component-set advertisement (finding above).
2. Confirm dav-server 0.11 `calendar-query` handles `comp-filter VTODO` and `VJOURNAL` against `PgDavFs`, or whether it needs interception like `sync-collection`.
3. Capture real MKCALENDAR, PUT and REPORT traffic from Reminders, Tasks.org, jtx Board and Thunderbird as golden fixtures under `tests/interop/fixtures/`. Also record what each client does on parent complete/delete (decision 8) and which filenames each sends (decision 5).

## Non-goals

- No VFREEBUSY storage; free-busy remains computed.
- No hidden VTODO/VJOURNAL blob store (ADR-011 stays right about that).
- No task dependencies beyond RELATED-TO parent/child (no `DEPENDS-ON` scheduling engine).
- No kanban, gantt, time-tracking or other task-manager features beyond what the four clients expose.
- No iTIP for journals.

## Suggested stages (input to `/sc:design`, not a design)

```
Stage 1: spikes + storage + change_log integration + component-set on collections + href addressing (events backfilled)
Stage 2: VTODO CalDAV round-trip (parse/serialize, PUT/GET/REPORT/sync) + fixtures
Stage 3: VJOURNAL CalDAV round-trip
Stage 4: API + OpenAPI for tasks and journals
Stage 5: recurrence, subtasks, reminders
Stage 6: web UI (tasks, journals, component-set management, calendar-view toggle) + rules triggers
Stage 7: iTIP assignment
Stage 8: ADR-015 (supersede ADR-011), docs, PRD/COMPATIBILITY updates, interop matrix
```
