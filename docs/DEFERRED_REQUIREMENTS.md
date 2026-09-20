# Deferred Work Requirements

Requirements for the nine deferred items from the 2026-09-19 project audit.
Each item's decision was confirmed with the owner during requirements
discovery; this document is the agreed spec. Architecture and implementation
detail follow in `/sc:design` and `/sc:workflow` — this file records WHAT and
WHY, not HOW.

## Decision summary

| # | Item | Decision | Phase |
|---|------|----------|-------|
| 4 | CANCEL on CalDAV-delete | Queue outbound iMIP CANCELs from the DAV delete path | 1 |
| 6 | Share-token CalDAV | Token as Basic username; read-only; revocation/expiry enforced | 1 |
| 3 | Notify dispatch claim | Single-instance deployment + crash-recovery claim | 2 |
| 1 | calendar-query filters | Full RFC 4791 §9.5 core | 2 |
| 2 | sync-collection | Full RFC 6578 | 2 |
| 9 | ADR-012 VTIMEZONE | Full honor | 2 |
| 5 | Inbound sender verification | Trust the receiving pipeline; document the boundary | 3 |
| 7 | Webhooks | Full subsystem | 3 |
| 8 | audit_log | Write on all actor-attributable mutations | 3 |

Sequencing: quick wins → interop epics → features. Each item lands green
(fmt/clippy/tests/interop) before the next starts.

---

## Phase 1 — quick wins

### Item 4: CANCEL on the CalDAV delete path

**Goal**: an organizer deleting a meeting from any CalDAV client notifies
attendees, identically to the JSON API delete.

**Functional requirements**
- Deleting a live event via `DELETE` on a DAV resource queues one
  `METHOD:CANCEL` outbound iMIP row per current attendee with an email
  address, using the same `schedule_cancels` mechanism the JSON API path
  uses.
- Soft-deleted resources re-created (resurrect) do not re-queue CANCELs.
- DAV deletes by a non-organizer principal (read_write but not organizer)
  behave identically — the CANCEL is attributed to the event's organizer
  identity, not the deleter.

**Acceptance criteria**
- CalDAV DELETE of an event with attendees produces outbound CANCEL rows
  (integration test).
- Deleting an event with zero attendees produces none.
- The existing interop CANCEL expectations for the JSON path still pass.

### Item 6: share-token CalDAV

**Goal**: a share link with `allows_caldav = true` grants read-only DAV
access to that calendar, making the existing flag and UI truthful.

**Functional requirements**
- DAV Basic auth accepts the share token as the username; the password is
  ignored. The authenticated principal is the share, not a user.
- Scope: read-only on exactly the shared calendar. No PROPPATCH, no PUT, no
  DELETE, no MKCALENDAR, no ACL access, no sync-collection write side
  (sync reads are fine).
- Revocation, expiry, and soft-deleted-calendar rules apply on every request
  via the same live-share lookup the JSON feed uses.
- Free-busy and event reads honor the share's privacy filter: PRIVATE and
  CONFIDENTIAL events contribute only time information where the share feed
  would withhold them.
- Discovery: `/.well-known/caldav` and the principal URL must not leak
  anything about shares; a share principal sees only the one calendar.
- Removing `allows_caldav` or revoking the share cuts DAV access
  immediately (next request).

**Acceptance criteria**
- A share with `allows_caldav` authenticates via Basic(token, ignored
  password) and can GET/PROPFIND/REPORT the calendar.
- A revoked or expired share gets 401/404, not access.
- A write verb from a share principal is refused.
- A share without `allows_caldav` does not authenticate to DAV.

---

## Phase 2 — interop epics

### Item 1: calendar-query REPORT, RFC 4791 §9.5 core

**Goal**: real clients (iOS, Thunderbird, DAVx5) get correct, filtered,
expanded results instead of every resource in the collection.

**Functional requirements**
- calendar-query REPORT is intercepted in `calendar-server/src/dav.rs`
  (the same pattern as free-busy-query) rather than delegated to
  dav-server's stub.
- Supported filters:
  - `time-range` on VEVENT (and VTODO/VJOURNAL when a collection's
    component set ever widens): an event matches if any occurrence or
    exception overlaps `[start, end)`.
  - `comp-filter` (VEVENT/VALARM presence rules).
  - `prop-filter` with `text-match` (collation may be treated as
    case-insensitive default; `negate-condition` honored).
  - `is-defined` / `is-not-defined` on properties.
- Recurring masters are expanded within the requested window: the response
  contains one resource per matching master (not per occurrence), and a
  master matches if any occurrence in the window matches. Exceptions in the
  window count toward the master's match.
- Requested `<D:prop>` is honored: `getetag`, `calendar-data`, `displayname`.
- Non-goals (explicit): `limit-recurrence-set`, `limit-freebusy-set`,
  timezone-expanded QUERY bodies (`CALDAV:timezone`), collation negotiation
  beyond i;ascii-casemap.
- Results respect ACL exactly as GET does; a calendar the caller cannot read
  404s.

**Acceptance criteria**
- A one-week time-range query returns only masters overlapping that week;
  a recurring daily master matching the window is returned once.
- An exception moved outside the window does not make the master match; an
  exception inside it does.
- `text-match` on SUMMARY with `negate-condition` behaves per RFC.
- Apple Calendar / Thunderbird smoke: a client-issued calendar-query returns
  the expected events (interop test with a captured client-style body).
- The substring-routing hazard in REPORT dispatch is gone (root-element
  parsing, not `body.contains`).

### Item 2: sync-collection, full RFC 6578

**Goal**: conforming clients discover and use incremental sync everywhere
they expect it.

**Functional requirements**
- `supported-report-set` advertises `sync-collection` on calendar
  collections (and the home set) in PROPFIND responses.
- `sync-collection` REPORT on the principal home set aggregates changes
  across all calendars the principal can see, with one token.
- The requested `<D:prop>` is honored: `getetag` and/or `calendar-data`.
- `<D:limit>` with `<D:nresults>` is honored: at most N changes are
  returned, the response includes a `next-token` usable for partial sync,
  and RFC 6578 §3.5 truncation semantics are followed (never drop
  deletions: if truncation would hide a deletion, the deleted entries are
  included even beyond the limit).
- An unknown/expired token returns the RFC 6578 error response with a fresh
  initial-sync token (change_log purge makes old tokens expirable; clients
  resync).
- The token/row two-query race in the current implementation is closed
  (single consistent read).

**Acceptance criteria**
- A client that probes `supported-report-set` sees sync-collection and
  issues it against the home set successfully.
- A limited sync returns ≤ N entries with a usable next-token, and the next
  page continues without gaps or repeats.
- Deleting a calendar entry surfaces as a 404 entry in the next sync.
- An expired token yields the defined error + new initial token.

### Item 9: ADR-012 — honor client-supplied VTIMEZONEs

**Goal**: events using non-IANA tzids recur at the client's intended wall
times instead of silently expanding as UTC.

**Functional requirements**
- On PUT (CalDAV and iMIP inbound), embedded `VTIMEZONE` definitions are
  parsed and stored in the existing `timezones` table keyed by tzid, scoped
  per calendar (later dedupe optional).
- Recurrence expansion, alarm anchoring, and free-busy resolve unknown
  tzids from the stored table; tzdb zones keep precedence (IANA identity
  preserved per the data rules).
- The stored definition is serialized back on export/GET so clients
  round-trip their own zone.
- Compile model: convert STANDARD/DAYOVER blocks with fixed offsets and
  RRULE/RDATE transition definitions into UTC offset transition pairs; an
  oversized expansion window (e.g. 4 years) bounds the transition table.
- A VTIMEZONE too complex to compile (e.g. unsupported sub-components) is
  rejected at PUT with a CalDAV precondition error naming the tzid — no
  silent UTC fallback remains anywhere.
- Existing events that stored a now-definable tzid resolve correctly after
  the calendar's zone is re-PUT.

**Acceptance criteria**
- A VEVENT with a custom tzid + VTIMEZONE and a DST-spanning RRULE expands
  to the correct wall times across the transition (unit + interop test).
- GET returns the VTIMEZONE the client sent.
- A PUT of an uncompilable VTIMEZONE fails with a clear error and stores
  nothing that later expands as UTC.
- No code path resolves an unknown tzid silently to UTC anymore (the
  fallback is removed, not just guarded).

---

## Phase 3 — features

### Item 7: webhooks subsystem (full)

**Goal**: close the PRD-mandated subsystem; make the rules engine's webhook
action reachable; stop carrying dead schema.

**Functional requirements**
- Admin CRUD under `/api/webhooks`: target URL, name, enabled, per-tenant
  scoping, optional shared-secret sign key (stored encrypted like provider
  config).
- Triggers: event created / updated / deleted (the rules trigger set), plus
  rule actions of type `webhook` (which become reachable).
- Delivery model: fire-and-record through `durable_jobs` with the same
  backoff and 5-attempt terminal semantics as `notify_send`; deliveries
  recorded in `webhook_deliveries` (request, response status, duration,
  attempt).
- Payload: JSON envelope with event id, calendar id, trigger, compact event
  view, and an HMAC-SHA256 signature header (`X-CalStack-Signature`) when a
  sign key is configured.
- Delivery is at-least-once; receivers must dedupe on delivery id.
- Never log or expose the sign key or target URL credentials.
- Test-send endpoint mirrors the notification-provider test pattern.

**Acceptance criteria**
- Creating/updating/deleting an event delivers a signed webhook to a local
  receiver in the interop harness; the signature verifies.
- A failing target retries with backoff and terminally fails after 5
  attempts with the delivery recorded.
- Disabling a webhook stops deliveries; deleting it removes it from the
  trigger set.
- A rule with a webhook action delivers, and `rule_executions` reflects it.

### Item 8: audit_log write side

**Goal**: the admin page and `/api/audit` show real history; PRD §22 is
implemented.

**Functional requirements**
- One audit row per actor-attributable mutation: authentication events
  (login success/failure, passkey login), MFA changes (TOTP enable/disable,
  passkey add/remove), credential changes (password, API tokens, app
  passwords), calendar CRUD, ACL edits, event create/update/delete (compact
  metadata only — never event contents, attendee PII, or attachment data),
  share create/revoke, rule/provider/webhook changes, admin user
  management.
- Row shape: actor (user or token id + auth method), action, object type +
  id, tenant, compact JSON metadata, timestamp, source IP when the request
  carried it.
- Failures to write the audit row never fail the mutation (log + move on).
- Retention: expired rows are purged by the existing retention job
  (RETENTION_DAYS).
- The existing `/api/audit` endpoint and admin page render the data
  unchanged.

**Acceptance criteria**
- Logging in, changing an ACL, and revoking a token each produce visible
  audit rows in the interop suite.
- A mutation still succeeds when the audit insert fails (forced failure
  test).
- Audit rows are purged past retention.

---

## Conditional / documentation-only items

### Item 3: notify dispatch crash claim

**Decision**: multi-instance-per-DB is NOT a deployment target; the
requirement is crash recovery only.

**Functional requirements**
- Before dispatching a pending notification row, the sender claims it
  (`claimed_until` lease, conditional UPDATE). A row whose claim expired is
  reclaimable; a row still claimed is skipped this pass.
- On crash between provider send success and `sent_at`, the next pass
  observes an unexpired... (exact semantics: the send marks `sent_at` in the
  same statement flow; the claim prevents a second concurrent worker; a
  crashed worker's claimed rows are retried after lease expiry — at-least-once
  semantics, receivers already tolerate duplicates via dedupe keys on the
  creation side).
- No SKIP LOCKED fan-out, no second-worker support; single sequential
  worker documented in ARCHITECTURE.

**Acceptance criteria**
- A row claimed by a simulated dead worker (lease set, worker gone) is
  skipped until expiry, then retried exactly once more.

### Item 5: inbound sender verification — trust the pipeline

**Decision**: no in-process SPF/DKIM/DMARC evaluation. The receiving mail
pipeline (Postmark inbound) performs authentication; the calendar server
treats Postmark's From as the sender identity.

**Requirements**
- Document the trust boundary in the webhook handler doc comment, README's
  Postmark section, and `docs/COMPATIBILITY.md`: RSVP trust == mail
  pipeline trust; deployments that do not trust their inbound pipeline must
  not configure it.
- The From-must-match-an-attendee check remains (it is authorization, not
  authentication).
- Revisit only if a deployment needs pipeline-less inbound (direct SMTP).

---

## Non-functional requirements (all items)

- Every item lands with: fmt, clippy `-D warnings`, workspace tests,
  interop suite additions, docker compose smoke for server/UI changes,
  codegraph reindex.
- No new external runtime dependencies beyond what the docs mandate
  (Postgres stays the only external dependency; webhook delivery and
  VTIMEZONE parsing use std/HTTP client already in tree).
- Protocol behavior targets RFC 4791/6578/5545/5546; deviations are
  documented in `docs/COMPATIBILITY.md`, not silent.
- Secrets (webhook sign keys, share tokens) follow the existing
  hashed/encrypted storage rules; never logged.

## Open questions

1. **calendar-query**: should VALARM sub-component filtering surface alarm
   data in `calendar-data` responses, or is alarm data suppressed outside
   the owner's own reads? (Default assumption: suppress for non-owner
   readers, matching event-visibility rules.)
2. **sync-collection home-set token**: one global change_log sequence
   already exists — confirm it can back a home-set token without per-
   calendar sharding (implementation design question, flagged for
   `/sc:design`).
3. **Webhooks**: does the payload need a per-tenant envelope version field
   now, or ship v1 and version later? (Default: v1, `version` field
   included.)
4. **audit_log**: retention period for audit rows — reuse RETENTION_DAYS or
   a separate AUDIT_RETENTION_DAYS env? (Default: separate env, default 90.)
5. **VTIMEZONE**: per-calendar scoping of stored zones vs per-tenant reuse
   when identical tzids recur. (Default: per-calendar first, dedupe later.)