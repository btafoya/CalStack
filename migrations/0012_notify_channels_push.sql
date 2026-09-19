-- Multi-channel reminder dispatch: per-alarm channel selection, per-user
-- opt-out, send bookkeeping, and Web Push subscriptions.

-- Wire alarms only carry DISPLAY/EMAIL (RFC 5545); sms/push stay app-side.
ALTER TABLE event_alarms ADD COLUMN notify_channels text[] NOT NULL DEFAULT '{in_app,email,sms,push}';

ALTER TABLE users ADD COLUMN notify_email boolean NOT NULL DEFAULT true;
ALTER TABLE users ADD COLUMN notify_sms boolean NOT NULL DEFAULT true;
ALTER TABLE users ADD COLUMN notify_push boolean NOT NULL DEFAULT true;

ALTER TABLE notifications ADD COLUMN sent_at timestamptz;
ALTER TABLE notifications ADD COLUMN send_attempts int NOT NULL DEFAULT 0;
ALTER TABLE notifications ADD COLUMN send_error text;
-- Only reminder rows (email/sms/push) get sent; in_app rows are sent_at NULL
-- forever. Partial index keeps the drain query cheap.
CREATE INDEX notifications_pending_idx ON notifications (created_at)
    WHERE channel IN ('email', 'sms', 'push') AND sent_at IS NULL;

CREATE TABLE push_subscriptions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint text NOT NULL,
    p256dh text NOT NULL,
    auth text NOT NULL,
    user_agent text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, endpoint)
);