# CardDAV Design

Implements `docs/CARDDAV_REQUIREMENTS.md` (all decisions settled). ADR-012 to be added to `docs/DECISIONS.md` at implementation start.

## 1. Component layout

One new crate, one mount, one feature flag.

```
calendar-server/src/dav.rs          + entry_carddav() — second DavHandler over /contacts/
calendar-carddav/                   NEW crate: PgAddressBookFs (dav-server DavFileSystem),
                                    vCard parse/serialize, location parsing
calendar-db/src/contacts.rs         NEW module: address_books + contacts CRUD, search, ctag
calendar-api/                       OpenAPI additions (now: utoipa annotations on contacts_api.rs)
calendar-web/                       /contacts page + contacts.js + event-editor autocomplete
```

**Mount**: `build_router` gains `/contacts`, `/contacts/`, `/contacts/{*rest}` → `dav::entry_carddav`, mirroring the existing `dav::entry` (auth via `resolve_auth`, OPTIONS probe, REPORT pass-through). Second `DavHandler` in `AppState::dav_carddav` built with `PgAddressBookFs`, `principal("/contacts/")`.

**Workspace**: `dav-server = { version = "0.11", features = ["caldav", "carddav"] }`. Both features on the shared handler set is fine — the two mounts use separate `DavHandler` instances so REPORT dispatch never crosses collections. `carddav` is the workspace-dep addition DECISIONS.md anticipated.

**Discovery**:
- `/.well-known/carddav` → 301 `/contacts/` (mirrors the caldav route).
- `CARD:addressbook-home-set` on principal PROPFIND: emitted by dav-server's `handle_props` with the feature enabled (verified in dav-server 0.11 source) — no custom code.
- OPTIONS DAV header on the contacts mount: `1, 2, 3, carddav`.

**REPORTs**: dav-server handles `addressbook-query` and `addressbook-multiget` against the fs. `sync-collection` is intercepted in `dav::entry_carddav` the same way `dav::entry` already intercepts it (RFC 6578 handler extended to addressbook paths) — dav-server does not implement sync-collection for either mount.

## 2. Migration 0006 — data model

```sql
CREATE TABLE address_books (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    owner_user_id uuid REFERENCES users(id),   -- NULL only for kind='directory'
    slug text NOT NULL,
    name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('personal','directory')),
    ctag bigint NOT NULL DEFAULT 0,
    deleted_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, owner_user_id, slug)
);
-- one directory book per tenant
CREATE UNIQUE INDEX address_books_directory_idx ON address_books(tenant_id) WHERE kind='directory' AND deleted_at IS NULL;

CREATE TABLE contacts (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    address_book_id uuid NOT NULL REFERENCES address_books(id),
    uid text NOT NULL,                          -- vCard UID, stable
    kind text NOT NULL DEFAULT 'individual' CHECK (kind IN ('individual','group')),
    fn text, given_name text, family_name text,
    org text, title text,
    email_primary citext,
    tel_primary text,
    photo bytea, photo_mime text,               -- capped, same constraint pattern as attachments
    raw_vcard text NOT NULL,                    -- verbatim card from the last client write
    etag text NOT NULL,
    deleted_at timestamptz,
    created_at/updated_at,
    UNIQUE (address_book_id, uid)
);
-- autocomplete/search
CREATE INDEX contacts_ab_live_idx ON contacts(address_book_id) WHERE deleted_at IS NULL;
CREATE INDEX contacts_fn_idx ON contacts USING gin(to_tsvector('simple', coalesce(fn,'') || ' ' || coalesce(org,'')));  -- match events_search_idx pattern
CREATE INDEX contacts_email_idx ON contacts(email_primary);
CREATE INDEX contacts_tel_idx ON contacts(tel_primary);

CREATE TABLE contact_emails (id, contact_id FK, email citext, kind text, is_primary bool);
CREATE TABLE contact_tels   (id, contact_id FK, number text, kind text, is_mobile bool, is_primary bool);
CREATE INDEX contact_tels_mobile_idx ON contact_tels(contact_id) WHERE is_mobile;   -- Twilio routing
CREATE TABLE contact_addresses (id, contact_id FK, street, locality, region, postal_code, country, kind);

CREATE TABLE contact_group_members (
    group_contact_id uuid NOT NULL REFERENCES contacts(id),
    member_contact_id uuid REFERENCES contacts(id),   -- resolved in-book member
    member_user_id uuid REFERENCES users(id),         -- resolved tenant user
    raw_member text NOT NULL,                         -- client's original member URI, always kept
    PRIMARY KEY (group_contact_id, raw_member)
);

ALTER TABLE event_attendees ADD COLUMN contact_id uuid REFERENCES contacts(id) ON DELETE SET NULL;
```

Rules carried over from the events design: Postgres is canonical, raw payload (`raw_vcard`) is fidelity not truth, soft-delete + existing retention/purge machinery, ctag bump on every mutation (same `UPDATE ... SET ctag = ctag + 1` pattern).

**Directory book is virtual.** No contacts rows are materialized for it. Reads project `tenant_members ⨝ users` into vCards on the fly; per-card etag = `user.updated_at` hash; the collection ctag is computed statelessly from `(count(*), max(updated_at))` of the tenant's members. Writes are refused (`403`). This answers "directory book syncs like a normal collection" with zero state: `sync-collection` REPORT lists the same projections with the same etags, and the sync-token encodes `(count, max(updated_at))`. Directory cards are generated vCard 3.0 (FN, N, EMAIL, TEL type=cell from user profile, ORG, PHOTO if avatar exists).

**Groups**: `KIND:group` cards stored like any contact (`kind='group'`), membership normalized. Apple/DAVx5 rewrite whole addressbooks on sync, so member URIs are preserved via `raw_member` even when unresolvable.

## 3. vCard handling

- **Parse**: `calcard` (vCard parser) — already a transitive dependency of dav-server 0.11, so no new dependency enters the tree. Parse path lives in `calendar-carddav::parse_vcard`; if `calcard` proves inadequate on RFC 6350 fidelity (folded lines, escaping, vCard 4.0 params), fall back to a hand-rolled parser in this crate, mirroring the `parse_ics` precedent. Project rule to add alongside: vCard parsing only through `calendar-carddav::parse_vcard`, never the crate parser raw (mirrors the `icalendar` folded-lines rule).
- **Export**: `GET`/multiget return `raw_vcard` verbatim — byte fidelity and stable etags for free, since clients sent it. Cards created via API/web UI (and directory cards) are generated as vCard 3.0 from normalized columns.
- `supported-address-data`: 3.0 + 4.0 advertised (dav-server emits this with the feature on).
- Normalization on write: parse card → upsert normalized columns (fn, name parts, emails, tels with `is_mobile` for `TEL;TYPE=cell|mobile`, addresses, group members) + store raw. Unknown/custom properties survive in `raw_vcard`.

## 4. PgAddressBookFs (calendar-carddav)

Mirrors `PgDavFs` structure — same `GuardedFileSystem<DavAuth>` trait, same `DavAuth`, `fs_err`/`capability_guard` helpers copied shape:

- `Location::Root | User | AddressBook(slug) | Contact(slug, uid)` — contact resources keyed by **vCard UID** in the URL (`/contacts/{book}/{uid}.vcf`), since that's what clients treat as identity.
- `metadata`/`open`/`read_dir`/`create_dir`/`remove_dir`/`remove_file`/`patch_props` (displayname, addressbook-description) — same shape as the calendar adapter.
- Directory book: `read_dir`/`open` serve projected cards; write paths return `Forbidden`.
- ctag → etag `ctag-{n}` on collections; contact etags from `contacts.etag` / user `updated_at`.

## 5. Application API (OpenAPI additions)

Token scopes unchanged — existing read/write/full middleware covers everything.

```
GET    /api/addressbooks                    personal + directory (kind flag)
POST   /api/addressbooks                    personal only (server creates the default + directory automatically)
GET    /api/addressbooks/{id}               → PATCH/DELETE (personal)
GET    /api/addressbooks/{id}/contacts      list + search (?q=, pagination)
POST   /api/addressbooks/{id}/contacts      create (normalized JSON; server generates vCard 3.0 + UID)
GET    /api/contacts/{id}                   → PATCH/DELETE (soft delete)
GET    /api/contacts/{id}/photo             → PUT (capped)
GET    /api/contacts/autocomplete?q=        union of personal books + tenant directory, ranked (fn prefix > contains > email/tel match)
GET    /api/contacts/{id}/photo             (avatar endpoint for directory users)
```

Directory book appears in `GET /api/addressbooks` as `kind:"directory"`; its contacts are served read-only from the user projection. `event_attendees.contact_id` is settable through the existing event endpoints' attendee objects (nullable, optional).

## 6. Web UI

Follows the existing page pattern (page `const` + handler + route in `calendar-web/src/lib.rs`, JS in `ASSETS`, `api()` helper everywhere):

- **`/contacts` page**: address books sidebar (like the calendars page; create/rename/delete personal books; directory book shown read-only), contact list with search, contact editor modal — name/org, emails, phones with a mobile checkbox (Twilio feed), address. Group cards listed but member-managed only via CardDAV clients (web UI shows membership read-only).
- **Attendee autocomplete in the event editor**: typeahead on `/api/contacts/autocomplete?q=`; picking an attendee stores `contact_id` alongside the existing CN/EMAIL snapshot.
- No new JS framework surface; jQuery 4 + Bootstrap per settled stack.

## 7. Twilio interplay

Nothing implemented here. The data contract this feature guarantees: `contact_tels.is_mobile` + `is_primary`, `contacts.email_primary`. The notification channel chain (push, SMS/Twilio, email — priority order) reads these later; that feature will define per-contact/user routing preferences.

## 8. Security notes

- Same auth path as CalDAV: Basic (app password) + API tokens via `resolve_auth`; session cookies must not authenticate DAV.
- Never log raw_vcard contents, photos, or TEL/email values (existing log rule extended to contact PII).
- PHOTO capped with the attachment-style check constraint; PUT body limit enforced before parse.
- Directory cards expose only what user profiles already expose to the tenant; no new PII surface.
- CardDAV mount honors the same `token_scope_guard` middleware.

## 9. Testing

- Migration applies clean + upgrade test (existing harness).
- Unit: vCard round-trip (fold/unfold, 3.0/4.0 params, groups), normalization (is_mobile detection, primary selection).
- Integration (throwaway PG + curl): addressbook CRUD via REPORT/MKADDRESSBOOK/PUT/GET/DELETE, multiget, sync-collection on personal + directory books, ETag/If-Match, directory read-only 403, well-known redirect.
- Interop: Apple Contacts + DAVx5 manual matrix (primary), Thunderbird opportunistic — per requirements.
- `docker compose up -d --build` smoke test per standing rule; clean up any test rows.
- ACL/scope: read-only token cannot create contacts; directory book 403 on write.

## 10. Implementation stages

```
Stage 1: migration 0006 + calendar-db contacts module + tests
Stage 2: vCard module (parse via calcard, serialize 3.0) + round-trip tests
Stage 3: calendar-carddav crate + dav.rs mount + well-known + sync-collection + interop curl tests
Stage 4: API endpoints + OpenAPI + event_attendees.contact_id
Stage 5: web UI (contacts page, autocomplete, book management)
Stage 6: ADR-012, docs updates, compose smoke test, interop matrix
```

Each stage compiles and passes the full check loop (`fmt`, `clippy -D warnings`, `test --workspace`, integration) before the next starts.

## Risks / notes

- dav-server's `addressbook-query` filter support is limited → treat client-side filtering in the fs as the fallback (same posture as the CalDAV full-window scan ponytail note; SQL push-down when profiled).
- `calcard` fidelity is the one real unknown in this design; the fallback (hand-rolled parser in `calendar-carddav`) is scoped and precedented.
- Directory book statelessness means one expensive `max(updated_at)` per sync probe — cheap on the tenants table; revisit only if profiled.