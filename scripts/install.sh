#!/usr/bin/env bash
# Install CalStack as a systemd service on Debian-family distros.
# Usage: sudo scripts/install.sh [--with-postgres] [--skip-key-gen] [--no-admin] [--non-interactive] [flags]
#   --with-postgres        apt install postgresql and provision a calstack DB
#   --skip-key-gen         don't generate APP_ENCRYPTION_KEY (external key management)
#   --no-admin             skip the interactive create-admin prompt
#   --non-interactive      fail instead of prompting; requires --database-url
#   --database-url URL     DATABASE_URL to write
#   --public-url URL       APP_PUBLIC_URL to write
#   --webauthn-rp-id ID    WEBAUTHN_RP_ID to write
#   --webauthn-origin URL  WEBAUTHN_ORIGIN to write
#   --postmark-secret S    POSTMARK_INBOUND_SECRET to write
set -euo pipefail

BIN=/usr/local/bin/calendar-server
ENV_DIR=/etc/calstack
ENV_FILE=$ENV_DIR/calstack.env
UNIT=/etc/systemd/system/calendar-server.service
USER=calstack

die() { echo "ERROR: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run with sudo/root"
cd "$(cd "$(dirname "$0")/.." && pwd)"

WITH_PG=0; SKIP_KEY=0; NO_ADMIN=0; INTERACTIVE=1
ARG_DB=""; ARG_URL=""; ARG_RPID=""; ARG_ORIGIN=""; ARG_PM=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-postgres) WITH_PG=1;;
    --skip-key-gen) SKIP_KEY=1;;
    --no-admin) NO_ADMIN=1;;
    --non-interactive) INTERACTIVE=0;;
    --database-url) ARG_DB=$2; shift;;
    --public-url) ARG_URL=$2; shift;;
    --webauthn-rp-id) ARG_RPID=$2; shift;;
    --webauthn-origin) ARG_ORIGIN=$2; shift;;
    --postmark-secret) ARG_PM=$2; shift;;
    --help|-h) sed -n '2,12p' "$0"; exit 0;;
    *) die "unknown argument: $1 (see --help)";;
  esac
  shift
done

ask() { # ask VAR PROMPT [DEFAULT] — sets VAR unless it already has a value
  local name=$1 prompt=$2 def=${3:-} val=""
  if [[ -n "${!name:-}" ]]; then return; fi
  if [[ $INTERACTIVE -eq 1 ]]; then
    read -r -p "$prompt${def:+ [$def]}: " val
    val=${val:-$def}
  else
    val=$def
  fi
  printf -v "$name" '%s' "$val"
}

# ============ build ============
if ! command -v cargo >/dev/null; then
  die "cargo not found — install Rust via https://rustup.rs (re-run afterwards)"
fi
echo "==> Building (cargo build --release)…"
cargo build --release
[[ -f target/release/calendar-server ]] || die "build did not produce target/release/calendar-server"

echo "==> Installing files…"
install -m 0755 target/release/calendar-server "$BIN"
getent group "$USER" >/dev/null || groupadd --system "$USER"
id -u "$USER" >/dev/null 2>&1 || useradd --system --no-create-home --gid "$USER" --shell /usr/sbin/nologin "$USER"

# ============ env file ============
mkdir -p "$ENV_DIR"
touch "$ENV_FILE" && chmod 0600 "$ENV_FILE" && chown root:"$USER" "$ENV_FILE"
# shellcheck source=/dev/null
source "$ENV_FILE"  # load existing values so re-runs never clobber

if [[ -n "$ARG_DB" ]]; then DATABASE_URL=$ARG_DB; fi
if [[ -n "$ARG_URL" ]]; then APP_PUBLIC_URL=$ARG_URL; fi
if [[ -n "$ARG_RPID" ]]; then WEBAUTHN_RP_ID=$ARG_RPID; fi
if [[ -n "$ARG_ORIGIN" ]]; then WEBAUTHN_ORIGIN=$ARG_ORIGIN; fi
if [[ -n "$ARG_PM" ]]; then POSTMARK_INBOUND_SECRET=$ARG_PM; fi

if [[ ${APP_ENCRYPTION_KEY:-} == "" && $SKIP_KEY -eq 0 ]]; then
  command -v openssl >/dev/null || die "openssl required to generate APP_ENCRYPTION_KEY (or pass --skip-key-gen)"
  APP_ENCRYPTION_KEY=$(openssl rand -hex 32)
fi

if [[ -z "${DATABASE_URL:-}" ]]; then
  if [[ $INTERACTIVE -eq 0 ]]; then die "DATABASE_URL required in non-interactive mode (--database-url)"; fi
  if [[ $WITH_PG -eq 1 ]]; then
    DATABASE_URL="postgres://$USER@/calstack?host=/var/run/postgresql"
  else
    ask DATABASE_URL "DATABASE_URL (PostgreSQL connection string): "
  fi
fi
[[ -n "${DATABASE_URL:-}" ]] || die "DATABASE_URL is required"
[[ -z "${BIND_ADDR:-}" ]] && BIND_ADDR=$BIND_DEFAULT

ask APP_PUBLIC_URL "APP_PUBLIC_URL (e.g. https://calendar.example.com, blank = skip): " ""
ask WEBAUTHN_RP_ID "WEBAUTHN_RP_ID (passkeys, blank = disable): " ""
if [[ -n "${WEBAUTHN_RP_ID:-}" && -z "${WEBAUTHN_ORIGIN:-}" ]]; then
  ask WEBAUTHN_ORIGIN "WEBAUTHN_ORIGIN (full origin, blank = disable): " ""
fi

esc() { printf "'%s'" "${1//\'/\'\\\'\'}"; }  # single-quote for safe sourcing

{
  echo "# CalStack configuration — managed by scripts/install.sh"
  echo "DATABASE_URL=$(esc "$DATABASE_URL")"
  echo "BIND_ADDR=$(esc "$BIND_ADDR")"
  [ -n "${APP_ENCRYPTION_KEY:-}" ] && echo "APP_ENCRYPTION_KEY=$(esc "$APP_ENCRYPTION_KEY")"
  [ -n "${APP_PUBLIC_URL:-}" ] && echo "APP_PUBLIC_URL=$(esc "$APP_PUBLIC_URL")"
  [ -n "${WEBAUTHN_RP_ID:-}" ] && echo "WEBAUTHN_RP_ID=$(esc "$WEBAUTHN_RP_ID")"
  [ -n "${WEBAUTHN_ORIGIN:-}" ] && echo "WEBAUTHN_ORIGIN=$(esc "$WEBAUTHN_ORIGIN")"
  [ -n "${POSTMARK_INBOUND_SECRET:-}" ] && echo "POSTMARK_INBOUND_SECRET=$(esc "$POSTMARK_INBOUND_SECRET")"
  true
} >"$ENV_FILE"
chmod 0600 "$ENV_FILE"

# ============ optional PostgreSQL ============
if [[ $WITH_PG -eq 1 ]]; then
  echo "==> Installing PostgreSQL…"
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq postgresql
  systemctl enable --now postgresql
  sudo -u postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname='$USER'" | grep -q 1 || \
    sudo -u postgres psql -qc "CREATE ROLE $USER LOGIN"
  sudo -u postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname='$USER'" | grep -q 1 || \
    sudo -u postgres createdb -O "$USER" "$USER"
  echo "==> Verifying database connectivity…"
  sudo -u "$USER" env DATABASE_URL="$DATABASE_URL" "$BIN" check >/dev/null || \
    die "calendar-server check failed against $DATABASE_URL"
fi

# ============ unit ============
PG_AFTER=""; PG_REQ=""
if [[ $WITH_PG -eq 1 ]]; then
  PG_AFTER="postgresql.service"
  PG_REQ="postgresql.service"
fi
cat >"$UNIT" <<EOF
[Unit]
Description=CalStack calendar server
After=network-online.target $PG_AFTER
Wants=network-online.target
Requires=$PG_REQ

[Service]
User=$USER
Group=$USER
EnvironmentFile=$ENV_FILE
ExecStart=$BIN serve
Restart=on-failure
RestartSec=5s
StateDirectory=$USER

# sandboxing
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictSUIDSGID=yes
SystemCallFilter=@system-service @network-io

[Install]
WantedBy=multi-user.target
EOF

echo "==> Enabling service…"
systemctl daemon-reload
systemctl enable calendar-server >/dev/null
systemctl restart calendar-server

HEALTH_ADDR=${BIND_ADDR/0.0.0.0/127.0.0.1}
for _ in $(seq 1 50); do
  curl -s -m 1 "http://$HEALTH_ADDR/healthz" 2>/dev/null | grep -q ok && break
  sleep 0.2
done
curl -s "http://$HEALTH_ADDR/healthz" | grep -q ok || die "service did not answer healthz — see: journalctl -u calendar-server -n 50"
echo "==> CalStack is running on http://$BIND_ADDR (enabled at boot)"

# ============ first admin ============
if [[ $NO_ADMIN -eq 0 && $INTERACTIVE -eq 1 ]]; then
  read -r -p "Create an admin account now? [y/N] " yn
  if [[ $yn =~ ^[Yy] ]]; then
    read -r -p "Username: " au
    read -r -p "Email: " ae
    read -rs -p "Password: " ap; echo
    sudo -u "$USER" env DATABASE_URL="$DATABASE_URL" "$BIN" create-admin "$au" "$ae" "$ap"
    echo "Admin created."
  fi
fi
echo "Done. Logs: journalctl -u calendar-server -f"