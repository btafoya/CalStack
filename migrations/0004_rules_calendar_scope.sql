-- Rules can now target one calendar; NULL keeps the old tenant-wide behavior.
ALTER TABLE rules ADD COLUMN calendar_id uuid REFERENCES calendars(id) ON DELETE CASCADE;
CREATE INDEX rules_calendar_idx ON rules(calendar_id, position);
