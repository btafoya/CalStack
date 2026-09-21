# Contributing to CalStack

Thanks for your interest in contributing. CalStack is a self-hosted calendar server in Rust with PostgreSQL as its only runtime dependency — CalDAV/CardDAV interop, a normalized OpenAPI domain API, and an embedded web UI. MIT licensed; contributions are welcome.

## Ways to contribute

- **Bug reports** — open a GitHub issue with the CalStack version, client (if CalDAV/CardDAV is involved), and a minimal reproduction. For protocol bugs, include the client name and version.
- **Feature requests** — check [docs/PRD.md](docs/PRD.md) and [docs/DEFERRED_REQUIREMENTS.md](docs/DEFERRED_REQUIREMENTS.md) first; the feature may be planned, deferred deliberately, or settled against by an ADR.
- **Code** — see the workflow below.
- **Protocol conformance** — fixes for RFC 4791/5545/5546/6047/6352/6638 behavior are especially valuable. Real client trace captures help ([docs/INTEROP_CAPTURE.md](docs/INTEROP_CAPTURE.md) explains the capture harness).

## Getting started

```bash
git clone https://github.com/btafoya/CalStack.git
cd CalStack
cargo build --workspace
```

You need:

- **Rust** stable (2024 edition)
- **PostgreSQL 16+** — the only runtime dependency
- **Docker + Docker Compose** (optional, for the dockerized smoke test)

Create a throwaway database for local runs:

```bash
createdb calendar
```

## Development workflow

1. Read the relevant docs first. `docs/ARCHITECTURE.md` covers the layering; `docs/DECISIONS.md` holds binding ADRs — if your change contradicts a settled ADR, raise an issue before writing code rather than re-litigating it in a PR.
2. Write or update tests alongside the change (unit tests, and the interop suite for protocol-visible behavior).
3. Implement the smallest change that satisfies the requirement.
4. Run the checks:

```bash
make fmt      # cargo fmt --all
make check    # cargo check --workspace --all-features
make lint     # cargo clippy --workspace --all-targets --all-features -- -D warnings
make test     # cargo test --workspace --all-features
make verify   # fmt + check + lint + test
make interop  # end-to-end protocol suite against a throwaway PostgreSQL
```

`make interop` is the bar for anything touching CalDAV/CardDAV, auth, sharing, scheduling, or the web UI. It boots a throwaway PostgreSQL 16, builds the server, and drives real protocol flows with curl.

## Codebase conventions

- **Workspace layout** — one crate per concern under `crates/`: `calendar-core` (domain), `calendar-db` (normalized store + migrations), `calendar-caldav`/`calendar-carddav` (protocol), `calendar-api`/`calendar-auth`/`calendar-notify`/`calendar-rules`, `calendar-server` (executable, routing, jobs), `calendar-web` (embedded UI).
- **PostgreSQL is the canonical store** — data lives in normalized tables; iCalendar and vCard are wire formats, never the source of truth. Parse ICS only through `calendar-caldav::parse_ics` (the `icalendar` crate itself rejects folded lines).
- **Migrations** — plain SQL under `migrations/` at the repo root. Never edit an applied migration; add a new one. `crates/calendar-db/build.rs` re-runs on `migrations/` changes — don't remove it.
- **No new runtime dependencies** without strong justification. The architecture mandate is: one binary, one PostgreSQL, nothing else.
- **Web UI** — Bootstrap 5.3 + jQuery 4, vendored assets only (no CDN, no build step). Every non-GET browser call goes through the shared `api()` helper so errors surface to the user.
- **Security** — never log passwords, tokens, passkeys, OTP secrets, provider credentials, or attachment contents. Add auth/authorization tests for any new endpoint.
- **Docs follow behavior** — if a change alters API surface, config, or protocol behavior, update `docs/` and the README in the same PR.

## Commit messages

Concise and terse — describe **what** changed and **why**, as bullet-style lines:

```
Add HMAC verification to webhook deliveries
Fix free-busy query ignoring attendee status
```

No AI-generated attribution lines.

## Submitting changes

1. Fork, create a feature branch off `main`.
2. Keep PRs focused — one logical change per PR.
3. Ensure `make verify` and (where applicable) `make interop` pass; the CI workflow runs fmt, clippy, workspace tests against live PostgreSQL, and the interop suite.
4. Describe the change and the motivation in the PR; link any related issue.
5. All contributions are made under the MIT license; by submitting, you agree your work is distributable on those terms.

## Reporting security issues

Please do **not** open a public issue for security vulnerabilities. Open a private security advisory through GitHub's "Security" tab, or contact the maintainer directly. Include affected version, reproduction steps, and impact.