<p align="center">
  <img src="assets/logo.png" alt="CalStack" width="200">
</p>

<h1 align="center">CalStack</h1>

<p align="center">
  A fast, self-hosted, single-binary calendar server written in Rust, backed by nothing but PostgreSQL.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

---

CalStack speaks CalDAV to real clients (Apple Calendar, Thunderbird, DAVx5, Outlook) and exposes a normalized OpenAPI domain model for everything else. One binary, one database, no Redis, no queue service, no data directory.

## Features

- **CalDAV** (RFC 4791) discovery, `MKCALENDAR`, CRUD, `calendar-query`/`calendar-multiget` REPORTs, `sync-collection` incremental sync, `free-busy-query`.
- **OpenAPI 3.1** domain API — the full model, not a CalDAV wrapper. Served live at `/api/openapi.json`.
- **PostgreSQL-normalized events** — full RRULE/RDATE/EXDATE/RECURRENCE-ID recurrence, hand-rolled and DST-correct. iCalendar is a wire format, never the source of truth.
- **ACLs** — multiple owners per calendar, owner/read-write/read-only/free-busy capabilities.
- **Public sharing** — revocable, optionally-expiring share tokens; anonymous read-only `.ics` feeds that withhold private/confidential events and attendee contact data.
- **Auth** — local accounts (Argon2id), WebAuthn/passkeys, TOTP 2FA with recovery codes, scoped API bearer tokens, CalDAV app passwords.
- **VALARM reminders** — server-side, fired by a PostgreSQL-backed durable job queue (no external scheduler).
- **Attachments** — capped, stored as `bytea` in PostgreSQL.
- **Search** — PostgreSQL full-text, no external search service.
- **Scheduling** — outbound iTIP invitations, inbound iMIP replies via a Postmark webhook.
- **Rules** — trigger → condition → action automation (event created, RSVP changed, alarm due, …).
- **Notifications** — Postmark, generic SMTP, Twilio SMS.
- **Embedded web UI** — Bootstrap 5.3 + jQuery 4 + [bs-calendar](https://github.com/ThomasDev-de/bs-calendar), vendored, no CDN, no build step.
- **Backup/restore** — portable JSON export/import, attachments included.

MIT licensed.

## Requirements

- **PostgreSQL 16+** — the only runtime dependency.
- **Rust** (stable toolchain, 2024 edition) — only if building from source.
- **Docker + Docker Compose** — only if running via Compose.

## Installation

### Docker Compose

```bash
git clone https://github.com/btafoya/CalStack.git
cd CalStack
cp .env.example .env   # edit as needed
docker compose up --build -d
```

This builds the app image and starts it alongside a PostgreSQL 16 container (named volume, healthcheck-gated). The app listens on `${APP_PORT:-8080}` on the host — set `APP_PORT` in `.env` to change it. Migrations run automatically on startup. Put a reverse proxy (nginx, Caddy, Traefik) in front for TLS.

### Build from source

```bash
git clone https://github.com/btafoya/CalStack.git
cd CalStack
cargo build --release
```

The binary lands at `target/release/calendar-server`. Copy it wherever you deploy; it needs no accompanying files — web UI assets are embedded.

### Database

Create an empty PostgreSQL database for it:

```bash
createdb calendar
```

Migrations run automatically on `serve` startup, or explicitly:

```bash
calendar-server migrate
```

## Configuration

Configuration is environment-variable only — no config files, no CLI flags for settings. Copy `.env.example` to `.env` and edit, or export the variables directly.

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `DATABASE_URL` | yes | — | PostgreSQL connection string |
| `DATABASE_MAX_CONNECTIONS` | no | `16` | Connection pool size |
| `BIND_ADDR` | no | `127.0.0.1:8080` | HTTP listen address |
| `SESSION_TTL_HOURS` | no | `168` (7 days) | Web session lifetime |
| `APP_ENCRYPTION_KEY` | no | — | 32-byte key (64 hex chars or base64) encrypting TOTP secrets and notification-provider credentials. Without it, TOTP and stored provider credentials are unavailable. Generate with `openssl rand -hex 32`. |
| `WEBAUTHN_RP_ID` | no* | — | Effective domain for passkeys, e.g. `calendar.example.com` |
| `WEBAUTHN_ORIGIN` | no* | — | Full origin browsers report, e.g. `https://calendar.example.com` |
| `ATTACHMENT_MAX_BYTES` | no | `52428800` (50 MB) | Per-attachment size cap |
| `RETENTION_DAYS` | no | `30` | Soft-deleted resources are purged after this many days |
| `POSTMARK_INBOUND_SECRET` | no | — | Shared secret validating Postmark's inbound iMIP webhook |

\* `WEBAUTHN_RP_ID` and `WEBAUTHN_ORIGIN` must both be set to enable passkey login; otherwise it's disabled and every other auth method still works.

Put CalStack behind a reverse proxy (nginx, Caddy, Traefik) for TLS — it speaks plain HTTP on `BIND_ADDR`.

## Running

```bash
# apply migrations and start the server
calendar-server serve

# just apply pending migrations, then exit
calendar-server migrate

# verify configuration and DB connectivity
calendar-server check

# export a portable JSON backup (attachments included) to stdout
calendar-server backup > backup.json

# restore into an empty, migrated database
calendar-server restore backup.json

# create the first admin user
calendar-server create-admin <username> <email> <password>
```

Under Docker Compose, run subcommands with `docker compose run --rm app <command> [args]`, e.g.:

```bash
docker compose run --rm app create-admin admin admin@example.com correcthorsebatterystaple
```

`serve` also starts an in-process worker that fires VALARM reminders, sends outbound iTIP invitations, and purges expired data on a schedule — no separate process to run.

## Usage

### Web UI

Open `http://<BIND_ADDR>/` (redirects to `/login` if unauthenticated). Register an account, create a calendar, and use the built-in week-view calendar to add events.

Self-registration never sets `is_admin` — it's required for the audit log endpoint only. Create it via the CLI (see [Running](#running)) or promote an existing account: `UPDATE users SET is_admin = true WHERE username = '...';`.

### CalDAV clients

Point any CalDAV client at:

```
https://<your-host>/
```

Discovery follows the standard `.well-known/caldav` → `current-user-principal` → `calendar-home-set` chain, so most clients (Apple Calendar/Contacts, Thunderbird + built-in CalDAV, DAVx5, Outlook via a CalDAV bridge) auto-configure from that URL alone.

Authenticate with a **CalDAV app password**, not your login password — create one from the web UI or the API:

```bash
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/auth/app-passwords \
  -H 'content-type: application/json' \
  -d '{"name":"my-phone"}'
```

The response's `password` field is shown once; use it as the CalDAV Basic-auth password.

### API

The full OpenAPI 3.1 document is served live at `/api/openapi.json` — point Swagger UI, Redoc, or any codegen tool at it directly.

Quick start:

```bash
# register
curl -s -X POST https://your-host/api/auth/register \
  -H 'content-type: application/json' \
  -d '{"username":"alice","email":"alice@example.com","password":"correcthorse"}'

# log in (stores the session cookie; grab the CSRF token for mutations)
curl -s -c cookies.txt -X POST https://your-host/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"username_or_email":"alice","password":"correcthorse"}'
# -> {"csrf_token": "...", "user": {...}}

# create a calendar
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/calendars \
  -H 'content-type: application/json' \
  -d '{"slug":"work","name":"Work"}'

# create an event
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/calendars/<calendar-id>/events \
  -H 'content-type: application/json' \
  -d '{"summary":"Standup","starts_at":"2026-09-20T09:00:00Z","ends_at":"2026-09-20T09:15:00Z"}'
```

Session cookies require the `X-CSRF-Token` header on every mutating request. Alternatively, skip cookies entirely and use a scoped bearer token (`POST /api/auth/tokens`) with `Authorization: Bearer <token>` — no CSRF header needed for token auth.

### Public sharing

```bash
# create a revocable share link for a calendar you own
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/calendars/<calendar-id>/shares \
  -H 'content-type: application/json' -d '{}'
# -> {"token": "...", ...}
```

The resulting feed (`https://your-host/share/<token>/calendar.ics`) needs no authentication and can be subscribed to from any calendar app. Revoke it any time via `DELETE /api/calendars/<calendar-id>/shares/<share-id>`.

## Development

```bash
make fmt      # cargo fmt --all
make check    # cargo check --workspace --all-features
make lint     # cargo clippy --workspace --all-targets --all-features -- -D warnings
make test     # cargo test --workspace --all-features
make verify   # fmt + check + lint + test
make interop  # end-to-end suite against a throwaway PostgreSQL instance
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/PRD.md`](docs/PRD.md), [`docs/DECISIONS.md`](docs/DECISIONS.md), and [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) for design rationale and client-compatibility notes.

## License

MIT — see [`LICENSE`](LICENSE).
