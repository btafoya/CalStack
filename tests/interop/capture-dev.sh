#!/usr/bin/env bash
# Dev instance for the client fixture session (docs/INTEROP_CAPTURE.md): a
# throwaway Postgres in docker plus the debug server (loopback only unless you
# pass BIND_ADDR=0.0.0.0, see below) with
# DAV_CAPTURE_DIR set, and a disposable "dev" account whose app password you
# type into Reminders, DAVx5, Thunderbird... Ctrl-C stops everything; the
# captured requests stay in the capture directory.
#
# Reaching it from a phone: front it with a TLS reverse proxy (recommended), or
# BIND_ADDR=0.0.0.0. The latter is plain HTTP with open registration; on a host
# with a public IP that is the whole internet, so only for a short session.
#
# Usage: tests/interop/capture-dev.sh   (PORT=8080 PGPORT=55442 BIND_ADDR=127.0.0.1 DAV_CAPTURE_DIR=... to override)
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PORT=${PORT:-8080}; PGPORT=${PGPORT:-55442}; BIND=${BIND_ADDR:-127.0.0.1}; NAME=calstack-capture
CAP=${DAV_CAPTURE_DIR:-$ROOT/captures/$(date +%Y%m%d-%H%M%S)}
BIN="$ROOT/target/debug/calendar-server"
[ -x "$BIN" ] || (cd "$ROOT" && cargo build -p calendar-server) || exit 1
mkdir -p "$CAP"

docker rm -f $NAME >/dev/null 2>&1
docker run -d --rm --name $NAME -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=capture \
  -p 127.0.0.1:$PGPORT:5432 postgres:16-alpine >/dev/null || exit 1
# The image restarts postgres once after init; wait for the second "ready".
for _ in $(seq 1 60); do
  [ "$(docker logs $NAME 2>&1 | grep -c "ready to accept connections")" -ge 2 ] && break; sleep 1
done

DATABASE_URL=postgres://postgres@127.0.0.1:$PGPORT/capture BIND_ADDR=$BIND:$PORT RUST_LOG=info \
  DAV_CAPTURE_DIR="$CAP" \
  APP_ENCRYPTION_KEY=11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff \
  "$BIN" serve &
PID=$!
cleanup() {
  kill $PID 2>/dev/null; docker stop $NAME >/dev/null 2>&1
  echo; echo "Captured $(ls "$CAP" 2>/dev/null | wc -l) requests in $CAP"
}
trap cleanup EXIT
B=http://127.0.0.1:$PORT
for _ in $(seq 1 50); do curl -s -m1 $B/healthz >/dev/null 2>&1 && break; sleep 0.3; done
curl -sf $B/healthz >/dev/null || { echo "server did not start"; exit 1; }

WEBPW=$(openssl rand -hex 12)
curl -s -X POST $B/api/auth/register -H 'content-type: application/json' \
  -d "{\"username\":\"dev\",\"email\":\"dev@example.com\",\"password\":\"$WEBPW\"}" >/dev/null
# App passwords are admin-only.
docker exec $NAME psql -U postgres capture -qc "UPDATE users SET is_admin = true WHERE username='dev'"
J=$(mktemp)
CSRF=$(curl -s -c "$J" -X POST $B/api/auth/login -H 'content-type: application/json' \
  -d "{\"username_or_email\":\"dev\",\"password\":\"$WEBPW\"}" | jq -r .csrf_token)
APPPW=$(curl -s -b "$J" -H "X-CSRF-Token: $CSRF" -H 'content-type: application/json' \
  -X POST $B/api/auth/app-passwords -d '{"name":"clients"}' | jq -r .password)
rm -f "$J"
if [ "$BIND" = 127.0.0.1 ]; then
  IP=127.0.0.1
  NOTE="Loopback only: put a TLS reverse proxy in front to reach it from a phone, or rerun with BIND_ADDR=0.0.0.0."
else
  IP=$(hostname -I 2>/dev/null | awk '{print $1}'); IP=${IP:-$BIND}
  NOTE="EXPOSED on $BIND: plain HTTP, open registration. Stop it as soon as the session is done."
fi

cat <<EOF

  Dev instance is up (disposable data). $NOTE
  Server URL      http://$IP:$PORT/calendars/    (or just http://$IP:$PORT for auto-discovery)
  Username        dev
  App password    $APPPW
  Web UI          http://$IP:$PORT/   (dev / $WEBPW)
  Captures        $CAP

  Follow docs/INTEROP_CAPTURE.md. Ctrl-C when done.

EOF
wait $PID
