-- Initial schema: complete normalized model per docs/PRD.md and docs/DECISIONS.md (ADR-001..012).
-- Canonical relational event store; iCalendar is a wire representation only.
-- Tenants and membership exist from this migration (ADR-008).
-- Attachments are capped bytea (ADR-010); VTODO is never stored (ADR-011).
-- Custom client VTIMEZONE definitions live in `timezones` (ADR-012).

CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS citext;

-- ============ identity and tenancy ============

CREATE TABLE users (
    id uuid PRIMARY KEY,
    username text NOT NULL UNIQUE CHECK (username ~ '^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$'),
    email citext NOT NULL UNIQUE,
    display_name text,
    password_hash text,                    -- argon2id; NULL until first password is set
    is_admin boolean NOT NULL DEFAULT false,
    timezone text,                         -- user-facing default IANA tzid
    disabled_at timestamptz,
    last_login_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tenants (
    id uuid PRIMARY KEY,
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    name text NOT NULL,
    is_personal boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tenant_members (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);
CREATE INDEX tenant_members_user_idx ON tenant_members(user_id);

-- ============ authentication ============

CREATE TABLE sessions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash bytea NOT NULL UNIQUE,      -- sha256 of the session cookie value
    csrf_token text NOT NULL,
    ip inet,
    user_agent text,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_expiry_idx ON sessions(expires_at) WHERE revoked_at IS NULL;

-- Human-typed secrets for CalDAV Basic auth: argon2id canonical hash plus
-- sha256 lookup_hash for a fast path on per-request verification.
CREATE TABLE app_passwords (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL,
    password_hash text NOT NULL,           -- argon2id
    lookup_hash bytea NOT NULL UNIQUE,     -- sha256
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    expires_at timestamptz,
    revoked_at timestamptz
);

CREATE TABLE api_tokens (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL,
    token_hash bytea NOT NULL UNIQUE,      -- sha256; tokens are high-entropy
    scopes text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    expires_at timestamptz,
    revoked_at timestamptz
);

CREATE TABLE webauthn_credentials (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id bytea NOT NULL UNIQUE,
    public_key bytea NOT NULL,
    sign_count bigint NOT NULL DEFAULT 0,
    transports text[] NOT NULL DEFAULT '{}',
    name text,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz
);

CREATE TABLE totp_secrets (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    secret_encrypted bytea NOT NULL,       -- envelope-encrypted with the environment key
    confirmed_at timestamptz,
    recovery_codes text[] NOT NULL DEFAULT '{}',  -- hashed one at a time, removed on use
    created_at timestamptz NOT NULL DEFAULT now()
);

-- ============ calendars and ACLs ============

CREATE TABLE calendars (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    name text NOT NULL,
    description text,
    color text,
    timezone text,                         -- default IANA tzid for the calendar
    order_index integer NOT NULL DEFAULT 0,
    ctag bigint NOT NULL DEFAULT 0,        -- bumped with every change_log entry
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,                -- soft delete for sync reporting
    UNIQUE (tenant_id, slug)
);

-- Principal-based ACL (ADR-003). v1 principals are users only; groups arrive as a
-- later principal type via a new nullable FK column, never as free-text roles.
CREATE TABLE calendar_acl (
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    principal_user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    capability text NOT NULL CHECK (capability IN ('owner', 'read_write', 'read_only', 'free_busy')),
    can_manage_acl boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (calendar_id, principal_user_id)
);
CREATE INDEX calendar_acl_user_idx ON calendar_acl(principal_user_id);

-- Client-supplied VTIMEZONE definitions that are not in the tzdb (ADR-012).
CREATE TABLE timezones (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    tzid text NOT NULL,
    definition text NOT NULL,              -- raw VTIMEZONE component text
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, tzid)
);

-- ============ events ============

CREATE TABLE locations (
    id uuid PRIMARY KEY,
    provider text,
    provider_place_id text,
    display_name text,
    formatted_address text,
    street_address text,
    locality text,
    administrative_area text,
    postal_code text,
    country text,
    latitude double precision,
    longitude double precision,
    website text,
    phone text,
    provider_metadata jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Masters and RECURRENCE-ID exceptions share this table: an exception is a row with
-- master_event_id set and the original occurrence start in recurrence_id(_date).
-- Recurrence is never flattened (ADR-002); expansion happens only for querying,
-- conflict detection, reminders and feeds.
CREATE TABLE events (
    id uuid PRIMARY KEY,
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid text NOT NULL,
    master_event_id uuid REFERENCES events(id) ON DELETE CASCADE,
    recurrence_id timestamp,               -- overridden occurrence local wall-clock in tzid
    recurrence_id_date date,               -- overridden occurrence start (all-day)
    is_exception boolean GENERATED ALWAYS AS (master_event_id IS NOT NULL) STORED,

    starts_at timestamptz,                 -- normalized instant for querying
    ends_at timestamptz,
    start_date date,                       -- all-day anchor
    end_date date,
    duration interval,                     -- DURATION property when the client sent one
    tzid text,
    all_day boolean NOT NULL DEFAULT false,

    rrule text,                            -- RRULE value string; never set on exceptions
    rdate jsonb NOT NULL DEFAULT '[]'::jsonb,   -- array of ISO instants or dates
    exdate jsonb NOT NULL DEFAULT '[]'::jsonb,

    summary text NOT NULL DEFAULT '',
    description_html text,                 -- sanitized HTML (canonical rich form)
    description_text text,                 -- derived plain text
    url text,
    status text CHECK (status IN ('TENTATIVE', 'CONFIRMED', 'CANCELLED')),
    priority smallint CHECK (priority BETWEEN 0 AND 9),
    class text CHECK (class IN ('PUBLIC', 'PRIVATE', 'CONFIDENTIAL')),
    transp text CHECK (transp IN ('OPAQUE', 'TRANSPARENT')),
    categories text[] NOT NULL DEFAULT '{}',
    location_id uuid REFERENCES locations(id),

    organizer_user_id uuid REFERENCES users(id),
    organizer_email citext NOT NULL,
    organizer_name text,

    sequence integer NOT NULL DEFAULT 0,
    etag text NOT NULL DEFAULT '',
    created_by uuid REFERENCES users(id),
    deleted_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CHECK ((starts_at IS NOT NULL) <> (start_date IS NOT NULL)),
    CHECK (ends_at IS NOT NULL OR end_date IS NOT NULL OR duration IS NOT NULL),
    CHECK (master_event_id IS NULL OR rrule IS NULL),  -- exceptions do not recur
    CHECK (NOT ((recurrence_id IS NOT NULL) AND (recurrence_id_date IS NOT NULL)))
);

CREATE UNIQUE INDEX events_uid_occurrence_idx
    ON events (calendar_id, uid,
        COALESCE(recurrence_id, recurrence_id_date::timestamp, '-infinity'::timestamp));
CREATE INDEX events_calendar_live_idx ON events(calendar_id, starts_at) WHERE deleted_at IS NULL;
CREATE INDEX events_calendar_deleted_idx ON events(calendar_id, deleted_at);
CREATE INDEX events_calendar_uid_idx ON events(calendar_id, uid);
CREATE INDEX events_master_idx ON events(master_event_id);
CREATE INDEX events_categories_idx ON events USING gin(categories);
ALTER TABLE events ADD COLUMN search_vector tsvector GENERATED ALWAYS AS
    (to_tsvector('simple', coalesce(summary, '') || ' ' || coalesce(description_text, ''))) STORED;
CREATE INDEX events_search_idx ON events USING gin(search_vector);

CREATE TABLE event_attendees (
    id uuid PRIMARY KEY,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    user_id uuid REFERENCES users(id),     -- internal attendee link; NULL = external
    email citext NOT NULL,
    display_name text,
    telephone text,
    role text NOT NULL DEFAULT 'REQ-PARTICIPANT'
        CHECK (role IN ('CHAIR', 'REQ-PARTICIPANT', 'OPT-PARTICIPANT', 'NON-PARTICIPANT')),
    partstat text NOT NULL DEFAULT 'NEEDS-ACTION'
        CHECK (partstat IN ('NEEDS-ACTION', 'ACCEPTED', 'DECLINED', 'TENTATIVE', 'DELEGATED')),
    rsvp boolean,
    schedule_status text,                  -- RFC 6638 scheduling status
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (event_id, email)
);
CREATE INDEX event_attendees_user_idx ON event_attendees(user_id);
CREATE INDEX event_attendees_email_idx ON event_attendees(email);

CREATE TABLE event_alarms (
    id uuid PRIMARY KEY,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    action text NOT NULL CHECK (action IN ('DISPLAY', 'EMAIL')),
    related text CHECK (related IN ('START', 'END')),
    offset_interval interval,              -- relative trigger
    trigger_at timestamptz,                -- absolute trigger
    description text,
    summary text,
    recipient_emails text[] NOT NULL DEFAULT '{}',  -- EMAIL action recipients
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((offset_interval IS NULL) <> (trigger_at IS NOT NULL))
);
CREATE INDEX event_alarms_event_idx ON event_alarms(event_id);

-- Capped bytea (ADR-010). The size cap is env-configurable and enforced by the
-- application on write; deliberately no static CHECK so the cap stays configurable.
CREATE TABLE attachments (
    id uuid PRIMARY KEY,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    filename text NOT NULL,
    content_type text NOT NULL,
    byte_size bigint NOT NULL,
    sha256 bytea NOT NULL,
    data bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX attachments_event_idx ON attachments(event_id);

-- ============ sync and change journal ============

CREATE TABLE change_log (
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,   -- global monotonic; sync token
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    resource_id uuid NOT NULL,
    resource_type text NOT NULL DEFAULT 'event',
    operation text NOT NULL CHECK (operation IN ('created', 'updated', 'deleted')),
    changed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX change_log_calendar_seq_idx ON change_log(calendar_id, seq);
CREATE INDEX change_log_purge_idx ON change_log(changed_at);

-- ============ public sharing (ADR-004) ============

CREATE TABLE public_shares (
    id uuid PRIMARY KEY,
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    event_id uuid REFERENCES events(id) ON DELETE CASCADE,  -- single-event share
    token_hash bytea NOT NULL UNIQUE,      -- sha256 of the share token
    allows_caldav boolean NOT NULL DEFAULT false,  -- read-only CalDAV via token
    created_by uuid REFERENCES users(id),
    expires_at timestamptz,
    revoked_at timestamptz,
    last_accessed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX public_shares_calendar_idx ON public_shares(calendar_id);

-- ============ background jobs ============

CREATE TABLE durable_jobs (
    id uuid PRIMARY KEY,
    job_type text NOT NULL,
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    run_at timestamptz NOT NULL DEFAULT now(),
    priority integer NOT NULL DEFAULT 0,
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    locked_until timestamptz,
    locked_by text,
    completed_at timestamptz,
    failed_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX durable_jobs_ready_idx ON durable_jobs(priority DESC, run_at)
    WHERE completed_at IS NULL AND failed_at IS NULL;
CREATE INDEX durable_jobs_stuck_idx ON durable_jobs(locked_until)
    WHERE completed_at IS NULL AND failed_at IS NULL AND locked_until IS NOT NULL;

-- ============ rules (ADR-007) ============

CREATE TABLE rules (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name text NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    trigger_type text NOT NULL,
    conditions jsonb NOT NULL DEFAULT '[]'::jsonb,
    actions jsonb NOT NULL DEFAULT '[]'::jsonb,
    position integer NOT NULL DEFAULT 0,
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX rules_tenant_idx ON rules(tenant_id, position);

CREATE TABLE rule_executions (
    id uuid PRIMARY KEY,
    rule_id uuid NOT NULL REFERENCES rules(id) ON DELETE CASCADE,
    subject_type text,
    subject_id uuid,
    status text NOT NULL CHECK (status IN ('succeeded', 'failed', 'skipped')),
    detail jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX rule_executions_retention_idx ON rule_executions(created_at);

-- ============ notifications ============

CREATE TABLE notification_providers (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('postmark', 'smtp', 'twilio', 'webpush')),
    name text NOT NULL,
    config_encrypted bytea NOT NULL,       -- credentials; envelope-encrypted with env key
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, kind, name)
);

CREATE TABLE notifications (
    id uuid PRIMARY KEY,
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    channel text NOT NULL CHECK (channel IN ('in_app', 'email', 'sms', 'push')),
    title text,
    body text,
    data jsonb,
    dedupe_key text UNIQUE,                -- idempotent notification creation
    read_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX notifications_user_idx ON notifications(user_id, created_at DESC);

CREATE TABLE web_push_subscriptions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint text NOT NULL UNIQUE,
    keys jsonb NOT NULL,                   -- p256dh and auth
    expiration_time timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE webhooks (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    url text NOT NULL,
    secret_encrypted bytea,                -- signing secret must stay retrievable: encrypted, never hashed
    events text[] NOT NULL DEFAULT '{}',
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE webhook_deliveries (
    id uuid PRIMARY KEY,
    webhook_id uuid NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    payload jsonb NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'succeeded', 'failed', 'exhausted')),
    attempts integer NOT NULL DEFAULT 0,
    response_code integer,
    error text,
    delivered_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_deliveries_status_idx ON webhook_deliveries(status, created_at);

-- ============ iTIP / iMIP message log (idempotent scheduling) ============

CREATE TABLE schedule_messages (
    id uuid PRIMARY KEY,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    attendee_email citext NOT NULL,
    method text NOT NULL CHECK (method IN ('REQUEST', 'REPLY', 'CANCEL', 'DECLINECOUNTER')),
    direction text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'sent', 'received', 'processed', 'failed')),
    message_id text,                       -- email Message-ID; inbound dedupe anchor
    error text,
    processed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (event_id, attendee_email, method, direction, message_id)
);
CREATE INDEX schedule_messages_inbound_idx ON schedule_messages(direction, status);

-- ============ inbound share subscriptions ============
-- A user subscribing to another calendar's public share. Read access flows
-- through the referenced public_shares row: revoking or expiring the share
-- ends the subscription with it.

CREATE TABLE subscriptions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    share_id uuid NOT NULL REFERENCES public_shares(id) ON DELETE CASCADE,
    color text,
    order_index integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, share_id)
);
CREATE INDEX subscriptions_share_idx ON subscriptions(share_id);

-- ============ audit (lightweight, no event versions) ============

CREATE TABLE audit_log (
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid REFERENCES tenants(id) ON DELETE CASCADE,
    actor_type text NOT NULL CHECK (actor_type IN ('user', 'token', 'session', 'system')),
    actor_id uuid,
    action text NOT NULL,
    object_type text NOT NULL,
    object_id uuid,
    change_summary jsonb,
    ip inet,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_tenant_idx ON audit_log(tenant_id, seq DESC);