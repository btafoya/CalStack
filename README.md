<p align="center">
  <img src="assets/logo.png" alt="Daymark" width="200">
</p>

<h1 align="center">Daymark</h1>

<p align="center">
  A standards-first, self-hosted calendar &amp; contacts server.
</p>

<p align="center">
  CalDAV &middot; CardDAV &middot; OpenAPI 3.1 &middot; single Rust binary &middot; PostgreSQL &middot; MIT
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <a href="https://github.com/btafoya/Daymark/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/btafoya/Daymark/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust" src="https://img.shields.io/badge/rust-stable%20(2024%20edition)-orange.svg">
  <img alt="PostgreSQL" src="https://img.shields.io/badge/PostgreSQL-16%2B-blue.svg">
</p>

<p align="center">
  <a href="#installation"><b>Quick start</b></a> ·
  <a href="#api"><b>API</b></a> ·
  <a href="#client-compatibility"><b>Client compatibility</b></a> ·
  <a href="docs/README.md"><b>Documentation</b></a>
</p>

---

Daymark speaks standard CalDAV and CardDAV (RFC 4791 / RFC 6352) and exposes a normalized OpenAPI domain model for everything else. One binary, one database, no Redis, no queue service, no data directory. Protocol behavior is exercised end-to-end by an automated interoperability suite — see [Client compatibility](#client-compatibility) for tested-client status.

Calendar and contact infrastructure without deploying a groupware suite.

```text
                    Internet
                       │
                 Reverse proxy
                (Caddy/nginx/TLS)
                       │
                       ▼
               ┌───────────────┐
               │    Daymark    │   one Rust binary:
               │  web UI + in- │   static assets and the
               │  process jobs │   job worker are embedded
               └───────┬───────┘
       ┌───────────────┼───────────────┐
       ▼               ▼               ▼
    CalDAV          CardDAV         OpenAPI
   RFC 4791        RFC 6352        /api/*
   /calendars      /contacts       apps & integrations
       │               │               │
       └───────────────┼───────────────┘
                       ▼
                 PostgreSQL 16+
      events · tasks · contacts · jobs
```

## Features

### Standards

- **CalDAV** (RFC 4791) — discovery, `MKCALENDAR`, CRUD, `calendar-query`/`calendar-multiget` REPORTs, `sync-collection` incremental sync, `free-busy-query` — for events, tasks, and journals alike.
- **CardDAV** (RFC 6352) — address books, `.well-known/carddav` discovery, vCard CRUD, the same app passwords as CalDAV, plus a normalized contacts API and web page.
- **iCalendar fidelity** — full RRULE/RDATE/EXDATE/RECURRENCE-ID recurrence, hand-rolled and DST-correct, with client-supplied VTIMEZONE definitions parsed, stored per calendar, and honored during expansion (custom tzids never silently become UTC).

### Data & storage

- **PostgreSQL-normalized events** — iCalendar is a wire format, never the source of truth; the same data is addressable through CalDAV, the API, and full-text search.
- **Tasks and journals** — VTODO and VJOURNAL are first-class stored components (ADR-015), not opaque blobs: normalized columns beside `extra_props` for anything unmodelled, full recurrence with overrides, CalDAV and API round-trips, and web pages.
- **Categories** — tenant-wide color-coded registry shared across every calendar; managed from the web UI, carried on events and exposed through the API.
- **ICS import / export / subscriptions** — upload a `.ics` file into any calendar (duplicates skipped by UID, recurrence exceptions round-trip), download any calendar as `.ics`, or subscribe a calendar to a remote `.ics` URL: the server re-fetches it on a schedule, keeps events in sync, and treats the calendar as read-only.
- **Attachments** — capped, stored as `bytea` in PostgreSQL.
- **Search** — PostgreSQL full-text, no external search service.
- **Backup/restore** — portable JSON export/import, attachments included.

### Authentication

- **Auth** — local accounts (Argon2id), WebAuthn/passkeys, TOTP 2FA with recovery codes, scoped API bearer tokens, CalDAV app passwords, and lockout after repeated failed logins.

### Collaboration

- **ACLs** — multiple owners per calendar, owner/read-write/read-only/free-busy capabilities.
- **Public sharing** — revocable, optionally-expiring share tokens; anonymous read-only `.ics` feeds that withhold private/confidential events and attendee contact data. A share token also works as a read-only CalDAV credential when `allows_caldav` is set.
- **Attendees** — invite by email or by phone alone; `sms:` attendee URIs round-trip through iCalendar.
- **Scheduling** — outbound iTIP invitations and cancellations, inbound iMIP replies via a Postmark webhook. Attendees on the same tenant get organizer-rebuilt copies of the event directly — no email involved — and their replies round-trip internally. Sender identity is trusted from Postmark's inbound pipeline (SPF/DKIM/DMARC happen there); the server only checks the From against the attendee list. Do not configure the webhook if you do not trust your inbound mail pipeline.

### Automation

- **Reminders** — VALARMs fire from a PostgreSQL-backed durable job queue (no external scheduler) and reach you however you want: in-app always, plus email, SMS, and Web Push. Pick channels per alarm, opt out per user, and failed sends retry with backoff before giving up with a notice in the app.
- **Rules** — trigger → condition → action automation on event created/updated/deleted (field/op/value conditions, in-app / SMS / webhook actions), scoped to one calendar or tenant-wide; managed from the web UI.
- **Webhooks** — register HMAC-SHA256-signed webhook URLs per tenant; every event create/update/delete (via the API or CalDAV) and rule webhook action delivers an at-least-once signed payload through the durable job queue, with retries, delivery history, and a send-test button.
- **Notifications** — Postmark, generic SMTP, Twilio SMS, and Web Push (VAPID) credentials, all configured from the web UI. Every provider is editable and has a send-test button.
- **Audit trail** — every authenticated API mutation and login event is recorded (actor, action, object, status) and rendered on the Admin page; rows purge after `AUDIT_RETENTION_DAYS`.

### Developer platform

- **OpenAPI 3.1** domain API — the full model, not a CalDAV wrapper. Served live at `/api/openapi.json`, browsable through the vendored Swagger UI at `/docs`.
- **Scoped tokens** — read/write/full bearer scopes enforced server-side; webhooks and a rules engine make the API programmable, not just readable.

### Web UI

- **Embedded web UI** — Bootstrap 5.3 + jQuery 4 + [bs-calendar](https://github.com/ThomasDev-de/bs-calendar), vendored, no CDN, no build step. Calendar view, per-calendar rules, notification providers, and admin user management.

MIT licensed.

## Screenshots

<table>
  <tr>
    <td width="50%"><img src="assets/screenshots/calendar.png" alt="Calendar month view"></td>
    <td width="50%"><img src="assets/screenshots/event-editor.png" alt="Event editor"></td>
  </tr>
  <tr>
    <td><img src="assets/screenshots/tasks.png" alt="Tasks"></td>
    <td><img src="assets/screenshots/journals.png" alt="Journals"></td>
  </tr>
  <tr>
    <td><img src="assets/screenshots/contacts.png" alt="Contacts"></td>
    <td><img src="assets/screenshots/rules.png" alt="Rules"></td>
  </tr>
  <tr>
    <td><img src="assets/screenshots/providers.png" alt="Notification providers"></td>
    <td><img src="assets/screenshots/admin.png" alt="Admin and audit log"></td>
  </tr>
  <tr>
    <td><img src="assets/screenshots/api-docs.png" alt="OpenAPI 3.1 documentation"></td>
    <td><img src="assets/screenshots/login.png" alt="Sign-in page"></td>
  </tr>
</table>

## Requirements

- **PostgreSQL 16+** — the only runtime dependency.
- **A stable x86-64 Linux** for the prebuilt binary (statically linked, works on any distro); other targets build from source.
- **Rust** (stable toolchain, 2024 edition) and a Debian-family distro + systemd — only for the `scripts/install.sh` and build-from-source paths.

## Installation

| Path | When |
|---|---|
| **Prebuilt binary** | Any Linux distro, no Rust toolchain needed |
| **OCI image** | `ghcr.io/btafoya/daymark`, works alongside your existing compose setup |
| **systemd (Debian)** | Bare-metal or VM deployment, service managed by systemd |
| **Build from source** | You manage the process yourself (or another supervisor) |

### Prebuilt binary

Grab `daymark-server` from the [latest release](https://github.com/btafoya/Daymark/releases/latest) — a statically-linked musl binary; the only thing it talks to is PostgreSQL over TCP.

```bash
tar xzf daymark-v*-x86_64-linux.tar.gz
DATABASE_URL=postgres://user:pass@localhost/calendar BIND_ADDR=0.0.0.0:8080 ./daymark-server serve
```

Migrations apply automatically on startup.

### OCI image

```bash
docker run -d --name daymark -p 8080:8080 \
  -e DATABASE_URL=postgres://user:pass@host/calendar \
  ghcr.io/btafoya/daymark:latest
```

Docker is optional — the binary above runs directly on any Linux. A `docker-compose.yml` is also included to run just PostgreSQL (`docker compose up -d`, on `127.0.0.1:5433` by default).

### systemd (Debian-family distros)

```bash
git clone https://github.com/btafoya/Daymark.git
cd Daymark
sudo scripts/install.sh                  # you already have PostgreSQL; prompts for DATABASE_URL
sudo scripts/install.sh --with-postgres  # also apt-installs PostgreSQL and provisions a calstack DB
```

The installer builds from source (needs Rust), installs the binary to `/usr/local/bin/calendar-server`, generates `APP_ENCRYPTION_KEY` for you, writes a sandboxed `calendar-server.service`, enables it at boot, and offers to create the first admin. Idempotent — re-running upgrades the binary safely. Uninstall with `sudo scripts/uninstall.sh` (never touches your database). Details, flags, logs, and config-reload notes: [`scripts/README.md`](scripts/README.md).

### Build from source

```bash
git clone https://github.com/btafoya/Daymark.git
cd Daymark
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
| `AUDIT_RETENTION_DAYS` | no | `90` | Audit-log rows are purged after this many days |
| `POSTMARK_INBOUND_SECRET` | required for inbound iMIP | — | Shared secret validating Postmark's inbound iMIP webhook; the webhook endpoint refuses all traffic (403) while this is unset |
| `APP_PUBLIC_URL` | no | — | Public base URL (e.g. `https://calendar.example.com`) for the click-through link in reminder emails and Web Push payloads. No link is added when unset. |
| `GOOGLE_MAPS_API_KEY` | no | — | Google Places API (New) key enabling place autocomplete in the web UI event form. Key stays server-side; browsers call the `/api/places/*` proxy. Without it, the location field is free text. |
| `IMPORT_MAX_BYTES` | no | `10485760` (10 MB) | Cap on a single ICS import body and on a remote subscription fetch |
| `ICS_SYNC_INTERVAL_SECS` | no | `3600` | How often subscribed remote `.ics` calendars are re-fetched |

\* `WEBAUTHN_RP_ID` and `WEBAUTHN_ORIGIN` must both be set to enable passkey login; otherwise it's disabled and every other auth method still works.

Put Daymark behind a reverse proxy (nginx, Caddy, Traefik) for TLS — it speaks plain HTTP on `BIND_ADDR`.

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

`serve` also starts an in-process worker that scans for due VALARM reminders and dispatches them, sends outbound iTIP invitations, re-fetches subscribed remote `.ics` calendars, and purges expired data on a schedule — no separate process to babysit.

## Usage

### Web UI

Open `http://<BIND_ADDR>/` (redirects to `/login` if unauthenticated). Register an account, create a calendar, and use the built-in week-view calendar to add events. The calendar sidebar covers the ICS lifecycle too: the **+/edit** dialog takes a remote `.ics` URL to subscribe, and the **import/export** buttons in the tab bar upload or download the selected calendar's `.ics`.

- **Account** (nav bar, every signed-in user) — change your password (this revokes every other live session), turn off email/SMS/push reminders if you don't want them, and enable Web Push on the current device.
- **Tasks** (nav bar) — VTODO to-do lists with due dates, priority, recurrence, and completion, in both list and web form.
- **Journals** (nav bar) — VJOURNAL day-notes with per-entry status.
- **Categories** (nav bar) — tenant-wide category registry with colors; entries become selectable on every event form.
- **Contacts** (nav bar) — vCard address book backing the CardDAV endpoint.
- **Rules** (nav bar, scoped to whichever calendar is selected) — create/enable/disable/delete trigger → action automation, per calendar or tenant-wide.
- **Providers** (nav bar) — configure Postmark, SMTP, Twilio, or Web Push (VAPID) credentials, edit them later, and send a test message from any row. Postmark's MessageStream is configurable and defaults to `outbound`; Web Push key pairs are generated for you on first save.
- **Admin** (nav bar, visible only to `is_admin` users) — list accounts, create users, promote/demote admin status, enable/disable accounts.

Self-registration never sets `is_admin` — it's required for the Admin page and the audit log endpoint. Create the first admin via the CLI (see [Running](#running)); every admin after that can be promoted from the Admin page itself.

### CalDAV clients

Point any CalDAV client at:

```
https://<your-host>/
```

Discovery follows the standard `.well-known/caldav` → `current-user-principal` → `calendar-home-set` chain — covered by the automated interop suite — so RFC-compliant clients can auto-configure from that URL alone. CardDAV clients go through `.well-known/carddav` to `/contacts` the same way; a single URL like `https://<your-host>/` covers both.

Outlook has no native CalDAV support and requires a third-party sync add-in; that path is untested.

Per-client setup guides (untested-status caveats included):

- [DAVx⁵ / Android](docs/guide/davx5.md)
- [Apple Calendar & Contacts](docs/guide/apple.md)
- [Thunderbird](docs/guide/thunderbird.md)

### Client compatibility

Real-device interoperability: DAVx⁵ (events and contacts) and a CalDAV-consuming application run against a production instance today; see [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) for the recorded configurations — the matrix grows as testing happens, and [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) is also the test plan for what isn't recorded yet. Protocol-level behavior — discovery, CRUD, `calendar-query`/`calendar-multiget`/`sync-collection`/`free-busy-query` REPORTs, ETag handling — is covered end-to-end by [`tests/interop/run.sh`](tests/interop/run.sh). Interoperability testing with a real client is a valued contribution category; see [CONTRIBUTING.md](CONTRIBUTING.md).

Authenticate with a **CalDAV app password**, not your login password — create one from the web UI or the API:

```bash
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/auth/app-passwords \
  -H 'content-type: application/json' \
  -d '{"name":"my-phone"}'
```

The response's `password` field is shown once; use it as the CalDAV Basic-auth password.

### API

The full OpenAPI 3.1 document is served live at `/api/openapi.json` — point Swagger UI, Redoc, or any codegen tool at it directly. A vendored Swagger UI (no CDN) is served at `/docs`, wired to that same JSON.

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

Session cookies require the `X-CSRF-Token` header on every mutating request. Alternatively, skip cookies entirely and use a scoped bearer token with `Authorization: Bearer <token>` — no CSRF header needed for token auth. Manage tokens and app passwords from the **Credentials** page (`/credentials`) or the API (`POST /api/auth/tokens`).

Token scopes (the `scopes` array at creation, validated server-side):

| Scope | Grants |
|---|---|
| *(empty)* or `full` | Everything (legacy tokens have empty scopes) |
| `write` | All methods, including reads |
| `read` | GET/HEAD only — safe for read-only integrations |

Scopes are enforced by an HTTP-verb router middleware: non-GET with a `read`-only token returns 403. App passwords are not scoped.

### Public sharing

```bash
# create a revocable share link for a calendar you own
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/calendars/<calendar-id>/shares \
  -H 'content-type: application/json' -d '{}'
# -> {"token": "...", ...}
```

The resulting feed (`https://your-host/share/<token>/calendar.ics`) needs no authentication and can be subscribed to from any calendar app. Revoke it any time via `DELETE /api/calendars/<calendar-id>/shares/<share-id>`.

Pass `"allows_caldav": true` when creating the share and the token also works as a read-only CalDAV credential: point a DAV client at the same server, authenticate with the **token as the username** (any password). The share principal sees only that calendar, only PUBLIC events, and can never write; revocation or expiry cuts DAV access on the next request.

### ICS import / export / subscriptions

```bash
# upload an .ics file into a calendar you can write to
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -H 'content-type: text/calendar' --data-binary @events.ics \
  -X POST https://your-host/api/calendars/<calendar-id>/import
# -> {"imported": 3, "skipped": 1, "rejected": [{"uid": "...", "reason": "..."}]}

# download a whole calendar as .ics
curl -s -b cookies.txt \
  https://your-host/api/calendars/<calendar-id>/export.ics -o work.ics
```

Events whose UID already exists live in the target calendar are **skipped, never overwritten**; a series (master plus its `RECURRENCE-ID` exceptions) imports as one unit. Per-series failures are reported in `rejected` and never abort the batch. Tasks and journals are not file-importable — those go through CalDAV.

To follow a remote calendar, create (or patch, owner-only) a calendar with a `source_url`:

```bash
curl -s -b cookies.txt -H "X-CSRF-Token: $CSRF" \
  -X POST https://your-host/api/calendars \
  -H 'content-type: application/json' \
  -d '{"slug":"remote","name":"Remote","source_url":"https://example.com/calendar.ics"}'
```

A subscribed calendar is **read-only** (file imports and CalDAV writes are refused): every `ICS_SYNC_INTERVAL_SECS` the server fetches the remote file (ETag-conditional, size-capped, no redirects followed, private network targets refused), replaces events by UID, and soft-deletes ones the remote no longer lists. Clear it with `PATCH {"source_url": ""}` to turn the calendar back into a normal, writable one.

## Development

```bash
make fmt      # cargo fmt --all
make check    # cargo check --workspace --all-features
make lint     # cargo clippy --workspace --all-targets --all-features -- -D warnings
make test     # cargo test --workspace --all-features
make verify   # fmt + check + lint + test
make interop  # end-to-end suite against a throwaway PostgreSQL instance
```

See [`docs/README.md`](docs/README.md) for design rationale, ADRs, and client-compatibility notes.

## License

MIT — see [`LICENSE`](LICENSE).
