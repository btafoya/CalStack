# CalStack systemd scripts (Debian)

Install, manage, and uninstall CalStack as a systemd service on Debian-family distros. No Docker involved.

## Install

```bash
git clone https://github.com/btafoya/CalStack.git
cd CalStack
sudo scripts/install.sh                 # you already have PostgreSQL; prompts for DATABASE_URL
sudo scripts/install.sh --with-postgres # also apt install postgresql and provision a `calstack` DB
```

Flags: `--database-url`, `--public-url`, `--webauthn-rp-id`, `--webauthn-origin`, `--postmark-secret`, `--skip-key-gen`, `--no-admin`, `--non-interactive`. See `sudo scripts/install.sh --help`.

What it does: `cargo build --release` (needs Rust), installs the binary to `/usr/local/bin/calendar-server`, creates the `calstack` system user, generates `APP_ENCRYPTION_KEY` (first run only), writes `/etc/calstack/calstack.env` (0600) and a sandboxed `calendar-server.service`, enables and starts the service, then offers to create the first admin. Re-running is safe: it refreshes the binary and env file and never rotates your key.

## Logs

```bash
journalctl -u calendar-server -f          # follow
journalctl -u calendar-server -b          # since boot
journalctl -u calendar-server -p err      # errors only
```

## Config changes

```bash
sudoedit /etc/calstack/calstack.env
sudo systemctl restart calendar-server    # env changes need a restart, not a reload
```

## Upgrade

```bash
git pull
sudo scripts/install.sh                   # rebuilds, swaps the binary, restarts if changed
```

## Uninstall

```bash
sudo scripts/uninstall.sh [--keep-config] [--with-postgres]
```

Removes the unit file, binary, and `/etc/calstack/` (unless `--keep-config`). **Never touches PostgreSQL or its data**; `--with-postgres` only prompts (default: no) before dropping the `calstack` database. The `calstack` system user is left in place.

## Notes

- TLS is not terminated here — put a reverse proxy (nginx, Caddy, Traefik) in front and set `APP_PUBLIC_URL` accordingly.
- The unit is sandboxed (`ProtectSystem=strict`, `NoNewPrivileges`, journald logging). If you need to debug with shell access, temporarily override: `sudo systemctl edit calendar-server`.