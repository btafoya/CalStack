# Tasks (VTODO) and Journals (VJOURNAL) Design

Implements `docs/TASKS_JOURNALS_REQUIREMENTS.md`. Design date 2026-09-19. Nothing is deployed, so no data-compatibility constraints: migrations may assume empty tables. ADR-015 (supersedes ADR-011) is drafted at the end and lands in `docs/DECISIONS.md` at implementation start.

## 0. Read this first: what changed since the requirements

Spikes were run against a real build (throwaway Postgres 16 container, debug build of `calendar-server`) and against the pinned dav-server 0.11.0 source. They contradict or refine several requirement assumptions; deviations and remaining judgment calls are in section 12.

| Finding | Evidence | Effect |
|---|---|---|
| MKCALENDAR ignores its request body: component set, displayname, description | Spike: `MKCALENDAR` with `VTODO`-only set + displayname "My Tasks" returned 201; PROPFIND showed all four components and displayname `tasks` (the slug). dav-server 0.11 `handle_mkcalendar` names its body param `_body`. | MKCALENDAR must be intercepted in `dav::entry`. |
| `supported-calendar-component-set` is hardcoded | dav-server `handle_props.rs` emits VEVENT/VTODO/VJOURNAL/VFREEBUSY for every calendar. `get_props` (dead props) cannot override built-in props. | PROPFIND responses need a targeted rewrite. |
| `calendar-query` ignores component filters | Spike: `comp-filter VTODO` over a collection holding one VEVENT returned the VEVENT. dav-server tests only that the outer `VCALENDAR` name appears in the content; time-range and prop-filter are unimplemented. | `calendar-query` must be intercepted, or mixed collections leak events to task clients and vice versa. |
| VEVENT PUT into a VTODO-only collection succeeded (201) | Spike step 5. | Component set must be enforced on PUT. |
| Non-UUID filenames fail with **409 Conflict** | Spike step 4 (`PUT .../my-event-uid.ics`). `parse_location` returns None, dav-server maps NotFound on PUT to 409. | Confirms requirement decision 5. |
| Floating date-times are rejected for events | `points_to_core` uses `try_into_utc()`, None for floating → `MissingDtstart` → PUT 403. Moderate confidence (icalendar crate behaviour from memory, not run). | Tasks must not inherit this: floating DUE is common. Section 3. |
| Same-server attendees do **not** get a copy of an event | `schedule_requests` only records outbound email REQUESTs; no CANCEL path exists in code; no internal delivery anywhere. | Requirement decision 10 was based on a false premise. Re-decided with the user: build internal delivery and CANCEL for events and tasks (12.1). |
| Apple Reminders over CalDAV is legacy-mode since iOS 13 | [BusyMac](https://www.busymac.com/docs/faqs/112990-reminders-in-ios-13-and-macos-catalina-drops-support-for-caldav/) ("Reminders in iOS 13 drops support for CalDAV"), read from a search snippet only. Confidence moderate-low. | Subtasks/sections likely not available to Reminders over CalDAV. Requirements client table overstated it. |

## 1. Component layout

```
                     ┌───────────────────────── calendar-server ──────────────────────────┐
 CalDAV clients ───▶ │ dav::entry ── intercepts: MKCALENDAR, PROPFIND (rewrite),          │
                     │               REPORT calendar-query / sync-collection, PUT gate    │
                     │ tasks_api.rs, journals_api.rs, calendars_api.rs(+components)        │
                     │ jobs.rs (alarm_scan, task_due, retention), scheduling.rs (iTIP)     │
                     └───────┬───────────────────────────────┬────────────────────────────┘
                             │                               │
                  calendar-caldav                       calendar-db
                  PgDavFs (href-addressed)              objects.rs   (calendar_objects view, href lookup)
                  parse_resource / *_to_ics             tasks.rs, journals.rs, alarms.rs(+task_alarms)
                  todo.rs, journal.rs                   scheduling.rs(+task messages), search.rs, backup.rs
```

No new crates. One dependency promotion: `xmltree 0.12` is already in `Cargo.lock` (via dav-server); it becomes a direct dependency of `calendar-server` for the new intercepts (precedent: `calcard` in the CardDAV design).

New files: `migrations/0009_tasks_journals.sql`, `calendar-db/src/{objects,tasks,journals}.rs`, `calendar-caldav/src/{todo,journal}.rs`, `calendar-server/src/{tasks_api,journals_api,xml.rs}`, `calendar-web/src/assets/js/{tasks,journals}.js`.

## 2. Key design decisions

- **D1 Separate typed tables** (`tasks`, `journals`). Not `events`, not one polymorphic table: each keeps its own CHECK constraints (status vocabularies, start/due rules) instead of loosening events'.
- **D2 Unmodelled properties are preserved, not blobbed.** Each task/journal row carries `extra_props jsonb` (array of `{name, params, value}`) for properties the model does not cover (X-*, ATTACH, GEO, COMMENT, RELATED-TO other than PARENT, ...). Emitted verbatim after modelled properties. Normalized columns stay the source of truth; the ICS is never stored. Events are not changed (they still drop X- properties).
- **D3 One resource = one series.** Master plus its RECURRENCE-ID overrides share a UID and one `.ics` (RFC 4791 4.1). Only master rows have an `href`; override rows hang off `master_task_id`. Etag of the resource = master row etag, bumped whenever any override changes.
- **D4 Addressing by stored href** (requirement decision 5). Every event, task and journal master has `href text` (the last path segment as the client chose it), unique per calendar across all three types via a view. API-created objects default to `{id}.ics`.
- **D5 `calendar_objects` view** unions the three tables (id, calendar_id, href, kind, uid, etag, timestamps, deleted_at). One query serves href resolution, `read_dir`, `calendar-query` and `sync-collection`; no per-type special-casing in the DAV layer.
- **D6 Component set is a column** on `calendars`, enforced on PUT and advertised via a PROPFIND rewrite. Upstream fix (`DavMetaData::calendar_components`) is a follow-up, not a dependency.
- **D7 Own the REPORTs and MKCALENDAR that dav-server gets wrong.** New `xml.rs` (xmltree, namespace-agnostic local-name matching) replaces the string-search helpers in `dav.rs` (`xml_element_text` looks for `<sync-token`, which never matches `<D:sync-token>`; a prefixed client re-syncs from token 0 every time).
- **D8 Recurring-task completion is RFC-pure.** Completing an occurrence writes a RECURRENCE-ID override (`STATUS:COMPLETED`, `COMPLETED:<now>`); the master is untouched; "next open occurrence" is derived by expansion. See risk R2 (client interop unverified).
- **D9 Journals are the small case.** Modelled: summary, first DESCRIPTION, DTSTART, STATUS, CLASS, CATEGORIES, URL. Everything else (extra DESCRIPTIONs, ORGANIZER, ATTENDEE, RRULE/RDATE/EXDATE, RELATED-TO) round-trips through `extra_props`, one VJOURNAL per resource. Rules trigger data and UI cover only modelled fields.

## 3. Data model: migration 0009

```sql
-- collections and addressing
ALTER TABLE calendars ADD COLUMN components text[] NOT NULL
    DEFAULT ARRAY['VEVENT','VTODO','VJOURNAL'],
  ADD CONSTRAINT calendars_components_chk CHECK (
    cardinality(components) BETWEEN 1 AND 3
    AND components <@ ARRAY['VEVENT','VTODO','VJOURNAL']);

ALTER TABLE events ADD COLUMN href text;
UPDATE events SET href = id::text || '.ics';
ALTER TABLE events ALTER COLUMN href SET NOT NULL;
CREATE UNIQUE INDEX events_href_idx ON events(calendar_id, href);
ALTER TABLE events ADD COLUMN floating boolean NOT NULL DEFAULT false;  -- same meaning as tasks.floating

CREATE TABLE tasks (
  id uuid PRIMARY KEY,
  calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
  uid text NOT NULL,
  href text,                                        -- series master only
  master_task_id uuid REFERENCES tasks(id) ON DELETE CASCADE,
  recurrence_id timestamp, recurrence_id_date date, -- override rows only

  starts_at timestamptz, start_date date,           -- DTSTART (optional)
  due_at timestamptz,    due_date date,             -- DUE (optional)
  duration interval,                                -- alternative to DUE
  tzid text,
  floating boolean NOT NULL DEFAULT false,          -- wall clock stored as if UTC; emitted without Z/TZID
  completed_at timestamptz,                         -- COMPLETED, always UTC on the wire

  rrule text, rdate jsonb NOT NULL DEFAULT '[]', exdate jsonb NOT NULL DEFAULT '[]',

  summary text NOT NULL DEFAULT '',
  description_html text, description_text text, url text, location text,
  status text CHECK (status IN ('NEEDS-ACTION','IN-PROCESS','COMPLETED','CANCELLED')),
  percent_complete smallint CHECK (percent_complete BETWEEN 0 AND 100),
  priority smallint CHECK (priority BETWEEN 0 AND 9),
  class text CHECK (class IN ('PUBLIC','PRIVATE','CONFIDENTIAL')),
  categories text[] NOT NULL DEFAULT '{}',
  parent_uid text,                                  -- RELATED-TO;RELTYPE=PARENT (RELTYPE absent = PARENT)
  sort_order bigint,                                -- X-APPLE-SORT-ORDER, also the web UI manual order
  extra_props jsonb NOT NULL DEFAULT '[]',

  organizer_user_id uuid REFERENCES users(id), organizer_email citext, organizer_name text,
  sequence integer NOT NULL DEFAULT 0, etag text NOT NULL DEFAULT '',
  created_by uuid REFERENCES users(id),
  deleted_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),

  CHECK (NOT (starts_at IS NOT NULL AND start_date IS NOT NULL)),
  CHECK (NOT (due_at IS NOT NULL AND due_date IS NOT NULL)),
  CHECK (NOT (duration IS NOT NULL AND (due_at IS NOT NULL OR due_date IS NOT NULL))),
  CHECK (master_task_id IS NULL OR rrule IS NULL),
  CHECK ((master_task_id IS NULL) = (href IS NOT NULL)),
  CHECK ((master_task_id IS NULL) = (recurrence_id IS NULL AND recurrence_id_date IS NULL)),
  CHECK (NOT (recurrence_id IS NOT NULL AND recurrence_id_date IS NOT NULL))
);
CREATE UNIQUE INDEX tasks_uid_occurrence_idx ON tasks (calendar_id, uid,
    COALESCE(recurrence_id, recurrence_id_date::timestamp, '-infinity'::timestamp));
CREATE UNIQUE INDEX tasks_href_idx   ON tasks(calendar_id, href) WHERE href IS NOT NULL;
CREATE INDEX tasks_due_idx           ON tasks(calendar_id, due_at) WHERE deleted_at IS NULL AND master_task_id IS NULL;
CREATE INDEX tasks_parent_idx        ON tasks(calendar_id, parent_uid) WHERE parent_uid IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX tasks_master_idx        ON tasks(master_task_id);
CREATE INDEX tasks_categories_idx    ON tasks USING gin(categories);
ALTER TABLE tasks ADD COLUMN search_vector tsvector GENERATED ALWAYS AS
    (to_tsvector('simple', coalesce(summary,'') || ' ' || coalesce(description_text,''))) STORED;
CREATE INDEX tasks_search_idx ON tasks USING gin(search_vector);

CREATE TABLE task_attendees (  -- same shape as event_attendees, FK task_id, UNIQUE (task_id, email)
);
CREATE TABLE task_alarms (     -- same shape as event_alarms, FK task_id; related END means DUE
);

CREATE TABLE journals (
  id uuid PRIMARY KEY,
  calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
  uid text NOT NULL, href text NOT NULL,
  starts_at timestamptz, start_date date, tzid text,
  floating boolean NOT NULL DEFAULT false,          -- DTSTART optional: undated = a note
  summary text NOT NULL DEFAULT '',
  description_html text, description_text text, url text,  -- first DESCRIPTION only (D9)
  status text CHECK (status IN ('DRAFT','FINAL','CANCELLED')),
  class text CHECK (class IN ('PUBLIC','PRIVATE','CONFIDENTIAL')),
  categories text[] NOT NULL DEFAULT '{}',
  extra_props jsonb NOT NULL DEFAULT '[]',
  sequence integer NOT NULL DEFAULT 0, etag text NOT NULL DEFAULT '',
  created_by uuid REFERENCES users(id),
  deleted_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  CHECK (NOT (starts_at IS NOT NULL AND start_date IS NOT NULL)),
  UNIQUE (calendar_id, uid), UNIQUE (calendar_id, href)
);
-- + categories gin, search_vector gin, (calendar_id, starts_at) live index

CREATE VIEW calendar_objects AS
  SELECT id, calendar_id, href, 'VEVENT'   AS kind, uid, etag, created_at, updated_at, deleted_at FROM events
  UNION ALL
  SELECT id, calendar_id, href, 'VTODO',    uid, etag, created_at, updated_at, deleted_at FROM tasks WHERE master_task_id IS NULL
  UNION ALL
  SELECT id, calendar_id, href, 'VJOURNAL', uid, etag, created_at, updated_at, deleted_at FROM journals;

-- iTIP log serves events and tasks; outbound dedupe includes the sequence
ALTER TABLE schedule_messages ALTER COLUMN event_id DROP NOT NULL,
  ADD COLUMN task_id uuid REFERENCES tasks(id) ON DELETE CASCADE,
  ADD COLUMN sequence integer,
  ADD CONSTRAINT schedule_messages_subject_chk CHECK (num_nonnulls(event_id, task_id) = 1);
CREATE UNIQUE INDEX schedule_messages_subject_idx ON schedule_messages (
    COALESCE(event_id, task_id), attendee_email, method, direction,
    COALESCE(sequence, -1), COALESCE(message_id, ''));

-- delivered copies (internal scheduling)
ALTER TABLE events ADD COLUMN origin_id uuid REFERENCES events(id) ON DELETE SET NULL;
ALTER TABLE tasks  ADD COLUMN origin_id uuid REFERENCES tasks(id)  ON DELETE SET NULL;
CREATE INDEX events_origin_idx ON events(origin_id) WHERE origin_id IS NOT NULL;
CREATE INDEX tasks_origin_idx  ON tasks(origin_id)  WHERE origin_id IS NOT NULL;
```

Rules that constraints cannot carry, enforced in `calendar-db`:
- Href uniqueness **across** types in one calendar (the view cannot have an index): checked in the insert transaction; the per-table unique indexes catch same-type races.
- Any change to an override row updates the master's `sequence`, `updated_at` and etag in the same transaction (D3).
- Soft-deleting a master soft-deletes its overrides; deleting a task soft-deletes its subtasks (recursive CTE over `parent_uid` within the calendar), one `change_log` row each so clients see every deletion (requirement decision 8).
- Reparenting rejects cycles.
- Component removal (`PATCH /api/calendars/{id}` changing `components`) is refused with 409 and per-type counts while live items of a removed type exist (decision 9).

`change_log`: `resource_type` gets `'task'` / `'journal'`, `resource_id` = master id. `append_change` gains a `resource_type` argument (today it never sets it; the default `'event'` applies).

## 4. Wire mapping (`calendar-caldav`)

- `parse_resource(text) -> Result<ParsedResource, IcsError>` where `ParsedResource` is `Events(Vec<ParsedEvent>) | Todos(ParsedTodoSeries) | Journal(ParsedJournal)`. It enforces one component kind and one UID per resource; VTIMEZONE is ignored as today. `parse_ics` keeps its event-only contract for existing callers (`scheduling.rs`) but stops erroring on VTODO. `IcsError::TodoUnsupported` is removed; new variants: `MixedComponents`, `UidMismatch`, `UnsupportedRange` (RECURRENCE-ID `RANGE=THISANDFUTURE`, unsupported for events today too). Project rule stays: ICS parsing only through this crate.
- **Property mapping (VTODO)**: modelled per the table above; VALARM → `task_alarms`; ATTENDEE/ORGANIZER → `task_attendees` + organizer columns; `RELATED-TO` with RELTYPE PARENT/absent → `parent_uid`, others → `extra_props`; `X-APPLE-SORT-ORDER` → `sort_order`; `X-ALT-DESC;FMTTYPE=text/html` → `description_html` through `sanitize_html` (ADR-005), same as events; everything else → `extra_props`.
- **Time**: date → `*_date`; UTC or TZID → `*_at` + `tzid`; **floating → `*_at` holding the wall clock as if UTC, `floating = true`, exported without `Z`/`TZID`**. Lossless round trip; comparisons against "now" for floating rows use the calendar timezone in the UI layer. The recurrence anchor is `DTSTART` else `DUE` (pragmatic; strict RFC 5545 wants DTSTART on recurring components. Confirm against real Tasks.org output in fixtures).
- **Serialization**: `todos_to_ics(series)` emits master + overrides in one VCALENDAR; `journal_to_ics`. `STATUS` NULL exports as `NEEDS-ACTION` (RFC default). `extra_props` re-emit with 75-octet folding.
- **Injection safety**: an `extra_props` entry is accepted only if its name matches `^[A-Za-z0-9-]+$`, its parameters/values contain no CR/LF (the parser has already unfolded), and the row's total `extra_props` size is ≤ `ATTACHMENT_MAX_BYTES`. Exceeding it fails the PUT with 403 and the CalDAV `max-resource-size` precondition.

## 5. DAV layer changes (`calendar-server/src/dav.rs`, `calendar-caldav/src/adapter.rs`)

`entry` order after change:

```
resolve_auth
 ├─ MKCALENDAR  → parse body (xmltree): displayname, calendar-description,
 │                supported-calendar-component-set (default all three); create calendar; 201
 ├─ PUT         → parse_resource; component ∈ calendars.components else 403 + <C:supported-calendar-component/>
 │                (replaces the ADR-011 gate); then dav-server → PgDavFs WriteFile
 ├─ REPORT      → sync-collection (via calendar_objects LEFT JOIN, href from view)
 │                calendar-query  (own implementation, below)
 │                free-busy-query (unchanged; events only)
 │                else → dav-server (calendar-multiget, principal reports)
 └─ PROPFIND    → dav-server, then rewrite supported-calendar-component-set per collection href
```

- **`Location::Object(slug, filename)`** replaces `Object(slug, Uuid)`. The filename is percent-decoded once, validated (no `/`, NUL or control characters, ≤ 255 bytes) and resolved by `calendar_objects(calendar_id, href)`. The `.ics` suffix is no longer stripped. Applies to events too.
- **`open` / `resolve` / `read_dir`** dispatch on `kind`. `read_dir` lists events as today (windowed) plus **every** live task and journal, because undated tasks have no window. All three renderers share one `render_object(kind, id)`.
- **`WriteFile::flush`** dispatches on `ParsedResource`; existing/new is decided by href lookup, not by URL uuid; new rows get `Uuid::new_v4()`.
- **Events PUT fixes (Stage 2b, settled 12.6)**: the "exactly one VEVENT" rule (`adapter.rs:706`) becomes "one UID per resource": one master plus its RECURRENCE-ID overrides, upserted in one transaction (overrides not in the PUT are removed, matching the resource-is-a-series rule of D3). Floating DTSTART/DTEND/RECURRENCE-ID are accepted using the `events.floating` flag; `points_to_core` stops discarding them, and recurrence expansion treats a floating series as tzid-less wall clock, consistent with the "as if UTC" storage. Free-busy and alarm scans already read UTC instants, so a floating event is computed in UTC wall time (documented limitation until a per-user timezone is applied).
- **PROPFIND rewrite**: buffer the multistatus (bounded), and for each `<D:response>` whose href is a calendar collection replace the `<C:supported-calendar-component-set>…</C:supported-calendar-component-set>` element with the collection's real set. Names are static strings (`VEVENT|VTODO|VJOURNAL`); no user text is interpolated. A unit test pins the dav-server 0.11 output shape so an upgrade fails loudly.
- **`calendar-query`**: read the first nested `comp-filter` below `VCALENDAR` (kind), default = every kind the collection allows; list matching `calendar_objects`, return `getetag` + `calendar-data`. `time-range` and `prop-filter` are not evaluated: the response is a superset, which is today's behaviour. `ponytail:` add time-range once a client is shown to depend on it (VTODO rules in RFC 4791 9.9).
- **`sync-collection`**: `LEFT JOIN calendar_objects` instead of `events`; the href comes from the view, so soft-deleted rows still resolve. Fix as part of D7: namespace-agnostic parse of `sync-token`.

## 6. Domain behaviour

- **Recurring completion (D8)**: `tasks::complete(task_id, occurrence)` for a recurring series inserts or updates the override row for that occurrence (`STATUS=COMPLETED`, `PERCENT-COMPLETE=100`, `COMPLETED=now`), bumps the master etag, and appends one `change_log` row. `reopen` reverses it. The API's `next_open` is computed with `expand_occurrences` skipping completed overrides. Non-recurring: set status/completed_at/percent on the row. Fires `task_completed` rules.
- **Subtasks**: children are `SELECT … WHERE calendar_id = $1 AND parent_uid = $2 AND deleted_at IS NULL`. `parent_uid` is a raw client string, so a child synced before its parent still works; the API returns `parent_id` when resolvable.
- **Alarms**: `list_scannable_alarms` becomes a union returning a common `ScanRow` (subject kind, id, calendar, anchor start/end or due, recurrence, tzid, summary, alarm). `alarm_scan` is otherwise unchanged; `related=END` maps to DUE for tasks. Skips: tasks with STATUS COMPLETED/CANCELLED, completed occurrences, relative alarms with no DTSTART/DUE. Absolute triggers always work (Apple-style "remind me at"). Notification `data` gains `task_id`; dedupe key format unchanged.
- **Rules**: `run_rules` already takes a free-string `trigger_type`. New values: `task_created`, `task_completed`, `task_due`, `journal_created`, `journal_updated`. `rule_executions.subject_type` is hardcoded `'event'` (`rules_api.rs:144`); it becomes a parameter. `task_due` is emitted from a `task_due_scan` step inside the existing `alarm_scan` tick, deduped by `(task_id, due instant)`. The rules UI trigger list gains the five values.
- **Retention**: `retention_purge` also deletes soft-deleted tasks and journals. It currently issues `DELETE FROM webauthn_challenges`, a table migration 0008 dropped: the job fails every run (seen in the spike server log) and the notifications sweep after it never executes. That statement is removed as part of this work.
- **Search**: `search_tasks` / `search_journals` beside `search_events`; `GET /api/search` gains `tasks` and `journals` arrays (additive; the existing top-level array is unchanged).
- **Backup**: `export`/`import` gain `tasks`, `task_attendees`, `task_alarms`, `journals` (`extra_props` included). Calendars export their `components`.
- **Public feeds**: `public_feed` adds tasks and journals with `class = PUBLIC` or NULL only, **without** `extra_props`, attendees or alarms (extras can carry private data such as `X-APPLE-STRUCTURED-LOCATION`).
- **Free-busy**: untouched; it reads `events` only, so tasks and journals cannot contribute. A test asserts it.
- **Scheduling for tasks and events (settled, 12.1)**: one dispatcher, `scheduling::dispatch(kind, id)`, replaces `schedule_requests` and is called from the same places (API create/patch/delete, DAV `flush`, DAV DELETE). It runs only for rows where `origin_id IS NULL` and the writer is the organizer. Per non-organizer attendee, it diffs against `schedule_messages` history by `(attendee, sequence)`:
  - **Internal attendee** (`user_id` set, or `users.email` matches): *deliver a copy* into that user's default collection for the kind (first calendar they own, by `order_index, slug`, whose `components` include the kind), in one transaction with the copy's `change_log` row and ctag bump, plus an `in_app` notification. **No email** is sent to internal attendees. If they own no suitable collection, they are treated as external.
  - **External attendee**: email REQUEST (new attendee), updated REQUEST (SEQUENCE bumped since last send), CANCEL (attendee removed or object deleted). `send_pending` branches on `event_id` / `task_id` and renders `METHOD:REQUEST|CANCEL` with `events_to_ics` / `todos_to_ics`.
  - Outbound dedupe is by `(subject, attendee, method, sequence)`; this also repairs the event path (see the pre-existing issues table).
- **Delivered copies**: rows in the attendee's calendar with `origin_id` pointing at the organizer's row, same UID, same ORGANIZER, rebuilt from the origin on every dispatch (recurring series include their overrides). The organizer's fields win: a PUT/PATCH to a copy applies only the attendee's **own PARTSTAT** and alarms; the rest is discarded and the new etag makes the client refetch. Deleting a copy = declining locally: sets that attendee's PARTSTAT to `DECLINED` on the origin, then soft-deletes the copy. Cancelling (organizer deletes, or removes the attendee) sets the copy to `STATUS:CANCELLED`, bumps SEQUENCE, keeps it visible (RFC 5546), and notifies.
- **Replies**: `scheduling::reply(subject, attendee_email, partstat)` is the single primitive. Internal: called when a copy's PARTSTAT changes. External: called by `postmark_inbound`, which now looks the UID up in `events` **and** `tasks`. For tasks only PARTSTAT is applied; status or percent from the assignee is ignored. The primitive updates the attendee row **and touches the parent row** (`sequence` unchanged, `updated_at`, etag, `change_log`, ctag) so the organizer's clients see the RSVP on their next sync. Today `update_partstat` changes only `event_attendees`, so RSVPs are invisible to sync.

## 7. Application API

Scopes and ACL checks are unchanged (`require_capability`, existing token-scope middleware). Concurrency uses the same ETag / `If-Match` mechanism as `patch_event`.

```
GET/POST   /api/calendars/{id}/tasks       list (?status,due_before,due_after,category,parent_id,q) / create
GET/PATCH/DELETE /api/tasks/{id}            get / update (incl. parent_uid, sort_order) / soft delete + subtask cascade
POST       /api/tasks/{id}/complete         body {occurrence?}; recurring → override per D8
POST       /api/tasks/{id}/reopen
GET/POST   /api/calendars/{id}/journals    list (?from,to,undated,category,q) / create
GET/PATCH/DELETE /api/journals/{id}
PATCH      /api/calendars/{id}              + components (refused with 409 + counts while occupied)
GET        /api/calendars/{id}/occurrences  + ?include=tasks,journals  (dated markers; feeds the calendar view)
GET        /api/search                      + tasks[], journals[]
```

Task view adds `subtasks_count`, `next_open` (recurring), `is_overdue`. `calendar-api` OpenAPI is hand-maintained; add `Task`, `Journal` schemas and the routes above plus `components` on `Calendar`. Attachments on tasks are out of scope (inline `ATTACH` round-trips through `extra_props`).

## 8. Web UI

Follows the page + handler + `ASSETS` pattern, `api()` helper, jQuery 4 + Bootstrap, no new framework.

- **`/tasks`** (`TASKS_PAGE`, `tasks.js`): calendar selector (only calendars whose set includes VTODO), filter bar (status, due, priority, category), tree list with subtask indent and expand/collapse, quick-add row, completion checkbox, edit modal (summary, rich description via summernote/sanitized HTML, start, due, priority, percent, status, categories, recurrence editor, reminders, assignees), drag reorder writes `sort_order`. Follows the modal-safety fixes in commit `407859d` (no native dialogs, no data loss on close, no double submit).
- **`/journals`** (`JOURNALS_PAGE`, `journals.js`): chronological feed grouped by month, an "undated notes" list, editor modal (summary, rich text, date optional, status, categories).
- **Calendar view** (`app.js`): `occurrences?include=tasks,journals` adds markers with a `type` field; a per-calendar toggle (requirement decision 7) shows tasks (checkbox glyph, completable in place) and dated journals (book glyph). Undated items never appear.
- **Calendars page**: component checkboxes on create/edit; the 409 message from component removal is shown verbatim.

## 9. Security

- Same `resolve_auth` path and `token_scope_guard` as events; DAV never accepts session cookies (existing rule).
- New user-controlled text reaches three sinks and each is handled: ICS output (injection rules in section 4), HTML descriptions (`sanitize_html`), and the PROPFIND rewrite (static strings only).
- Public feeds withhold `extra_props`, attendees, alarms and non-PUBLIC classes.
- Never log summaries, descriptions, `extra_props` or attendee addresses (existing PII rule).
- Bounded request bodies: MKCALENDAR/calendar-query bodies are parsed with a size cap and xmltree's default entity handling (no external entities).
- **Dev-only request capture** (`DAV_CAPTURE_DIR`, unset by default): when set, `dav::entry` writes one file per request (method, path, headers with `Authorization` and `Cookie` redacted, body). Captures contain calendar content, so the directory must be a throwaway; the server logs a warning at startup while capture is on, and the variable is not documented in `.env.example`.

## 10. Testing

- **Unit**: parse/serialize round trips (folded lines, all-day, TZID, floating, UTC; VTODO with no DTSTART/DUE/ORGANIZER; X- properties surviving; journal with two DESCRIPTIONs; journal with no DTSTART); recurrence completion and `next_open`; cycle rejection; component-set constraint; the pinned PROPFIND shape.
- **Integration** (docker `postgres:16-alpine`, the same rig as the spike; `tests/interop/run.sh` needs local PG 16 binaries that this workstation lacks, so run it in CI or adapt it to docker): MKCALENDAR honours body; PROPFIND shows the real set; PUT VTODO/VJOURNAL round trip and 403 outside the set; non-UUID filename PUT/GET/DELETE for all three kinds; `calendar-query` per component; incremental `sync-collection` with prefixed and default-namespace bodies; subtask cascade appears in sync as deletions; component removal 409; free-busy ignores tasks.
- Events: PUT of a master plus two overrides in one resource round-trips and syncs as one href; floating DTSTART/DTEND/RECURRENCE-ID round-trips unchanged; removing an override from the PUT removes the override row.
- The existing `vtodo_rejected` unit test and the interop step "VTODO PUT rejected 403" are replaced by the component-set tests.
- **Interop matrix** (manual, golden fixtures under `tests/interop/fixtures/`): Reminders, Tasks.org, jtx Board, Thunderbird; per client capture MKCALENDAR/PUT/REPORT traffic, filenames sent, recurring-completion representation, parent complete/delete behaviour.

## 11. Stages

```
Stage 0: remove the dead webauthn_challenges DELETE (one line) — verify: retention_purge job succeeds
Stage 1: migration 0009 + objects/tasks/journals db modules + component-set column — verify: db tests, make verify
Stage 2: DAV layer: xml.rs, MKCALENDAR, PROPFIND rewrite, calendar-query, sync via view, href addressing (events only) — verify: integration tests; ships fixes for events already
Stage 2b: events PUT accepts master + overrides in one resource, and floating times (events.floating) — verify: PUT/GET round trips with real-shaped Apple and DAVx5 bodies
Stage 2c: DAV_CAPTURE_DIR request capture, then a fixture session: you run Reminders, Tasks.org/jtx Board (DAVx5) and Thunderbird against a dev instance through a scripted checklist (create list, add task, subtask, complete recurring, delete parent, add note/journal, edit). Captures become golden fixtures and settle R2/R3/R4 before Stage 3 starts
Stage 3: VTODO wire: parse/serialize, PUT/GET, alarms table, extras, floating — verify: round trips + fixtures
Stage 4: VJOURNAL wire — verify: round trips
Stage 5: API + OpenAPI + search/backup/feeds/retention — verify: API tests, openapi validation test
Stage 6: recurrence completion, subtasks, alarms + task_due, rules triggers — verify: unit + job tests
Stage 7: web UI — verify: manual + playwright smoke
Stage 8a: scheduling::dispatch + reply primitive + schedule_messages/origin_id migration, events only (email CANCEL/updates, PARTSTAT visible to sync) — verify: event scheduling tests, no regression in existing interop steps
Stage 8b: internal delivery for events (copy, cancel, decline, own-PARTSTAT-only edits) — verify: two-user integration tests
Stage 8c: the same for tasks (assignment) — verify: two-user + inbound REPLY tests
Stage 9: ADR-015, docs, compatibility matrix, interop run
```

Each stage passes `make verify` (fmt, check, clippy `-D warnings`, test) and the docker integration run before the next starts. Stages 0 and 2 have standalone value and could ship first.

## 12. Decisions taken in design and deviations from the requirements

1. **iTIP scope (settled: tasks and events both).** Requirement decision 10 promised same-server assignees "get the task in their default task collection automatically, as events do today". Events do not do that, and send no CANCEL. Chosen: internal delivery, CANCEL and updates for **both** events and tasks (section 6). This makes Stage 8 a feature in its own right: it changes existing event behaviour (an internal attendee now receives a copy and no email; RSVPs become visible to sync). Consequences accepted with the choice: attendee edits to a delivered copy are limited to own PARTSTAT and alarms; "default collection" is the first owned calendar that allows the kind; local users are never emailed.
2. **Journals modelled narrowly (D9, confirmed by the user 2026-09-19).** ORGANIZER, ATTENDEE, RRULE, RDATE, EXDATE, RELATED-TO and extra DESCRIPTIONs survive verbatim but are not queryable or editable; RECURRENCE-ID overrides on journals are not supported. Requirement F11 listed them as fields. Cheap to widen later; no known client needs more.
3. **Apple Reminders subtasks** over CalDAV are probably unavailable on iOS 13+; the requirements' client table listed them as a target. Reminders stays a primary target for flat lists; subtasks are exercised through Tasks.org and jtx Board.
4. **Recurring-task anchor** falls back to DUE when DTSTART is absent (RFC 5545 asks for DTSTART). Verify with Tasks.org fixtures.
5. **Existing-calendar default** stays all three (decision 3); with nothing deployed this is a schema default only.
6. **Event PUT gaps are in scope (settled 2026-09-19).** Master + overrides in one resource, and floating times, for events (Stage 2b). This widens the feature beyond tasks and journals, on the reasoning that Stage 2 already reworks the PUT path and addressing.
7. **Fixtures come from live clients (settled 2026-09-19).** The user connects Reminders, Tasks.org/jtx Board via DAVx5 and Thunderbird to a dev instance with capture on (Stage 2c). Stage 3 does not start until the captures settle recurring completion (R2), `extra_props` fidelity (R3) and client quirks (R4). If a client is unavailable, its row in the interop matrix stays unverified rather than assumed.

### Pre-existing issues found (not caused by this feature)

| Issue | Disposition |
|---|---|
| `retention_purge` fails every run (`webauthn_challenges` dropped in 0008) | Fix in Stage 0 |
| `xml_element_text` misses prefixed tags; prefixed sync-collection clients always full-resync | Fixed by D7 |
| MKCALENDAR displayname ignored; a new calendar is named after its slug | Fixed in Stage 2 |
| CalDAV PUT requires exactly one VEVENT, so a recurring event plus its RECURRENCE-ID overrides in one resource (what Apple and DAVx5 send) is refused with 403 | Fixed in Stage 2b (settled 12.6) |
| Floating DTSTART on events is rejected | Fixed in Stage 2b (settled 12.6) |
| `schedule_messages` unique key includes a NULL `message_id`, which Postgres treats as distinct, so each `schedule_requests` call likely re-queues REQUESTs (high confidence from Postgres semantics, not tested) | Fixed in Stage 8a (sequence in the key) |
| `update_partstat` changes only `event_attendees`: no etag, `updated_at` or `change_log` touch, so an RSVP never reaches the organizer's CalDAV clients (confirmed from code) | Fixed in Stage 8a (reply primitive touches the parent row) |
| `update_event` bumps `sequence` on every edit, not only material ones (`lib.rs:1016`) | Kept; every edit re-sends an updated REQUEST to external attendees, deduped by sequence |
| `calendar-query` ignores `time-range`/`prop-filter` | Kept as superset (section 5) |

## 12a. Implementation log

| Stage | Status | Deviations from this design |
|---|---|---|
| 0 | Done | none |
| 2 (partial) | Done: components column, stored hrefs for events, MKCALENDAR body, PROPFIND component-set rewrite, PUT component gate, namespace-agnostic XML (`xml.rs`), percent-encoded sync hrefs. Checks: `tests/interop/components_hrefs.sh` (`make interop-docker`), unit tests in `dav.rs`, `store.rs`, `xml.rs`, `adapter.rs`. | `events.href` is **nullable** (NULL = `{id}.ics`) with a unique index on `COALESCE(href, id::text \|\| '.ics')`, so API-created events and canonical PUTs need no href. The `calendar_objects` view and the `calendar-query` intercept are deferred to Stage 3, when a second object type exists (no scaffolding ahead of a consumer). The view must use the same `COALESCE`. MKCALENDAR is applied *after* dav-server creates the collection (reuses its slug validation and errors) instead of being reimplemented. |
| 2b | Done: `ics_upsert::put_series` stores master + overrides in one transaction with one `change_log` row; overrides are part of the master's resource (hidden from listings, rendered into its GET, their API changes refresh the master's etag and report the master); floating date-times (`events.floating`); a soft-deleted UID is brought back by a new PUT. Checks: `tests/interop/components_hrefs.sh` (series, floating, delete/re-PUT sections) and `floating_times_round_trip_without_zone`. | Migration is `0010_events_floating.sql` (not folded into 0009, which may already be applied). Limits accepted: a PUT **replaces** the override set, so override rows get new ids and any API-attached data on an override (attachments) is dropped; a resource with only overrides (an invitation to a single occurrence) is refused with 403; RECURRENCE-ID `RANGE=THISANDFUTURE` is still unsupported; the backup export/import does not carry `floating`; floating times compare as UTC wall time in alarm scans and free-busy. Verified: without the floating arm in `points_to_core`, a floating DTSTART fails with `MissingDtstart` (the earlier "moderate confidence" claim is now confirmed). |
| 2c | Done: `DAV_CAPTURE_DIR` request capture (`capture.rs`, an outermost layer only when the variable is set; DAV paths only, never `/api`; `Authorization`/`Cookie` redacted; response status appended), `tests/interop/capture-dev.sh` (throwaway Postgres + server + disposable account), `docs/INTEROP_CAPTURE.md` (per-client checklist). Checks: 3 unit tests plus the capture section of `components_hrefs.sh`. | The session script binds **loopback by default**: the host has a public IP and the server has open registration, so exposure (`BIND_ADDR=0.0.0.0`) is opt-in with a warning. Tasks are still 403'd until stage 3, so the checklist has each scenario use its own item and do follow-up steps offline so the first upload carries the final state. |

Found and fixed while implementing (both pre-existing): `PROPPATCH displayname` stored the raw element text including an XML declaration, and every MKCALENDAR wrote dav-server's default description as escaped XML (`xml_text` in the adapter). The check script initially passed vacuously when the server failed to start; it now aborts on a dead server and guards the description check.

Also fixed in 2c (pre-existing): `/.well-known/caldav` and `/.well-known/carddav` answered only GET, so a client that starts discovery with PROPFIND (RFC 6764) got 405. They now answer any method with a 308 (method-preserving) to the mount.

Next: the fixture session (yours to run, see `docs/INTEROP_CAPTURE.md`), then Stage 3.

## 13. Risks

- **R1 PROPFIND rewrite** depends on dav-server 0.11's exact XML shape. Mitigation: pinned version, pinned-shape test, upstream change as follow-up.
- **R2 Recurring completion interop.** Tasks.org appears to expect RECURRENCE-ID overrides (another client's separate-VTODO approach produced duplicate open tasks in [tasks/tasks#1261](https://github.com/tasks/tasks/issues/1261)); the ecosystem has no consensus (python-caldav#127). Gate Stage 6 on real fixtures from all four clients; fallback is advancing DTSTART/DUE in place.
- **R3 `extra_props` fidelity.** icalendar's parser may normalize parameter quoting. Round-trip tests with real client output decide whether verbatim re-emission needs a custom writer.
- **R4 Client quirks**: Apple alarm X-props, all-day vs floating handling, filenames. Fixtures first (Stage 2 exit criterion for events, Stage 3 for tasks).
- **R5 Rendering cost**: `read_dir` renders every object to learn its length. Fine for tasks and journals at expected volumes; the existing event `ponytail:` note applies.
- **R6 Cascade fan-out**: deleting a large task tree writes one `change_log` row per node. Bounded by tree size; acceptable.
- **R7 Internal delivery is the riskiest stage.** It rewrites how events with attendees behave. Risks: delivery loops (guarded by `origin_id IS NULL` + organizer-only dispatch), copy/origin drift on concurrent edits (copy is rebuilt from origin, organizer wins), recurring series with overrides (copy rebuilt as a whole), and a client repeatedly re-PUTting a discarded edit. Ship 8a and 8b behind two-user integration tests before 8c, and expect this stage to need its own design review if interop shows attendee-edit conflicts.

## ADR-015 (draft): Tasks and journals are stored as typed relational components

VTODO and VJOURNAL are first-class stored components (`tasks`, `journals`) with per-collection component sets. Supersedes ADR-011. ADR-011's reasoning still holds for opaque storage: unmodelled properties are kept in `extra_props` beside normalized columns, never as an ICS blob. Resources are addressed by client-chosen href; a resource is a whole series (master plus RECURRENCE-ID overrides).
