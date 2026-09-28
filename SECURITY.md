# Security Policy

## Reporting a vulnerability

Email **btafoya@briantafoya.com** directly. Please do not open a public issue for a security report.

Include: affected area (CalDAV/CardDAV, API, web UI, auth, webhooks), reproduction steps or a minimal proof of concept, and the commit or release you tested against.

You can expect an initial response within a few days. I will confirm the issue, agree on a fix timeline with you, and credit reporters in the release notes unless asked not to.

## Scope

In scope:

- authentication and session handling (passwords, TOTP, WebAuthn, API tokens, app passwords, share tokens)
- CalDAV/CardDAV authorization boundaries (ACL enforcement, principal isolation)
- the OpenAPI surface (authorization, input validation, injection)
- webhook signing and delivery
- the iMIP inbound webhook
- attachments, public share feeds, data leakage across tenants

Out of scope:

- social engineering
- denial of service by volume
- reports from automated scanners without a demonstrated impact
- vulnerabilities in PostgreSQL itself — report those upstream

## Design notes

- Passwords are stored with Argon2id; API tokens and app passwords are stored hashed.
- TOTP secrets and notification-provider credentials are encrypted at rest with an environment-provided key; the server disables those features if the key is absent rather than storing secrets in plaintext.
- Webhook payloads are signed with HMAC-SHA256.
- The server never logs passwords, tokens, or credential material.