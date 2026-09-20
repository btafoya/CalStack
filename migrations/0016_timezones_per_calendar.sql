-- ADR-012: client-supplied VTIMEZONEs scoped per calendar. The tenant-scoped
-- table from 0001 was never read or written by any code path; recreate it
-- per-calendar with the compiled STANDARD/DAYLIGHT rules expansion needs.
DROP TABLE timezones;
CREATE TABLE timezones (
    id uuid PRIMARY KEY,
    calendar_id uuid NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    tzid text NOT NULL,
    definition text NOT NULL,              -- raw VTIMEZONE component text, re-emitted on export
    rules jsonb NOT NULL,                  -- compiled STANDARD/DAYLIGHT rules for UTC offset transitions
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (calendar_id, tzid)
);