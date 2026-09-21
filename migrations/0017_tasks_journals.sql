-- Tasks (VTODO) and journals (VJOURNAL) as typed relational components
-- (ADR-015, supersedes ADR-011; docs/TASKS_JOURNALS_DESIGN.md section 3).
-- Adapted from the design's migration 0009 to the schema as it exists after
-- stages 0/2/2b/2c: hrefs are nullable with a unique index on
-- COALESCE(href, id::text || '.ics') (12a log), components landed in 0009/0013.

CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid text NOT NULL,
    href text,                                          -- series master only; NULL = "{id}.ics"
    master_task_id uuid REFERENCES tasks(id) ON DELETE CASCADE,
    recurrence_id timestamp, recurrence_id_date date,   -- override rows only

    starts_at timestamptz, start_date date,             -- DTSTART (optional)
    due_at timestamptz,    due_date date,               -- DUE (optional)
    duration interval,                                  -- alternative to DUE
    tzid text,
    floating boolean NOT NULL DEFAULT false,            -- wall clock stored as if UTC
    completed_at timestamptz,                           -- COMPLETED, always UTC on the wire

    rrule text, rdate jsonb NOT NULL DEFAULT '[]', exdate jsonb NOT NULL DEFAULT '[]',

    summary text NOT NULL DEFAULT '',
    description_html text, description_text text, url text, location text,
    status text CHECK (status IN ('NEEDS-ACTION', 'IN-PROCESS', 'COMPLETED', 'CANCELLED')),
    percent_complete smallint CHECK (percent_complete BETWEEN 0 AND 100),
    priority smallint CHECK (priority BETWEEN 0 AND 9),
    class text CHECK (class IN ('PUBLIC', 'PRIVATE', 'CONFIDENTIAL')),
    categories text[] NOT NULL DEFAULT '{}',
    parent_uid text,                                    -- RELATED-TO;RELTYPE=PARENT (absent RELTYPE = PARENT)
    sort_order bigint,                                  -- X-APPLE-SORT-ORDER / web UI manual order
    extra_props jsonb NOT NULL DEFAULT '[]',

    organizer_user_id uuid REFERENCES users(id), organizer_email citext, organizer_name text,
    origin_id uuid REFERENCES tasks(id) ON DELETE SET NULL,  -- delivered scheduling copy's origin
    sequence integer NOT NULL DEFAULT 0,
    etag text NOT NULL DEFAULT '',
    created_by uuid REFERENCES users(id),
    deleted_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CHECK (NOT (starts_at IS NOT NULL AND start_date IS NOT NULL)),
    CHECK (NOT (due_at IS NOT NULL AND due_date IS NOT NULL)),
    CHECK (NOT (duration IS NOT NULL AND (due_at IS NOT NULL OR due_date IS NOT NULL))),
    CHECK (master_task_id IS NULL OR rrule IS NULL),    -- overrides do not recur
    CHECK (master_task_id IS NULL OR href IS NULL),     -- only masters are href-addressed
    CHECK ((master_task_id IS NULL) = (recurrence_id IS NULL AND recurrence_id_date IS NULL)),
    CHECK (NOT (recurrence_id IS NOT NULL AND recurrence_id_date IS NOT NULL))
);

CREATE UNIQUE INDEX tasks_uid_occurrence_idx
    ON tasks (calendar_id, uid,
        COALESCE(recurrence_id, recurrence_id_date::timestamp, '-infinity'::timestamp));
-- Same nullable-href pattern as events (0009/12a): NULL means "{id}.ics".
CREATE UNIQUE INDEX tasks_href_idx
    ON tasks (calendar_id, COALESCE(href, id::text || '.ics'));
CREATE INDEX tasks_due_idx
    ON tasks (calendar_id, due_at) WHERE deleted_at IS NULL AND master_task_id IS NULL;
CREATE INDEX tasks_parent_idx
    ON tasks (calendar_id, parent_uid) WHERE parent_uid IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX tasks_master_idx ON tasks(master_task_id);
CREATE INDEX tasks_categories_idx ON tasks USING gin(categories);
ALTER TABLE tasks ADD COLUMN search_vector tsvector GENERATED ALWAYS AS
    (to_tsvector('simple', coalesce(summary, '') || ' ' || coalesce(description_text, ''))) STORED;
CREATE INDEX tasks_search_idx ON tasks USING gin(search_vector);

-- Same shape as event_attendees (post-0011: nullable email, identity by
-- email-or-telephone), FK task_id.
CREATE TABLE task_attendees (
    id uuid PRIMARY KEY,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    user_id uuid REFERENCES users(id),      -- internal attendee link; NULL = external
    contact_id uuid REFERENCES contacts(id) ON DELETE SET NULL,
    email citext,
    display_name text,
    telephone text,
    role text NOT NULL DEFAULT 'REQ-PARTICIPANT'
        CHECK (role IN ('CHAIR', 'REQ-PARTICIPANT', 'OPT-PARTICIPANT', 'NON-PARTICIPANT')),
    partstat text NOT NULL DEFAULT 'NEEDS-ACTION'
        CHECK (partstat IN ('NEEDS-ACTION', 'ACCEPTED', 'DECLINED', 'TENTATIVE', 'DELEGATED')),
    rsvp boolean,
    schedule_status text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
-- citext has no implicit cast to text in COALESCE with text; an expression
-- key must be a unique index, not a table constraint (0011 precedent).
CREATE UNIQUE INDEX task_attendees_task_identity_key
    ON task_attendees (task_id, COALESCE(email::text, telephone));
CREATE INDEX task_attendees_user_idx ON task_attendees(user_id);
CREATE INDEX task_attendees_email_idx ON task_attendees(email);

-- Same shape as event_alarms (post-0012: notify_channels); related END means DUE.
CREATE TABLE task_alarms (
    id uuid PRIMARY KEY,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    action text NOT NULL CHECK (action IN ('DISPLAY', 'EMAIL')),
    related text CHECK (related IN ('START', 'END')),
    offset_interval interval,
    trigger_at timestamptz,
    description text,
    summary text,
    recipient_emails text[] NOT NULL DEFAULT '{}',
    notify_channels text[] NOT NULL DEFAULT '{in_app,email,sms,push}',
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Exactly one trigger form (0001's version rejected relative triggers;
    -- 0002's fix is the intended constraint).
    CHECK ((offset_interval IS NULL) <> (trigger_at IS NULL))
);
CREATE INDEX task_alarms_task_idx ON task_alarms(task_id);

-- Journals are the small case (D9): modelled summary, first DESCRIPTION,
-- DTSTART, STATUS, CLASS, CATEGORIES, URL; everything else in extra_props.
CREATE TABLE journals (
    id uuid PRIMARY KEY,
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid text NOT NULL,
    href text,                              -- NULL = "{id}.ics"
    starts_at timestamptz, start_date date, -- DTSTART optional: undated = a note
    tzid text,
    floating boolean NOT NULL DEFAULT false,
    summary text NOT NULL DEFAULT '',
    description_html text, description_text text, url text,  -- first DESCRIPTION only
    status text CHECK (status IN ('DRAFT', 'FINAL', 'CANCELLED')),
    class text CHECK (class IN ('PUBLIC', 'PRIVATE', 'CONFIDENTIAL')),
    categories text[] NOT NULL DEFAULT '{}',
    extra_props jsonb NOT NULL DEFAULT '[]',
    sequence integer NOT NULL DEFAULT 0,
    etag text NOT NULL DEFAULT '',
    created_by uuid REFERENCES users(id),
    deleted_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (NOT (starts_at IS NOT NULL AND start_date IS NOT NULL)),
    UNIQUE (calendar_id, uid)
);
CREATE UNIQUE INDEX journals_href_idx
    ON journals (calendar_id, COALESCE(href, id::text || '.ics'));
CREATE INDEX journals_categories_idx ON journals USING gin(categories);
CREATE INDEX journals_calendar_live_idx
    ON journals (calendar_id, starts_at) WHERE deleted_at IS NULL;
ALTER TABLE journals ADD COLUMN search_vector tsvector GENERATED ALWAYS AS
    (to_tsvector('simple', coalesce(summary, '') || ' ' || coalesce(description_text, ''))) STORED;
CREATE INDEX journals_search_idx ON journals USING gin(search_vector);

-- One listing for href resolution, read_dir, calendar-query and sync-collection.
CREATE VIEW calendar_objects AS
    SELECT id, calendar_id, COALESCE(href, id::text || '.ics') AS href,
           'VEVENT' AS kind, uid, etag, created_at, updated_at, deleted_at
    FROM events
    UNION ALL
    SELECT id, calendar_id, COALESCE(href, id::text || '.ics'),
           'VTODO', uid, etag, created_at, updated_at, deleted_at
    FROM tasks WHERE master_task_id IS NULL
    UNION ALL
    SELECT id, calendar_id, COALESCE(href, id::text || '.ics'),
           'VJOURNAL', uid, etag, created_at, updated_at, deleted_at
    FROM journals;

-- The iTIP log serves events and tasks; outbound dedupe includes the sequence
-- (repairs the NULL-message_id dedupe gap for events, 12 table).
ALTER TABLE schedule_messages ALTER COLUMN event_id DROP NOT NULL,
    ADD COLUMN task_id uuid REFERENCES tasks(id) ON DELETE CASCADE,
    ADD COLUMN sequence integer,
    ADD CONSTRAINT schedule_messages_subject_chk CHECK (num_nonnulls(event_id, task_id) = 1);
CREATE UNIQUE INDEX schedule_messages_subject_idx ON schedule_messages (
    COALESCE(event_id, task_id), attendee_email, method, direction,
    COALESCE(sequence, -1), COALESCE(message_id, ''));

-- Delivered copies (internal scheduling, stage 8).
ALTER TABLE events ADD COLUMN origin_id uuid REFERENCES events(id) ON DELETE SET NULL;
CREATE INDEX events_origin_idx ON events(origin_id) WHERE origin_id IS NOT NULL;
CREATE INDEX tasks_origin_idx ON tasks(origin_id) WHERE origin_id IS NOT NULL;