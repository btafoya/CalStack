-- Per-collection component sets and client-chosen resource filenames
-- (docs/TASKS_JOURNALS_DESIGN.md sections 2-3).

ALTER TABLE calendars
    ADD COLUMN components text[] NOT NULL DEFAULT ARRAY['VEVENT', 'VTODO', 'VJOURNAL'],
    ADD CONSTRAINT calendars_components_chk CHECK (
        cardinality(components) BETWEEN 1 AND 3
        AND components <@ ARRAY['VEVENT', 'VTODO', 'VJOURNAL']
    );

-- Filename the client PUT the resource under. NULL means "{id}.ics" (API-created
-- events and CalDAV PUTs to a canonical "{uuid}.ics" URL).
ALTER TABLE events ADD COLUMN href text;
CREATE UNIQUE INDEX events_href_idx
    ON events (calendar_id, COALESCE(href, id::text || '.ics'));
