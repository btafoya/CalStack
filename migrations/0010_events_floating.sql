-- Floating date-times (no Z, no TZID): the wall clock is stored as if UTC and
-- exported without a zone, so a client's floating DTSTART round-trips unchanged.
ALTER TABLE events ADD COLUMN floating boolean NOT NULL DEFAULT false;
