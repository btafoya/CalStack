-- Passkey ceremony state is in-memory only (mfa.rs); this table was never read from.
DROP TABLE IF EXISTS webauthn_challenges;
