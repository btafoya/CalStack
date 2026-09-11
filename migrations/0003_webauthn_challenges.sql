CREATE TABLE webauthn_challenges (
    id uuid PRIMARY KEY,
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    username text,
    kind text NOT NULL,
    state jsonb NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE INDEX webauthn_challenges_expires_at_idx ON webauthn_challenges (expires_at);
