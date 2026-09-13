-- CardDAV support (docs/CARDDAV_DESIGN.md, ADR-013): personal address books
-- plus a virtual read-only tenant directory (not stored here; projected from
-- tenant_members+users). Contacts are canonical normalized rows with the
-- verbatim last-written vCard kept for wire fidelity, same pattern as events.

CREATE TABLE address_books (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    owner_user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    name text NOT NULL,
    ctag bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    UNIQUE (owner_user_id, slug)
);
CREATE INDEX address_books_owner_idx ON address_books(owner_user_id);

CREATE TABLE contacts (
    id uuid PRIMARY KEY,
    address_book_id uuid NOT NULL REFERENCES address_books(id) ON DELETE CASCADE,
    uid text NOT NULL,
    kind text NOT NULL DEFAULT 'individual' CHECK (kind IN ('individual', 'group')),
    full_name text NOT NULL DEFAULT '',
    given_name text,
    family_name text,
    org text,
    title text,
    street_address text,
    locality text,
    region text,
    postal_code text,
    country text,
    photo bytea,               -- capped on write, same posture as attachments (ADR-010)
    photo_mime text,
    raw_vcard text NOT NULL,   -- verbatim last-written card (wire fidelity)
    etag text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    UNIQUE (address_book_id, uid)
);
CREATE INDEX contacts_ab_live_idx ON contacts(address_book_id) WHERE deleted_at IS NULL;
CREATE INDEX contacts_search_idx ON contacts
    USING gin(to_tsvector('simple', coalesce(full_name, '') || ' ' || coalesce(org, '')));

CREATE TABLE contact_emails (
    id uuid PRIMARY KEY,
    contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    email citext NOT NULL,
    kind text,
    is_primary boolean NOT NULL DEFAULT false
);
CREATE INDEX contact_emails_contact_idx ON contact_emails(contact_id);
CREATE INDEX contact_emails_email_idx ON contact_emails(email);

-- is_mobile drives the planned Twilio SMS channel: only mobile-typed numbers
-- are eligible recipients.
CREATE TABLE contact_tels (
    id uuid PRIMARY KEY,
    contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    number text NOT NULL,
    kind text,
    is_mobile boolean NOT NULL DEFAULT false,
    is_primary boolean NOT NULL DEFAULT false
);
CREATE INDEX contact_tels_contact_idx ON contact_tels(contact_id);
CREATE INDEX contact_tels_mobile_idx ON contact_tels(contact_id) WHERE is_mobile;

-- KIND:group membership. raw_member is always kept (the client's original
-- MEMBER URI) since Apple Contacts/DAVx5 rewrite whole address books on sync
-- and unresolvable members must still round-trip.
CREATE TABLE contact_group_members (
    group_contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    raw_member text NOT NULL,
    member_contact_id uuid REFERENCES contacts(id) ON DELETE SET NULL,
    PRIMARY KEY (group_contact_id, raw_member)
);

-- Loose ref (PRD: attendees stay independent of ACL/contacts); name/email are
-- snapshotted on event_attendees already, contact deletion does not cascade.
ALTER TABLE event_attendees ADD COLUMN contact_id uuid REFERENCES contacts(id) ON DELETE SET NULL;
CREATE INDEX event_attendees_contact_idx ON event_attendees(contact_id);
