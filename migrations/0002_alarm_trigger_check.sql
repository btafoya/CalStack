-- 0001's event_alarms CHECK inverted the intended "exactly one trigger form"
-- constraint: `(offset_interval IS NULL) <> (trigger_at IS NOT NULL)` rejects
-- every relative trigger (offset set, trigger NULL -> false <> false -> false).
-- Replace with the intended one-of constraint.
ALTER TABLE event_alarms DROP CONSTRAINT event_alarms_check;
ALTER TABLE event_alarms ADD CONSTRAINT event_alarms_check
    CHECK ((offset_interval IS NULL) <> (trigger_at IS NULL));