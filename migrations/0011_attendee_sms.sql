-- SMS-only attendees: email is optional; identity falls back to telephone.
-- Attendees are keyed by email for iMIP, but the app also notifies via SMS
-- (twilio providers), so a contact reached only by phone is a valid attendee.
ALTER TABLE event_attendees ALTER COLUMN email DROP NOT NULL;
ALTER TABLE event_attendees DROP CONSTRAINT event_attendees_event_id_email_key;
-- citext has no implicit cast to text in COALESCE with text.
CREATE UNIQUE INDEX event_attendees_event_identity_key
    ON event_attendees (event_id, COALESCE(email::text, telephone));