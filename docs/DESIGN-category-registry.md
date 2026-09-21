# Design: Event Category Registry (hybrid)

Decisions settled in brainstorm: calendar owners manage their calendars' categories
(admins manage tenant-wide), slug rename cascades to events, fixed color palette,
event responses always carry enriched details, rules-trigger integration is a
follow-up release.

## Principle

Registry rows are **display metadata attached by slug match**, not schema.
`events.categories text[]` stays freeform; CalDAV `CATEGORIES` round-trips as pure
text. Empty registry = current behavior. Unknown strings keep working everywhere.

## Database (migration 0005_categories.sql)

```sql
CREATE TABLE categories (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  calendar_id uuid REFERENCES calendars(id) ON DELETE CASCADE, -- NULL = tenant-wide
  slug text NOT NULL,
  name text NOT NULL,
  color text NOT NULL,               -- fixed-palette key, validated app-side
  sort_order int NOT NULL DEFAULT 0,
  created_by uuid REFERENCES users(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Same nullable-calendar scope pattern as rules.
CREATE UNIQUE INDEX categories_scope_slug_idx ON categories
  (tenant_id, COALESCE(calendar_id, '00000000-0000-0000-0000-000000000000'::uuid), slug);
```

No `events` change. No GIN change (existing `events_categories_idx` suffices).

## Rename cascade (app-side transaction, not a trigger)

Scope follows the row's scope:

- calendar-scoped row: `UPDATE events SET categories = array_replace(categories, $old, $new) WHERE calendar_id = $cal AND $old = ANY(categories)`
- tenant-wide row: same with `calendar_id IN (SELECT id FROM calendars WHERE tenant_id = $tenant)`

Same transaction as the `categories` row update. Accepted edge: renaming a
tenant-wide row also rewrites strings shadowed by a calendar-scoped row (display
resolution unaffected; calendar wins).

## calendar-db: `src/categories.rs`

`CategoryRow { id, tenant_id, calendar_id, slug, name, color, sort_order }`

- `list_categories(pool, tenant_id, calendar_id: Option<Uuid>) -> Vec<CategoryRow>`
- `create_category(pool, tenant_id, NewCategory) -> CategoryRow`
- `update_category(pool, id, CategoryUpdate) -> CategoryRow` — rename cascade inside same tx
- `delete_category(pool, id)`

Slug validation reuses `calendar_core::validate_slug`. Color validated against
`CATEGORY_COLORS` const (below) in `calendar-core`.

## calendar-core

```rust
pub const CATEGORY_COLORS: &[&str] = &[
    "blue", "azure", "indigo", "purple", "pink", "red", "orange",
    "yellow", "lime", "green", "teal", "cyan",
]; // Tabler palette; CSS classes: bg-{color}-lt, badge bg-{color}-lt
```

## calendar-server: `src/categories_api.rs` (router like rules_api.rs)

| Method | Path | Auth |
|---|---|---|
| GET | `/api/categories?calendar_id=` | any authenticated user (own tenant) |
| POST | `/api/categories` | `calendar_id` set → `Owner` capability; NULL → `require_admin` |
| PATCH | `/api/categories/{id}` | `Owner` cap on row's calendar; tenant-wide → admin |
| DELETE | `/api/categories/{id}` | same as PATCH |

Mutations: `resolve_auth` + `require_csrf` + `find_personal_tenant`, same as rules.
Unlike rules API, this is **not** admin-only — capability-gated per the settled
decision.

Note: `find_personal_tenant` gives the caller's personal tenant, matching the
existing single-tenant-per-user model used by rules/providers.

### Event enrichment

`event_view` gains one field:

```json
"category_details": [{ "slug": "work", "name": "Work", "color": "blue" }]
```

Load pattern (one query per request, never per event): fetch registry rows for the
scope once — `WHERE tenant_id = $1 AND (calendar_id = $2 OR calendar_id IS NULL)`
— then match `event.categories` in memory. Applied in `list_events` and
`get_event`. CalDAV/ICS/export paths unchanged.

### OpenAPI

Categories carry utoipa annotations (`categories_api.rs`); the
`category_details` nested view lives in `events_api::EventView`.

## Web UI

- New page `/categories` (`categories.js` + page const/handler/route in
  `calendar-web/src/lib.rs`, registered in `ASSETS`), rules-page template:
  table + create/edit modal, palette `<select>` with `bg-*-lt` swatches,
  scope selector (calendar or tenant-wide, tenant-wide admin-only).
- Event form (`app.js`): `#ev-categories` gains a `<datalist>` fed from
  `GET /api/categories` for the current calendar — free text still accepted.
- Event display: category strings matching registry render as Tabler badges
  (`bg-{color}-lt`), unmatched render as plain badges.

## Non-goals (day one)

- No rules-engine condition (follow-up release).
- No auto-creation of registry rows from CalDAV input.
- No ICS metadata leakage — `CATEGORIES` value unchanged.
- No per-user category overrides.

## Testing

- Unit: color-palette validation, slug validation reuse, rename cascade SQL semantics.
- Integration: CRUD round-trip per scope, shadowing (calendar row wins display),
  rename cascade scope, enrichment in list/get, delete leaves event strings intact.
- Compose smoke test: create category via UI, tag event, verify badge + details.

## Stages

1. Migration + calendar-db `categories.rs` — verify: migration applies, CRUD integration test.
2. calendar-core palette + validation — verify: unit tests.
3. categories_api.rs + OpenAPI — verify: curl round-trip on throwaway PG.
4. Event enrichment in list/get — verify: API integration test.
5. Web UI page + datalist + badges — verify: compose smoke test.