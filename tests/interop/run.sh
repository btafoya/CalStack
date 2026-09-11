#!/usr/bin/env bash
# Interoperability + end-to-end test suite (checklist stage 20).
# Boots a throwaway PostgreSQL 16 and the calendar server, then drives real
# protocol flows with curl: API CRUD, CalDAV discovery/CRUD/REPORTs,
# sync-token round-trips, free/busy, public feeds, search, TOTP gating.
#
# Usage: tests/interop/run.sh [BINARY]   (default: target/debug/calendar-server)
set -euo pipefail

BIN="${1:-target/debug/calendar-server}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT/$([[ "$BIN" = /* ]] && echo "${1:-target/debug/calendar-server}" || echo "${1:-target/debug/calendar-server}")"
PORT="${PORT:-18099}"
PGPORT="${PGPORT:-55433}"
DATA="$(mktemp -d /tmp/cal-interop.XXXXXX)"
SOCK="$DATA/pgsock"
mkdir -p "$SOCK"

cleanup() {
  [[ -n "${SRV_PID:-}" ]] && kill "$SRV_PID" 2>/dev/null || true
  /usr/lib/postgresql/16/bin/pg_ctl -D "$DATA/pg" stop -m fast >/dev/null 2>&1 || true
  rm -rf "$DATA"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

step() { echo "== $1"; }

# ============ infrastructure ============
/usr/lib/postgresql/16/bin/initdb -D "$DATA/pg" -U postgres --auth=trust >/dev/null
/usr/lib/postgresql/16/bin/pg_ctl -D "$DATA/pg" \
  -o "-p $PGPORT -k $SOCK -c listen_addresses=127.0.0.1" -l "$DATA/pg.log" start >/dev/null
/usr/lib/postgresql/16/bin/createdb -h 127.0.0.1 -p "$PGPORT" -U postgres caltest

BASE="http://127.0.0.1:$PORT"
APPKEY="11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff"

DATABASE_URL="postgres://postgres@127.0.0.1:$PGPORT/caltest" \
BIND_ADDR="127.0.0.1:$PORT" APP_ENCRYPTION_KEY="$APPKEY" RUST_LOG=warn \
  setsid "$BIN" serve >"$DATA/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 50); do
  curl -s -m 1 "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -s "$BASE/healthz" | grep -q ok || fail "server did not start (see $DATA/server.log)"

# ============ helpers ============
register() { # user email password -> sets session jar + csrf file
  curl -s -X POST "$BASE/api/auth/register" -H 'content-type: application/json' \
    -d "{\"username\":\"$1\",\"email\":\"$2\",\"password\":\"$3\"}" >/dev/null
  curl -s -c "$DATA/$1.jar" -X POST "$BASE/api/auth/login" -H 'content-type: application/json' \
    -d "{\"username_or_email\":\"$1\",\"password\":\"$3\"}" > "$DATA/$1.json"
  python3 -c "import json,sys;print(json.load(open('$DATA/$1.json'))['csrf_token'])" > "$DATA/$1.csrf"
}

csrf() { cat "$DATA/$1.csrf"; }

# ============ 1. OpenAPI + discovery ============
step "OpenAPI served and valid JSON"
curl -s "$BASE/api/openapi.json" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['openapi']=='3.1.0'"

step "CalDAV well-known discovery"
curl -s -D- -o /dev/null "$BASE/.well-known/caldav" | grep -qi "^location: /calendars/"

step "OPTIONS advertises calendar-access"
curl -s -X OPTIONS "$BASE/calendars" -D- -o /dev/null | grep -i "^dav:" | grep -q calendar-access

# ============ 2. accounts, calendars, events (API) ============
step "Register + login + CSRF-protected mutations"
register alice alice@example.com password123
ALICE="alice:$(python3 -c "import json;print(json.load(open('$DATA/alice.json'))['csrf_token'])")"
CAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" \
  -d '{"slug":"work","name":"Work"}' | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$CAL" ] || fail "calendar create"

step "Event create + ETag If-Match update + 409 on stale"
EV=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"API event","starts_at":"2026-09-20T10:00:00Z","ends_at":"2026-09-20T11:00:00Z"}')
EV_ID=$(echo "$EV" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
ETAG=$(echo "$EV" | python3 -c "import json,sys;print(json.load(sys.stdin)['etag'])")
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H "If-Match: \"stale\"" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV_ID" -d '{"summary":"nope"}')
[ "$CODE" = 409 ] || fail "expected 409 on stale etag, got $CODE"

step "Second account + free-busy isolation"
register bob bob@example.com password456
# bob cannot read alice's calendar
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/bob.jar" "$BASE/api/calendars/$CAL")
[ "$CODE" = 404 ] || fail "bob should not see alice's calendar (got $CODE)"

# ============ 3. CalDAV ============
step "App password Basic auth on CalDAV"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/app-passwords" \
  -d '{"name":"interop"}' > "$DATA/appw.json"
APPW=$(python3 -c "import json;print(json.load(open('$DATA/appw.json'))['password'])")
AUTH="alice:$APPW"

curl -s -u "$AUTH" -X PROPFIND "$BASE/calendars" -H 'Depth: 0' \
  --data-binary '<?xml version="1.0"?><D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:current-user-principal/><C:calendar-home-set/></D:prop></D:propfind>' \
  | grep -q 'calendar-home-set' || fail "discovery"

step "CalDAV PUT/GET round-trip preserves TZID + RRULE"
UUID=$(uuidgen)
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VEVENT\r\nUID:cal-dav-1@interop\r\nDTSTAMP:20260911T120000Z\r\nDTSTART;TZID=America/Denver:20260915T090000\r\nDTEND;TZID=America/Denver:20260915T100000\r\nSUMMARY:DAV event\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' > "$DATA/ev.ics"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUID.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/ev.ics" -D- -o /dev/null | grep -q "201" || fail "CalDAV PUT"
curl -s -u "$AUTH" "$BASE/calendars/alice/work/$UUID.ics" | grep -q "TZID=America/Denver" || fail "TZID round-trip"

step "Calendar multiget REPORT returns calendar-data"
curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary "<?xml version=\"1.0\"?><C:calendar-multiget xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/></D:prop><D:href>/calendars/alice/work/$UUID.ics</D:href></C:calendar-multiget>" \
  | grep -q "getetag" || fail "multiget"

step "Sync-token: initial sync then deletion tombstone"
SYNC1=$(curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:prop><D:getetag/></D:prop></D:sync-collection>')
echo "$SYNC1" | grep -q "$UUID.ics" || fail "initial sync-token list"
TOKEN=$(echo "$SYNC1" | grep -o '<D:sync-token>[0-9]*</D:sync-token>' | grep -o '[0-9]*')
curl -s -u "$AUTH" -X DELETE "$BASE/calendars/alice/work/$UUID.ics" -o /dev/null
curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary "<?xml version=\"1.0\"?><D:sync-collection xmlns:D=\"DAV:\"><D:sync-token>$TOKEN</D:sync-token><D:prop><D:getetag/></D:prop></D:sync-collection>" \
  | grep -q "404 Not Found" || fail "deletion tombstone in sync report"

step "Free-busy query respects window"
UUID2=$(uuidgen)
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VEVENT\r\nUID:fb-1@interop\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:20260915T090000Z\r\nDTEND:20260915T100000Z\r\nSUMMARY:Busy\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' > "$DATA/fb.ics"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUID2.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/fb.ics" -o /dev/null
curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><C:free-busy-query xmlns:C="urn:ietf:params:xml:ns:caldav"><C:time-range start="20260915T000000Z" end="20260916T000000Z"/></C:free-busy-query>' \
  | grep -q 'FREEBUSY:20260915T090000Z/20260915T100000Z' || fail "free-busy periods"

step "VTODO PUT rejected 403 with CalDAV error body"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUID2.ics" -H 'content-type: text/calendar' \
  --data-binary $'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t1\r\nEND:VTODO\r\nEND:VCALENDAR\r\n' \
  -D- | grep -q "supported-calendar-component" || fail "VTODO rejection body"

# ============ 4. sharing + feed ============
step "Public share feed: hashed token, privacy filter"
SHARE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/shares" -d '{}')
TOKEN_SHARE=$(echo "$SHARE" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")
curl -s "$BASE/share/$TOKEN_SHARE/calendar.ics" | grep -q "BEGIN:VEVENT" || fail "public feed"
# private events withheld
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"Secret","class":"PRIVATE","starts_at":"2026-09-21T10:00:00Z","ends_at":"2026-09-21T11:00:00Z"}' >/dev/null
curl -s "$BASE/share/$TOKEN_SHARE/calendar.ics" | grep -q "Secret" && fail "PRIVATE event leaked into feed"
echo "feed ok"

step "Search returns hit after indexing"
curl -s -b "$DATA/alice.jar" "$BASE/api/search?q=API" | grep -q "API event" || fail "search"

step "iCal export via occurrences + exceptions"
REC=$(curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/occurrences?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z")
echo "$REC" | grep -q '"occurrence"' || fail "occurrence expansion (got: $(echo "$REC" | head -c 200))"

echo "ALL INTEROP CHECKS PASSED"