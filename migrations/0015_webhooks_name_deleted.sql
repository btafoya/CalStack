-- Webhooks (docs/DEFERRED_REQUIREMENTS.md item 7): the initial schema lacks a
-- display name and soft delete. Revoking keeps the row (and its delivery
-- history) instead of cascading the deliveries away.
ALTER TABLE webhooks ADD COLUMN name text NOT NULL DEFAULT '';
ALTER TABLE webhooks ADD COLUMN deleted_at timestamptz;