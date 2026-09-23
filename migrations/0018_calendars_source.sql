-- Subscribed remote calendars (PRD: outbound .ics subscription).
-- A calendar with source_url is fed from a remote ICS file by the ics_sync
-- job and is read-only to content writes. source_etag carries the last
-- conditional-GET validator (ETag or Last-Modified); source_synced_at is the
-- last successful pass.
ALTER TABLE calendars ADD COLUMN source_url text;
ALTER TABLE calendars ADD COLUMN source_etag text;
ALTER TABLE calendars ADD COLUMN source_synced_at timestamptz;