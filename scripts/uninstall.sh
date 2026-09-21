#!/usr/bin/env bash
# Remove the CalStack systemd deployment installed by scripts/install.sh.
# Usage: sudo scripts/uninstall.sh [--keep-config] [--with-postgres]
#   --keep-config     keep /etc/calstack/calstack.env
#   --with-postgres   also offer to drop the calstack database (prompted, default no)
set -euo pipefail

BIN=/usr/local/bin/calendar-server
UNIT=/etc/systemd/system/calendar-server.service
ENV_DIR=/etc/calstack
DBNAME=calstack

die() { echo "ERROR: $*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || die "run with sudo/root"

KEEP=0; PG=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep-config) KEEP=1;;
    --with-postgres) PG=1;;
    --help|-h) sed -n '2,4p' "$0"; exit 0;;
    *) die "unknown argument: $1 (see --help)";;
  esac
  shift
done

systemctl stop calendar-server 2>/dev/null || true
systemctl disable calendar-server 2>/dev/null || true
rm -f "$UNIT"
systemctl daemon-reload
rm -f "$BIN"
if [[ $KEEP -eq 1 ]]; then
  echo "Kept $ENV_DIR"
else
  rm -rf "$ENV_DIR"
fi

if [[ $PG -eq 1 ]]; then
  read -r -p "Drop the '$DBNAME' PostgreSQL database? [y/N] " yn
  if [[ $yn =~ ^[Yy] ]]; then
    sudo -u postgres psql -qc "DROP DATABASE IF EXISTS $DBNAME"
    sudo -u postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname='calstack'" | grep -q 1 && \
      sudo -u postgres psql -qc "DROP ROLE IF EXISTS calstack" || true
  fi
fi

# PostgreSQL itself and the calstack user are left in place: the database may
# hold real data the admin did not ask us to touch.
echo "Uninstalled. PostgreSQL and its data were not touched."