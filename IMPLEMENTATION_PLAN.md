# Implementation Plan: Spec-Generated OpenAPI (utoipa)

**Goal**: Replace the hand-maintained `openapi_document()` (`crates/calendar-api/src/lib.rs`) with a document generated from the handlers, so `/api/openapi.json` cannot drift from the router.

**Decided requirements** (from brainstorm, 2026-09-21):

- Tooling: `utoipa` — `#[utoipa::path]` on each handler in `calendar-server/src/*.rs`, collected into one `OpenApi` derive, served at the existing `GET /api/openapi.json`.
- Schemas: typed request bodies (existing structs get `ToSchema`) and typed response view structs (the ad-hoc `json!` views become structs with `ToSchema`).
- Migration: module-by-module; legacy `openapi_document()` remains source of truth for un-migrated modules and is deleted when the last module converts.
- Validation: interop suite validates live responses against the generated response schemas.
- Deliverable: spec only — no generated client committed.
- Document stays **OpenAPI 3.1.0** (PRD:285); verify utoipa's 3.1 feature flag output in stage 1, not at the end.

**Binding constraint — response compatibility**: every converted view struct must serialize to the identical field set as the `json!` view it replaces (same names, same nullability, same casing). Enforced by a unit test per module comparing `serde_json::to_value(&typed_view)` against the old `json!` output for a fixture row. Response shape changes are never a side effect of this work; if one is ever needed it is a separate, explicit change.

**Non-goals**: new endpoints, auth changes on `/docs` or `/api/openapi.json` (both stay unauthenticated), Swagger UI behavior changes (it keeps rendering the same URL), CalDAV/CardDAV route documentation.

**Completion test for every stage**: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo build --bin calendar-server` + `tests/interop/run.sh`.

---

## Stage 1: Foundation — utoipa wired, 3.1.0 verified, dual-doc serving

**Goal**: `/api/openapi.json` serves a utoipa-built document that today consists of the legacy doc merged in; annotations pipeline proven end to end.
**Success Criteria**:
- `utoipa` added to workspace with the OpenAPI 3.1 feature flag; served document reports `"openapi": "3.1.0"` (verified by interop test, unchanged).
- `ApiDoc` merges the legacy `openapi_document()` fragment so path coverage is unchanged from day one.
- `/docs` Swagger UI renders the merged document unchanged.
**Tests**: interop step 1 passes unchanged; unit test asserts `ApiDoc` paths ⊇ legacy paths.
**Status**: Complete (utoipa 5.5 emits 3.1.0 by default; legacy fragment gap-fills paths, schemas and securitySchemes; verified against `calstack-test-pg`)

## Stage 2: Auth surface (auth, tokens, app-passwords, TOTP, WebAuthn, notify-prefs)

**Goal**: Largest group converted first, to prove the annotation pattern on session/CSRF/TOTP/passkey routes including their untyped ceremony bodies.
**Success Criteria**: All `/api/auth/*` paths generated from annotations; legacy auth paths removed from the fragment; response views (`me`, login session, token list, TOTP status, passkey list) are typed structs passing the compatibility fixture test.
**Tests**: compatibility fixture test for each converted view; completeness test pinning the auth path inventory.
**Status**: Complete (auth + mfa annotated; legacy fragment's `/api/auth/*` entries deleted; also fixed the legacy doc's broken `json_response`/`ok["200"]` responses, which were invalid JSON Schema. Interop verified against `calstack-test-pg` after recreating `caltest` — the suite assumes a fresh DB, it does not reset one)

## Stage 3: Calendaring core (calendars, ACL, events, occurrences, tasks, journals)

**Goal**: The heart of the API: `calendars_api`, `events_api`, `tasks_api`, `journals_api`.
**Success Criteria**: Calendar/event/occurrence/task/journal routes generated; `calendar_view`, task/journal views, occurrence overlay responses typed; legacy paths removed.
**Tests**: compatibility fixture tests (calendar_view is the critical one — every field incl. nulls); completeness test updated.
**Status**: Complete (all four modules annotated; calendar/event/occurrence/task/journal views are typed structs; the events/tasks/journals existing shape tests now verify struct serialization. Legacy fragment is down to sharing/attachments/categories/rules/addressbook/contacts/providers/push/webhooks/audit/admin. Discovered during conversion: /api/search, /api/changes, /api/notifications, /api/places and /api/attachments/{id}/meta were never in the legacy doc at all — they get documented in stage 4)

## Stage 4: Everything else (contacts/addressbook, categories, rules, subscriptions/shares, push, webhooks, notification providers, audit, search, changes, attachments, places, admin, mfa, extras)

**Goal**: Remaining modules converted; handled in small sub-batches to keep each commit green.
**Success Criteria**: All `/api/*` routes come from annotations; legacy fragment contains no paths.
**Tests**: compatibility fixture tests per converted view; completeness test covers the full ~66-path inventory.
**Status**: Not Started

## Stage 5: Delete legacy doc + response-schema validation in interop

**Goal**: Retire `openapi_document()`, `json_response`, `param` and the calendar-api crate's doc code; interop suite starts validating live responses against generated schemas.
**Success Criteria**:
- `openapi_document()` deleted; nothing references it.
- Interop harness validates a sample of live responses (calendar list, event CRUD round-trip, task, webhook, audit) against the generated response schemas; any mismatch fails the suite.
**Tests**: full interop run with response validation enabled; the completeness test now asserts `ApiDoc` paths == the full pinned inventory (deleting an annotation or handler fails CI).
**Status**: Not Started