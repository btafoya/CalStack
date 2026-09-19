# Documentation Index

Four of these files are quoted by path in applied, immutable migrations
(`docs/PRD.md`, `docs/DECISIONS.md`, `docs/DESIGN-category-registry.md`,
`docs/CARDDAV_DESIGN.md` — see `migrations/0001`, `0005`, `0006`) and in
source-file doc comments, so none of the files below move or get renamed;
this index organizes by linking, not by relocating.

## Requirements & decisions

- [`PRD.md`](PRD.md) — the product/technical spec: data model, protocols, API surface, done criteria.
- [`DECISIONS.md`](DECISIONS.md) — binding ADRs (numbered, additive-only — settled decisions are not re-litigated).
- [`IMPLEMENTATION_CHECKLIST.md`](IMPLEMENTATION_CHECKLIST.md) — the build-order checklist for a from-scratch implementation pass.

## Architecture & compatibility

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — crate layering, dav-server-rs integration, transaction/change-journal/concurrency rules.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — target CalDAV/CardDAV clients and the test strategy against them.

## Feature designs

- [`CARDDAV_REQUIREMENTS.md`](CARDDAV_REQUIREMENTS.md) — CardDAV requirements discovery (settled, implemented).
- [`CARDDAV_DESIGN.md`](CARDDAV_DESIGN.md) — CardDAV implementation design following those requirements.
- [`TASKS_JOURNALS_REQUIREMENTS.md`](TASKS_JOURNALS_REQUIREMENTS.md) — VTODO/VJOURNAL requirements discovery (settled).
- [`TASKS_JOURNALS_DESIGN.md`](TASKS_JOURNALS_DESIGN.md) — VTODO/VJOURNAL design, spike findings and staged plan.
- [`INTEROP_CAPTURE.md`](INTEROP_CAPTURE.md) — client fixture session: how to capture real client traffic and the per-client checklist.
- [`DESIGN-category-registry.md`](DESIGN-category-registry.md) — the event category registry design (hybrid slug-match metadata, calendar/tenant scoping).

## Changelog

- [`FIXES-CHANGES.md`](FIXES-CHANGES.md) — running list of UI fixes and changes, checked off as completed.
