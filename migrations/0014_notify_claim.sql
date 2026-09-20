-- Crash-recovery claim for notification dispatch: a row claimed by a worker
-- that died mid-send is retried only after the lease expires.
ALTER TABLE notifications ADD COLUMN claimed_until timestamptz;