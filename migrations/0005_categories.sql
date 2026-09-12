-- Category registry: display metadata (name, fixed-palette color) attached to
-- event category strings by slug match. Events keep their freeform text[] —
-- the registry never constrains what clients send (hybrid model, ADR: see
-- docs/DESIGN-category-registry.md).
CREATE TABLE categories (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  calendar_id uuid REFERENCES calendars(id) ON DELETE CASCADE, -- NULL = tenant-wide
  slug text NOT NULL,
  name text NOT NULL,
  color text NOT NULL,
  sort_order int NOT NULL DEFAULT 0,
  created_by uuid REFERENCES users(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Same nullable-calendar scope pattern as rules: NULL scopes to the tenant.
CREATE UNIQUE INDEX categories_scope_slug_idx ON categories
  (tenant_id, COALESCE(calendar_id, '00000000-0000-0000-0000-000000000000'::uuid), slug);