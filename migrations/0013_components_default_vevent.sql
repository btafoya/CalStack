-- H7: the advertised component set must match what storage accepts.
-- ADR-011 rejects VTODO and VJOURNAL is never stored, so calendars may only
-- advertise VEVENT (0009 defaulted to all three and PROPFIND echoed it).

UPDATE calendars SET components = ARRAY['VEVENT'];

ALTER TABLE calendars
    ALTER COLUMN components SET DEFAULT ARRAY['VEVENT'];