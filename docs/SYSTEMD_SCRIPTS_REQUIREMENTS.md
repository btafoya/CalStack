# Requirements: systemd autostart scripts for Debian (`scripts/`)

Status: requirements only — implementation pending.

## Goal

First-class bare-metal deployment on Debian-family distros: `scripts/` ships an installer that builds CalStack from source, installs a hardened systemd unit, and gets the service running with a single command. No Docker required.

## User stories

1. As a Debian admin, I run `sudo scripts/install.sh` on a fresh checkout and end up with `calendar-server` built, installed, running under systemd, and enabled at boot.
2. As a Debian admin without PostgreSQL, I run `sudo scripts/install.sh --with-postgres` and get PostgreSQL installed, a `calstack` database created, and a working local `DATABASE_URL` configured.
3. As a Debian admin, I can stop and fully remove the deployment with `sudo scripts/uninstall.sh` without touching my PostgreSQL data.
4. As a Debian admin, I can read `scripts/README.md` to learn how to inspect logs (`journalctl -u calendar-server`), reload config, and upgrade.

## Functional requirements

### `scripts/install.sh`
- **Root-gated**: refuses to run non-interactively without `sudo`/root (except `--help`).
- **Binary source**: runs `cargo build --release` in the repo checkout; installs the resulting binary to `/usr/local/bin/calendar-server`. Refuses to proceed if the build fails; warns (not fails) if `cargo` is missing, pointing at rustup.
- **Idempotent**: safe to re-run. Re-running refreshes the binary and env file; never clobbers an existing `APP_ENCRYPTION_KEY`; restarts the service only when something changed.
- **Files installed**:
  - `/usr/local/bin/calendar-server` (binary)
  - `/etc/calstack/calstack.env` (environment file, mode 0600, owner `root:calstack`)
  - `/etc/systemd/system/calendar-server.service` (from template)
- **System user**: creates `calstack` system user/group (`--system --no-create-home`).
- **Encryption key**: auto-generates `APP_ENCRYPTION_KEY` with `openssl rand -hex 32` on first install; preserves an existing one on re-runs. `--skip-key-gen` disables generation for externally-managed keys.
- **Admin prompt**: after start, offers to run `calendar-server create-admin` interactively; skippable with `--no-admin`.
- **PostgreSQL modes**:
  - Default: verifies connectivity only — runs `calendar-server check` with the configured env before enabling the service; aborts with the check output on failure.
  - `--with-postgres`: `apt install postgresql`, creates database `calstack` and grants it to the `calstack` role (peer auth via the `calstack` user), writes the local `DATABASE_URL`, and enables the default cluster via `systemctl`.
- **Config flow**: prompts for `DATABASE_URL`, `APP_PUBLIC_URL`, and optional extras (WebAuthn RP ID/origin, Postmark inbound secret) if not passed as flags or found in an existing env file; non-interactive mode (`CI=1` or `--non-interactive`) requires flags and fails loudly on missing required values.
- **Post-install**: `systemctl daemon-reload`, `systemctl enable --now calendar-server`, then polls `/healthz` and prints the bound address.

### `scripts/calendar-server.service` (template, filled at install)
- Runs `ExecStart=/usr/local/bin/calendar-server serve` with `EnvironmentFile=/etc/calstack/calstack.env`.
- Dedicated user `calstack`, group `calstack`.
- Sandboxing: `NoNewPrivileges=yes`, `PrivateTmp=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `ReadWritePaths=` (none needed — no filesystem data directory), `ProtectKernelTunables=yes`, `ProtectKernelModules=yes`, `ProtectControlGroups=yes`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`, `RestrictSUIDSGID=yes`, `SystemCallFilter=@system-service` (with `@network-io` allowed).
- `StateDirectory=calstack` (future-proofing; the app itself needs no data dir).
- `Restart=on-failure`, `RestartSec=5s`; journald for output (`StandardOutput=journal`).
- `After=network-online.target postgresql.service` + `Wants=network-online.target`; the postgresql dependency only when installed with `--with-postgres` (template placeholder handled by the installer).

### `scripts/uninstall.sh`
- Root-gated, idempotent.
- Stops and disables the service, `daemon-reload`, removes the unit, binary, and `/etc/calstack/` (with a `--keep-config` escape hatch preserving the env file).
- Never touches PostgreSQL or its data; in `--with-postgres` mode offers (prompted, default no) to drop the `calstack` database.
- Reports anything non-standard it declines to delete rather than guessing.

### `scripts/README.md`
- Quick install/upgrade/uninstall commands.
- Log inspection: `journalctl -u calendar-server -f`, boot-scoped, and grep-for-error patterns.
- Config reload flow: edit `/etc/calstack/calstack.env`, `systemctl restart calendar-server` (env changes need a restart, not a reload).
- Note that TLS is not terminated by the unit — reverse proxy in front, as per the README.
- Explicitly lists what uninstall removes and what it never removes.

## Non-functional requirements

- POSIX-compatible bash (`set -euo pipefail`), no dependencies beyond `systemctl`, `apt`, `openssl`, `curl` (all standard on Debian).
- Never writes secrets to stdout logs; env file permissions 0600.
- No modification of files outside the declared install set.
- Scripts live only in `scripts/`; nothing in the Rust build depends on them.
- CI does not exercise these scripts (no root container); correctness is manual-test documented in `scripts/README.md`.

## Acceptance criteria

1. On a clean Debian 12/13 VM: `git clone`, `sudo scripts/install.sh --with-postgres` → service running, `/healthz` returns `ok` after reboot.
2. Re-running install.sh changes nothing except the binary (no key rotation, no duplicate unit).
3. `sudo scripts/uninstall.sh` leaves the system without the service, binary, unit file, or `/etc/calstack/`; PostgreSQL data intact.
4. Service survives reboot (`systemctl is-enabled` = enabled) and restarts automatically on crash.
5. `journalctl -u calendar-server` shows server logs; nothing sensitive appears in journal output.

## Open questions

- Should a future release tarball download mode be added to install.sh (currently build-from-source only)? Deferred until releases exist.
- Debian packaging (`deb` via cargo-deb) is out of scope for this iteration; revisit if the scripts feel insufficient.