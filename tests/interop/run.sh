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
  : keep artifacts
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

step() { echo "== $1"; }

# ============ infrastructure ============
# EXTERNAL_DATABASE_URL points the suite at a running Postgres with the
# `caltest` database already created (CI service container); otherwise a
# throwaway local PostgreSQL 16 is initialized. The URL is used verbatim.
if [[ -n "${EXTERNAL_DATABASE_URL:-}" ]]; then
  : # no local cluster to manage
else
  /usr/lib/postgresql/16/bin/initdb -D "$DATA/pg" -U postgres --auth=trust >/dev/null
  /usr/lib/postgresql/16/bin/pg_ctl -D "$DATA/pg" \
    -o "-p $PGPORT -k $SOCK -c listen_addresses=127.0.0.1" -l "$DATA/pg.log" start >/dev/null
  /usr/lib/postgresql/16/bin/createdb -h 127.0.0.1 -p "$PGPORT" -U postgres caltest
fi
TEST_DB_URL="${EXTERNAL_DATABASE_URL:-postgres://postgres@127.0.0.1:$PGPORT/caltest}"

BASE="http://127.0.0.1:$PORT"
APPKEY="11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff"

DATABASE_URL="$TEST_DB_URL" \
BIND_ADDR="127.0.0.1:$PORT" APP_ENCRYPTION_KEY="$APPKEY" RUST_LOG=warn \
  setsid "$BIN" serve >"$DATA/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 150); do
  curl -s -m 1 "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -s "$BASE/healthz" | grep -q ok || { echo "--- server.log:" >&2; cat "$DATA/server.log" >&2; fail "server did not start (see $DATA/server.log)"; }

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

step "Calendar create rejects a non-slug name with a clear 400 (web UI must slugify before posting)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" -d '{"slug":"My Calendar","name":"My Calendar"}')
[ "$CODE" = 400 ] || fail "expected 400 for invalid slug, got $CODE"

step "Calendar PATCH updates name; DELETE soft-deletes (subsequent GET 404s)"
SCRATCH_CAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" \
  -d '{"slug":"scratch","name":"Scratch"}' | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X PATCH "$BASE/api/calendars/$SCRATCH_CAL" \
  -d '{"name":"Renamed"}' | python3 -c "import json,sys;assert json.load(sys.stdin)['name']=='Renamed'" \
  || fail "calendar PATCH did not apply"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -X DELETE "$BASE/api/calendars/$SCRATCH_CAL" -o /dev/null
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" "$BASE/api/calendars/$SCRATCH_CAL")
[ "$CODE" = 404 ] || fail "deleted calendar should 404 (got $CODE)"

step "ICS import: 3 series round-trip, duplicates skipped, export matches"
ICS_CAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" \
  -d '{"slug":"ics-demo","name":"ICS demo"}' | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
cat > "$DATA/import.ics" <<'EOF'
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//test//test//EN
BEGIN:VEVENT
UID:import-a@example.com
DTSTAMP:20260901T000000Z
DTSTART:20261001T100000Z
DTEND:20261001T110000Z
SUMMARY:Import A
END:VEVENT
BEGIN:VEVENT
UID:import-b@example.com
DTSTAMP:20260901T000000Z
DTSTART:20261002T100000Z
DTEND:20261002T110000Z
SUMMARY:Import B
END:VEVENT
BEGIN:VEVENT
UID:import-r@example.com
DTSTAMP:20260901T000000Z
DTSTART:20261005T100000Z
DTEND:20261005T103000Z
RRULE:FREQ=DAILY;COUNT=3
SUMMARY:Import R
END:VEVENT
BEGIN:VEVENT
UID:import-r@example.com
RECURRENCE-ID:20261006T100000Z
DTSTAMP:20260901T000000Z
DTSTART:20261006T140000Z
DTEND:20261006T143000Z
SUMMARY:Import R moved
END:VEVENT
END:VCALENDAR
EOF
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: text/calendar' --data-binary @"$DATA/import.ics" \
  -X POST "$BASE/api/calendars/$ICS_CAL/import" \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['imported']==3 and r['skipped']==0 and not r['rejected'], r" \
  || fail "first import should place 3 series"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: text/calendar' --data-binary @"$DATA/import.ics" \
  -X POST "$BASE/api/calendars/$ICS_CAL/import" \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['imported']==0 and r['skipped']==3 and not r['rejected'], r" \
  || fail "duplicate import must skip, never overwrite"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$ICS_CAL/export.ics" > "$DATA/export.ics"
grep -q "Import A" "$DATA/export.ics" || fail "export missing imported event"
grep -q "RECURRENCE-ID:20261006T100000Z" "$DATA/export.ics" \
  || fail "export missing the imported exception"

step "ICS import refuses VTODO-only files with a clear 400"
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t@x\r\nDTSTAMP:20260901T000000Z\r\nSUMMARY:Task\r\nEND:VTODO\r\nEND:VCALENDAR\r\n' > "$DATA/tasks.ics"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: text/calendar' --data-binary @"$DATA/tasks.ics" \
  -X POST "$BASE/api/calendars/$ICS_CAL/import")
[ "$CODE" = 400 ] || fail "VTODO-only import should 400, got $CODE"

step "Subscribed calendar: read-only to import and CalDAV writes, owner can unsubscribe"
SUB_CAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" \
  -d '{"slug":"subscribed","name":"Subscribed","source_url":"http://example.invalid/calendar.ics"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "$SUB_CAL" | python3 -c "import json,sys;assert len(sys.argv[1])==36" "$SUB_CAL" \
  || fail "subscribed calendar create failed"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: text/calendar' --data-binary @"$DATA/import.ics" \
  -X POST "$BASE/api/calendars/$SUB_CAL/import")
[ "$CODE" = 403 ] || fail "import into a subscribed calendar should 403, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" \
  -X PUT "$BASE/calendars/alice/subscribed/synced.ics" \
  -H 'content-type: text/calendar' --data-binary @"$DATA/import.ics")
[ "$CODE" = 403 ] || fail "CalDAV PUT to a subscribed calendar should 403, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X PATCH "$BASE/api/calendars/$SUB_CAL" \
  -d '{"source_url":""}')
[ "$CODE" = 200 ] || fail "owner unsubscribe (clear source_url) should 200, got $CODE"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SUB_CAL" \
  | python3 -c "import json,sys;c=json.load(sys.stdin);assert not c['read_only'] and c['source_url'] is None" \
  || fail "cleared source_url should restore a writable calendar"

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

step "Event create carries structured location + attendees; PATCH replaces both"
EV2=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"Located event","starts_at":"2026-09-21T10:00:00Z","ends_at":"2026-09-21T11:00:00Z",
       "location":{"display_name":"Union Station"},"attendees":[{"email":"carol@example.com"}]}')
EV2_ID=$(echo "$EV2" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "$EV2" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d['location']['display_name'] == 'Union Station'
assert d['attendees'][0]['email'] == 'carol@example.com'
" || fail "create did not carry location/attendees"
EV2_ETAG=$(echo "$EV2" | python3 -c "import json,sys;print(json.load(sys.stdin)['etag'])")
EV2_PATCHED=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H "If-Match: $EV2_ETAG" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV2_ID" \
  -d '{"summary":"Located event","starts_at":"2026-09-21T10:00:00Z","ends_at":"2026-09-21T11:00:00Z",
       "location":{"display_name":"New Venue"},"attendees":[{"email":"dave@example.com"}]}')
echo "$EV2_PATCHED" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d['location']['display_name'] == 'New Venue'
assert len(d['attendees']) == 1 and d['attendees'][0]['email'] == 'dave@example.com'
" || fail "patch did not replace location/attendees"

step "PATCH can switch a timed event to all-day and back (mutually exclusive columns)"
EV3=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"Switchable","starts_at":"2026-09-22T10:00:00Z","ends_at":"2026-09-22T11:00:00Z"}')
EV3_ID=$(echo "$EV3" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
EV3_ETAG=$(echo "$EV3" | python3 -c "import json,sys;print(json.load(sys.stdin)['etag'])")
EV3_ALLDAY=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H "If-Match: $EV3_ETAG" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV3_ID" \
  -d '{"summary":"Switchable","all_day":true,"start_date":"2026-09-22","end_date":"2026-09-23"}')
echo "$EV3_ALLDAY" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d['all_day'] is True
assert d['start_date'] == '2026-09-22'
assert d['starts_at'] is None
" || fail "switch to all-day did not clear starts_at"
EV3_ETAG2=$(echo "$EV3_ALLDAY" | python3 -c "import json,sys;print(json.load(sys.stdin)['etag'])")
EV3_TIMED=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H "If-Match: $EV3_ETAG2" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV3_ID" \
  -d '{"summary":"Switchable","all_day":false,"starts_at":"2026-09-22T14:00:00Z","ends_at":"2026-09-22T15:00:00Z"}')
echo "$EV3_TIMED" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d['all_day'] is False
assert d['start_date'] is None
assert d['starts_at'] is not None
" || fail "switch back to timed did not clear start_date"

step "Second account + free-busy isolation"
register bob bob@example.com password456
# bob cannot read alice's calendar
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/bob.jar" "$BASE/api/calendars/$CAL")
[ "$CODE" = 404 ] || fail "bob should not see alice's calendar (got $CODE)"

# ============ 3. CalDAV ============
# App passwords are admin-only since 00a9e13; seed an admin, promote alice,
# then mint her CalDAV app password.
DATABASE_URL="$TEST_DB_URL" "$BIN" create-admin admin admin@example.com adminpass1 >/dev/null
register admin admin@example.com adminpass1
ADMIN_ID=$(curl -s -b "$DATA/admin.jar" "$BASE/api/auth/me" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
ALICE_ID=$(curl -s -b "$DATA/alice.jar" "$BASE/api/auth/me" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -b "$DATA/admin.jar" -H "X-CSRF-Token: $(csrf admin)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/admin/users/$ALICE_ID" -d '{"is_admin":true}' >/dev/null

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

step "CalDAV export includes LOCATION for an event with a structured location"
curl -s -u "$AUTH" "$BASE/calendars/alice/work/$EV2_ID.ics" | grep -q "LOCATION:New Venue" \
  || fail "LOCATION missing from CalDAV export"

step "CalDAV PUT parses LOCATION into a structured location row (import)"
LOC_UUID=$(uuidgen)
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VEVENT\r\nUID:cal-dav-loc@interop\r\nDTSTAMP:20260911T120000Z\r\nDTSTART;TZID=America/Denver:20260916T090000\r\nDTEND;TZID=America/Denver:20260916T100000\r\nSUMMARY:DAV location event\r\nLOCATION:Imported Venue\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' > "$DATA/ev-loc.ics"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$LOC_UUID.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/ev-loc.ics" -D- -o /dev/null | grep -q "201" || fail "CalDAV PUT with LOCATION"
curl -s -u "$AUTH" "$BASE/calendars/alice/work/$LOC_UUID.ics" | grep -q "LOCATION:Imported Venue" \
  || fail "LOCATION did not round-trip through CalDAV PUT/GET"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events?from=2026-09-15T00:00:00Z&to=2026-09-17T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
match = next((e for e in rows if e['summary'] == 'DAV location event'), None)
assert match, 'imported event not found via API'
assert match['location']['display_name'] == 'Imported Venue'
" || fail "API did not surface the location parsed from CalDAV PUT"

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

step "PROPFIND advertises supported-report-set on the calendar collection"
curl -s -u "$AUTH" -X PROPFIND "$BASE/calendars/alice/work/" -H 'Depth: 0' \
  -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:supported-report-set/></D:prop></D:propfind>' \
  | grep -q "sync-collection" || fail "supported-report-set missing sync-collection"

step "calendar-query REPORT honors time-range and expands recurrence"
UUIDQ=$(uuidgen)
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VEVENT\r\nUID:q-rec@interop\r\nDTSTAMP:20260911T120000Z\r\nDTSTART;TZID=America/Denver:20260901T090000\r\nDTEND;TZID=America/Denver:20260901T100000\r\nRRULE:FREQ=DAILY;COUNT=30\r\nSUMMARY:Recurring query probe\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' > "$DATA/q.ics"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUIDQ.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/q.ics" -o /dev/null
Q=$(curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="20260910T000000Z" end="20260911T000000Z"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>')
echo "$Q" | grep -q "Recurring query probe" || fail "calendar-query misses matching recurring master"
echo "$Q" | grep -q "Busy" && fail "calendar-query time-range returned non-overlapping event"
QTEXT=$(curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/work/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:prop-filter name="SUMMARY"><C:text-match>probe</C:text-match></C:prop-filter></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>')
echo "$QTEXT" | grep -q "Recurring query probe" || fail "calendar-query text-match misses"
echo "$QTEXT" | grep -q "Busy" && fail "text-match returned non-matching event"

step "sync-collection on the home set aggregates calendars"
curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:prop><D:getetag/></D:prop><D:limit><D:nresults>2</D:nresults></D:limit></D:sync-collection>' \
  | grep -q "<D:sync-token>" || fail "home-set sync returned no token"
HOMELIMIT=$(curl -s -u "$AUTH" -X REPORT "$BASE/calendars/alice/" -H 'content-type: application/xml' \
  --data-binary '<?xml version="1.0"?><D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:prop><D:getetag/></D:prop><D:limit><D:nresults>2</D:nresults></D:limit></D:sync-collection>')
echo "$HOMELIMIT" | grep -q "work/" || fail "home-set sync href missing slug prefix"

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

step "Share-token CalDAV: read-only DAV via Basic(token, any password)"
DAV_SHARE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$CAL/shares" \
  -d '{"allows_caldav": true}')
DTOKEN=$(echo "$DAV_SHARE" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "$DTOKEN:anything" -X PROPFIND \
  -H 'Depth: 1' "$BASE/calendars/alice/work/")
[ "$CODE" = 207 ] || fail "share principal PROPFIND failed, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "$DTOKEN:anything" -X PUT \
  -H 'content-type: text/calendar' --data-binary "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x1\r\nDTSTART:20261001T100000Z\r\nSUMMARY:evil\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n" \
  "$BASE/calendars/alice/work/x1.ics")
[ "$CODE" = "403" ] || [ "$CODE" = "404" ] || fail "share principal must not write, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "$DTOKEN:anything" -X MKCALENDAR "$BASE/calendars/alice/evilcal")
[ "$CODE" = "403" ] || fail "share principal must not create calendars, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "not-a-token:x" -X PROPFIND -H 'Depth: 1' "$BASE/calendars/alice/work/")
[ "$CODE" = 401 ] || fail "bogus share token must 401, got $CODE"
SHARE_ID=$(echo "$DAV_SHARE" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -X DELETE "$BASE/api/calendars/$CAL/shares/$SHARE_ID" -o /dev/null
CODE=$(curl -s -o /dev/null -w '%{http_code}' -u "$DTOKEN:anything" -X PROPFIND -H 'Depth: 1' "$BASE/calendars/alice/work/")
[ "$CODE" = 401 ] || fail "revoked share must lose DAV access, got $CODE"

step "Audit log records mutations"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/calendars/$CAL" -d '{"description":"audited"}' >/dev/null
AUDIT=$(curl -s -b "$DATA/admin.jar" "$BASE/api/audit?limit=50")
echo "$AUDIT" | python3 -c "
import json,sys
rows=json.load(sys.stdin)
actions=[r['action'] for r in rows]
assert 'PATCH' in actions, actions
assert 'login' in actions, actions
" || fail "audit rows missing for mutation or login: $AUDIT"

step "Webhooks deliver signed payloads to a local receiver"
RXPORT=18098
cat > "$DATA/receiver.py" << 'PYEOF'
import http.server, json, sys, hmac, hashlib
LOG = sys.argv[1]; KEY = sys.argv[2].encode()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        sig = self.headers.get('X-CalStack-Signature', '')
        expect = hmac.new(KEY, body, hashlib.sha256).hexdigest()
        with open(LOG, 'a') as f:
            f.write(json.dumps({"ok": sig == expect, "body": body.decode('utf-8', 'replace')}) + "\n")
        self.send_response(200); self.end_headers(); self.wfile.write(b'{}')
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', int(sys.argv[3])), H).serve_forever()
PYEOF
python3 "$DATA/receiver.py" "$DATA/hook.log" "whsec-test-123" "$RXPORT" 2>/dev/null & RXPID=$!
WH=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/webhooks" \
  -d "{\"url\":\"http://127.0.0.1:$RXPORT/hook\",\"name\":\"interop\",\"sign_key\":\"whsec-test-123\"}")
WH_ID=$(echo "$WH" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$WH_ID" ] || fail "webhook create failed: $WH"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"webhook trigger event","starts_at":"2026-09-23T10:00:00Z","ends_at":"2026-09-23T11:00:00Z"}' >/dev/null
for _ in $(seq 1 60); do grep -q '"ok": true' "$DATA/hook.log" 2>/dev/null && break; sleep 0.25; done
grep -q '"ok": true' "$DATA/hook.log" 2>/dev/null || fail "signed webhook delivery did not arrive or signature mismatch: $(cat "$DATA/hook.log" 2>/dev/null)"
grep -q 'event_created' "$DATA/hook.log" || fail "delivery payload missing trigger"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -X DELETE "$BASE/api/webhooks/$WH_ID")
[ "$CODE" = 200 ] || fail "webhook delete, got $CODE"
kill $RXPID 2>/dev/null || true

step "Search returns hit after indexing"
curl -s -b "$DATA/alice.jar" "$BASE/api/search?q=API" | grep -q "API event" || fail "search"

step "iCal export via occurrences + exceptions"
REC=$(curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/occurrences?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z")
echo "$REC" | grep -q '"occurrence"' || fail "occurrence expansion (got: $(echo "$REC" | head -c 200))"

# ============ 4c. series editing ============
# The web UI's three-way dialog behind it: "this event" (RECURRENCE-ID
# exception), "this and following" (split), "delete this and following".
step "Series editing: this-occurrence exception lands as a RECURRENCE-ID override"
SER_CAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars" \
  -d '{"slug":"series-edit","name":"Series edit"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
SER=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$SER_CAL/events" \
  -d '{"summary":"Series probe","uid":"series-probe@interop","starts_at":"2026-10-01T10:00:00Z","ends_at":"2026-10-01T10:30:00Z","rrule":"FREQ=DAILY;COUNT=5"}')
SER_ID=$(echo "$SER" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z" \
  | python3 -c "import json,sys;rows=json.load(sys.stdin);assert len(rows)==5 and not any(r['is_exception'] for r in rows), rows" \
  || fail "series should expand to 5 plain occurrences"

# "This event": move the 10-03 occurrence to 15:00.
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$SER_CAL/events" \
  -d '{"summary":"Series probe (moved)","uid":"series-probe@interop","master_event_id":"'"$SER_ID"'","recurrence_id_at":"2026-10-03T10:00:00Z","starts_at":"2026-10-03T15:00:00Z","ends_at":"2026-10-03T15:30:00Z"}' \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['master_event_id']=='$SER_ID' and r['recurrence_id']=='2026-10-03T10:00:00', r" \
  || fail "this-occurrence exception should carry the derived wall-clock RECURRENCE-ID"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
assert len(rows) == 5, len(rows)
moved = next((r for r in rows if r['event']['recurrence_id'] == '2026-10-03T10:00:00'), None)
assert moved and moved['is_exception'], rows
assert moved['event']['summary'] == 'Series probe (moved)'
assert moved['event']['starts_at'].startswith('2026-10-03T15:00')
assert moved['occurrence']['at'].startswith('2026-10-03T10:00')
assert any(r['event']['id'] == '$SER_ID' and r['occurrence']['at'].startswith('2026-10-02T10:00') for r in rows)
" || fail "exception should overlay only its own occurrence"

step "Cancelled instance: hidden from /occurrences, still a CANCELLED override over CalDAV"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$SER_CAL/events" \
  -d '{"summary":"Series probe","uid":"series-probe@interop","master_event_id":"'"$SER_ID"'","status":"CANCELLED","recurrence_id_at":"2026-10-04T10:00:00Z","starts_at":"2026-10-04T10:00:00Z","ends_at":"2026-10-04T10:30:00Z"}' >/dev/null
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
assert not any((r['occurrence'] or {}).get('at', '').startswith('2026-10-04') for r in rows), rows
" || fail "cancelled occurrence must be hidden from the occurrence feed"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z&include=cancelled" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
hit = [r for r in rows if (r['occurrence'] or {}).get('at', '').startswith('2026-10-04')]
assert len(hit) == 1 and hit[0]['is_exception'] and hit[0]['event']['status'] == 'CANCELLED', rows
" || fail "include=cancelled must surface the cancelled override"
curl -s -u "$AUTH" "$BASE/calendars/alice/series-edit/$SER_ID.ics" \
  | grep -q "STATUS:CANCELLED" \
  || fail "CalDAV must still serve the cancelled override inside the master"
curl -s -u "$AUTH" "$BASE/calendars/alice/series-edit/$SER_ID.ics" \
  | grep -q "RECURRENCE-ID:20261004T100000Z" \
  || fail "CalDAV override must carry the original wall-clock RECURRENCE-ID"

step "This and following: master truncates, continuation carries the edit, exceptions re-parent"
SPLIT=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/events/$SER_ID/split" \
  -d '{"summary":"Series probe (rest)","recurrence_id_at":"2026-10-05T10:00:00Z","starts_at":"2026-10-05T12:00:00Z","ends_at":"2026-10-05T12:30:00Z"}')
echo "$SPLIT" | grep -q '"ok":true' || fail "split response: $SPLIT"
CONT_ID=$(echo "$SPLIT" | python3 -c "import json,sys;print(json.load(sys.stdin)['continuation_id'])")
[ -n "$CONT_ID" ] || fail "split without continuation id"
curl -s -b "$DATA/alice.jar" "$BASE/api/events/$SER_ID" \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['rrule']=='FREQ=DAILY;COUNT=4', r['rrule']" \
  || fail "master must truncate COUNT to the occurrences before the split"
curl -s -b "$DATA/alice.jar" "$BASE/api/events/$CONT_ID" \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['rrule']=='FREQ=DAILY;COUNT=1', r['rrule'];assert r['starts_at'].startswith('2026-10-05T12:00');assert r['uid']!='series-probe@interop'" \
  || fail "continuation must re-anchor COUNT and start at the split occurrence"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
at = sorted((r['occurrence'] or {}).get('at', '') for r in rows)
assert at == ['2026-10-01T10:00:00Z', '2026-10-02T10:00:00Z', '2026-10-03T10:00:00Z', '2026-10-05T12:00:00Z'], at
" || fail "split should leave 3 master slots + the moved continuation occurrence"
curl -s -u "$AUTH" "$BASE/calendars/alice/series-edit/$CONT_ID.ics" \
  | grep -q "SUMMARY:Series probe (rest)" \
  || fail "continuation must serve as its own CalDAV resource"

step "Delete this and following: master truncates, no continuation, overrides past the wall go"
TRUNC=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/calendars/$SER_CAL/events" \
  -d '{"summary":"Truncate probe","uid":"truncate-probe@interop","starts_at":"2026-11-01T08:00:00Z","ends_at":"2026-11-01T08:30:00Z","rrule":"FREQ=DAILY;COUNT=5"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/events/$TRUNC/split" \
  -d '{"summary":"Truncate probe","truncate_rest":true,"recurrence_id_at":"2026-11-03T08:00:00Z"}' \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['ok'] and r['continuation_id'] is None, r" \
  || fail "truncate split should not create a continuation"
curl -s -b "$DATA/alice.jar" "$BASE/api/events/$TRUNC" \
  | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['rrule']=='FREQ=DAILY;COUNT=2', r['rrule']" \
  || fail "truncated master should keep only its first two occurrences"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-11-01T00:00:00Z&to=2026-11-10T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
at = sorted((r['occurrence'] or {}).get('at', '') for r in rows)
assert at == ['2026-11-01T08:00:00Z', '2026-11-02T08:00:00Z'], at
" || fail "truncated series must stop at the split wall"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$SER_CAL/occurrences?from=2026-10-01T00:00:00Z&to=2026-10-15T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
moved = [r for r in rows if r['event']['summary'] == 'Series probe (moved)']
assert len(moved) == 1, rows
" || fail "the moved exception must survive the split (re-parented or kept)"

# ============ 5. rules ============
step "Rules: create, toggle enabled, list reflects it"
RULE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/rules" \
  -d '{"name":"r1","trigger_type":"event_created","actions":[{"type":"create_notification","title":"hi"}]}')
RULE_ID=$(echo "$RULE" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X PATCH "$BASE/api/rules/$RULE_ID" -d '{"enabled":false}')
[ "$CODE" = 200 ] || fail "rule enable toggle, got $CODE"
curl -s -b "$DATA/alice.jar" "$BASE/api/rules" | grep -q '"enabled":false' || fail "rule disable not reflected in list"

step "Rules are calendar-scoped: per-calendar rule stays out of the global-only list"
GLOBAL_RULE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/rules" -d '{"name":"global-r","trigger_type":"event_created"}')
echo "$GLOBAL_RULE" | grep -q '"id"' || fail "global rule create failed: $GLOBAL_RULE"
CAL_RULE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/rules" -d "{\"name\":\"cal-r\",\"trigger_type\":\"event_created\",\"calendar_id\":\"$CAL\"}")
echo "$CAL_RULE" | grep -q '"id"' || fail "calendar-scoped rule create failed: $CAL_RULE"
curl -s -b "$DATA/alice.jar" "$BASE/api/rules?calendar_id=$CAL" | grep -q '"cal-r"' || fail "calendar-scoped rule missing from scoped list"
curl -s -b "$DATA/alice.jar" "$BASE/api/rules?calendar_id=$CAL" | grep -q '"global-r"' || fail "global rule missing from scoped list"
curl -s -b "$DATA/alice.jar" "$BASE/api/rules" | grep -q '"cal-r"' && fail "calendar-scoped rule leaked into global-only (unscoped) list"
curl -s -b "$DATA/alice.jar" "$BASE/api/rules" | grep -q '"global-r"' || fail "global rule missing from unscoped list"

step "Rules API is admin-gated: non-admin cannot scope a rule at all (403 from the admin gate)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/bob.jar" -H "X-CSRF-Token: $(csrf bob)" \
  -H 'content-type: application/json' -X POST "$BASE/api/rules" \
  -d "{\"name\":\"nope\",\"trigger_type\":\"event_created\",\"calendar_id\":\"$CAL\"}")
[ "$CODE" = 403 ] || fail "bob should not create a rule on alice's calendar, got $CODE"

step "Unknown rule trigger_type is rejected with a clear 400"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/rules" \
  -d '{"name":"bad-trigger","trigger_type":"banana"}')
[ "$CODE" = 400 ] || fail "unknown trigger_type must 400, got $CODE"

step "Rule conditions are honored: a matching rule fires, a non-matching one does not"
COND_RULE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/rules" \
  -d '{"name":"cond-r","trigger_type":"event_updated","conditions":[{"field":"summary","op":"contains","value":"conditioned"}],"actions":[{"type":"create_notification","title":"cond hit","body":"fired"}]}')
echo "$COND_RULE" | grep -q '"id"' || fail "conditional rule create failed: $COND_RULE"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV_ID" -d '{"summary":"conditioned update"}' >/dev/null
ETAG=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/events/$EV_ID" -d '{"summary":"unmatched edit"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['etag'])")
sleep 0.5
HITS=$(curl -s -b "$DATA/alice.jar" "$BASE/api/notifications" \
  | python3 -c "import json,sys;rows=json.load(sys.stdin);print(len([r for r in rows if r.get('title')=='cond hit']))")
[ "$HITS" = "1" ] || fail "conditioned rule should fire exactly once on the matching edit, got $HITS hits"

step "SMS rule action: skipped (not silently ignored) with no Twilio provider configured"
SMS_RULE=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/rules" \
  -d '{"name":"sms-r","trigger_type":"event_created","actions":[{"type":"sms","to":"+15551234567","body":"hi"}]}')
echo "$SMS_RULE" | grep -q '"id"' || fail "sms rule create failed: $SMS_RULE"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"sms trigger 1","starts_at":"2026-09-22T10:00:00Z","ends_at":"2026-09-22T11:00:00Z"}' >/dev/null
for _ in $(seq 1 20); do grep -q "no Twilio provider configured" "$DATA/server.log" && break; sleep 0.2; done
grep -q "no Twilio provider configured" "$DATA/server.log" || fail "sms action should log a skip with no provider configured"

step "Notification providers: create typed Twilio config, list; SMS action now attempts a real send"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/notification-providers" \
  -d '{"kind":"twilio","name":"main","config":{"account_sid":"ACxxx","auth_token":"secret","from":"+15550000000"}}')
[ "$CODE" = 201 ] || fail "create twilio provider, got $CODE"
PROV_ID=$(curl -s -b "$DATA/alice.jar" "$BASE/api/notification-providers" \
  | python3 -c "import json,sys;print([p for p in json.load(sys.stdin) if p['kind']=='twilio'][0]['id'])")
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"sms trigger 2","starts_at":"2026-09-22T12:00:00Z","ends_at":"2026-09-22T13:00:00Z"}' >/dev/null
for _ in $(seq 1 20); do grep -q "rule sms action failed" "$DATA/server.log" && break; sleep 0.2; done
grep -q "rule sms action failed" "$DATA/server.log" || fail "sms action should attempt a real Twilio send once a provider is configured (fake creds are expected to fail, but it must try)"

step "Notification providers: delete"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -X DELETE "$BASE/api/notification-providers/$PROV_ID")
[ "$CODE" = 200 ] || fail "delete twilio provider, got $CODE"

step "Decrypted provider config is session-only (bearer tokens get 403)"
RTOK=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/auth/tokens" -d '{"name":"probe","scopes":["read"]}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['secret'])")
[ -n "$RTOK" ] || fail "read-scoped token create"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  "$BASE/api/notification-providers/00000000-0000-0000-0000-000000000000")
[ "$CODE" = 403 ] || fail "bearer token must not read decrypted provider config, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" \
  "$BASE/api/notification-providers/$PROV_ID")
[ "$CODE" = 404 ] || fail "session GET on a deleted provider should reach the lookup (404), got $CODE"

step "Postmark inbound webhook is fail-closed without POSTMARK_INBOUND_SECRET"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H 'content-type: application/json' \
  -X POST "$BASE/webhooks/postmark/inbound" -d '{"From":"x@example.com"}')
[ "$CODE" = 403 ] || fail "unconfigured secret must fail closed, got $CODE"

step "Bearer tokens cannot mint credentials or touch MFA (session+CSRF only)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/tokens" -d '{"name":"esc"}')
[ "$CODE" = 403 ] || fail "bearer token must not create API tokens, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/app-passwords" -d '{"name":"esc"}')
[ "$CODE" = 403 ] || fail "bearer token must not create app passwords, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/password" \
  -d '{"current_password":"x","new_password":"password123"}')
[ "$CODE" = 403 ] || fail "bearer token must not reset the account password, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/totp/setup")
[ "$CODE" = 403 ] || fail "bearer token must not touch TOTP state, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  -H 'content-type: application/json' -X POST "$BASE/api/auth/webauthn/register/start")
[ "$CODE" = 403 ] || fail "bearer token must not start a passkey enrollment, got $CODE"

step "Passkey enrollment finish endpoint is served at its documented /api path"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" \
  -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/auth/webauthn/register/finish" -d '{"challenge_id":"00000000-0000-0000-0000-000000000000"}')
[ "$CODE" = 400 ] || [ "$CODE" = 422 ] || fail "register/finish should reject an unknown challenge (400/422), got $CODE"

step "Web UI: /rules redirects to its index tab; /admin and /providers pages exist (admin-gated since 00a9e13/3275b18)"
OUT=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' -b "$DATA/admin.jar" "$BASE/rules")
[ "$OUT" = "303 $BASE/?tab=rules" ] || fail "/rules should 303 to /?tab=rules, got $OUT"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" "$BASE/?tab=rules")
[ "$CODE" = 200 ] || fail "/?tab=rules page, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" "$BASE/admin")
[ "$CODE" = 200 ] || fail "/admin page, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" "$BASE/providers")
[ "$CODE" = 200 ] || fail "/providers page, got $CODE"

# Alice stays admin through the rules/providers sections (both admin-gated
# since 3275b18/00a9e13); demote her again so the categories and section-6
# checks exercise the non-admin path. The tenant-wide category must be minted
# while she is still admin — categories live in the creator's tenant.
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/categories" -d '{"slug":"holiday","name":"Holiday","color":"green"}' >/dev/null
curl -s -b "$DATA/admin.jar" -H "X-CSRF-Token: $(csrf admin)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/admin/users/$ALICE_ID" -d '{"is_admin":false}' >/dev/null

# ============ 5b. categories ============
# ============ 5b. tasks + journals (ADR-015) ============
step "Tasks: API create/list/complete/reopen round-trip"
TASKCAL=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars" -d '{"slug":"todo","name":"To-Do","components":["VEVENT","VTODO"]}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$TASKCAL" ] || fail "task calendar create"
TASK=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$TASKCAL/tasks" \
  -d '{"summary":"Interoperability task","due_at":"2026-10-01T17:00:00Z","priority":5,"percent_complete":0}')
TASK_ID=$(echo "$TASK" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$TASK_ID" ] || fail "task create failed: $TASK"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/tasks/$TASK_ID/complete" -o /dev/null
STATUS=$(curl -s -b "$DATA/alice.jar" "$BASE/api/tasks/$TASK_ID" | python3 -c "import json,sys;print(json.load(sys.stdin)['status'])")
[ "$STATUS" = "COMPLETED" ] || fail "complete did not set COMPLETED, got $STATUS"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/tasks/$TASK_ID/reopen" -o /dev/null
STATUS=$(curl -s -b "$DATA/alice.jar" "$BASE/api/tasks/$TASK_ID" | python3 -c "import json,sys;print(json.load(sys.stdin)['status'])")
[ "$STATUS" = "NEEDS-ACTION" ] || fail "reopen did not restore NEEDS-ACTION, got $STATUS"

step "CalDAV: VTODO PUT/GET round-trips on a VTODO-capable collection"
UUIDT=$(uuidgen)
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VTODO\r\nUID:task-wire@interop\r\nDTSTAMP:20260911T120000Z\r\nSUMMARY:Wired task\r\nDUE;VALUE=DATE:20261002\r\nEND:VTODO\r\nEND:VCALENDAR\r\n' > "$DATA/task.ics"
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/todo/$UUIDT.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/task.ics" -o /dev/null
curl -s -u "$AUTH" "$BASE/calendars/alice/todo/$UUIDT.ics" | grep -q "BEGIN:VTODO" \
  || fail "VTODO GET round-trip"
curl -s -u "$AUTH" "$BASE/calendars/alice/todo/$UUIDT.ics" | grep -q "SUMMARY:Wired task" \
  || fail "VTODO summary round-trip"

step "VTODO PUT still rejected with the CalDAV body on a VEVENT-only calendar"
CODE=$(curl -sS -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUIDT.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/task.ics" -o /dev/null -w '%{http_code}' 2>&1)
[ "$CODE" = 403 ] || fail "component gate should refuse VTODO outside its set, got [$CODE]"

step "Journals: API create/get round-trip"
JRN=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/journals" \
  -d '{"summary":"Interop journal","status":"FINAL","description_text":"a note"}')
JRN_ID=$(echo "$JRN" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$JRN_ID" ] || fail "journal create failed: $JRN"

step "Search returns tasks and journals"
curl -s -b "$DATA/alice.jar" "$BASE/api/search?q=Interoperability" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert any(t['summary'] == 'Interoperability task' for t in d.get('tasks', [])), d
" || fail "search did not return the task"

step "Change stream (SSE) delivers a change frame"
curl -s -N -m 6 -b "$DATA/alice.jar" "$BASE/api/changes/stream?since=0" > "$DATA/stream.out" 2>/dev/null &
STREAM_PID=$!
sleep 1
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"stream probe","starts_at":"2026-09-25T10:00:00Z","ends_at":"2026-09-25T11:00:00Z"}' >/dev/null
for _ in $(seq 1 12); do grep -q "event: change" "$DATA/stream.out" 2>/dev/null && break; sleep 0.5; done
kill $STREAM_PID 2>/dev/null || true
grep -q "event: change" "$DATA/stream.out" || fail "SSE stream produced no change frame: $(head -c 200 "$DATA/stream.out")"

step "Categories: create calendar-scoped row as owner, enriches event responses"
CAT=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/categories" \
  -d "{\"calendar_id\":\"$CAL\",\"slug\":\"client\",\"name\":\"Client work\",\"color\":\"blue\"}")
CAT_ID=$(echo "$CAT" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$CAT_ID" ] || fail "category create failed: $CAT"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"Categorized event","categories":["client","unregistered"],"starts_at":"2026-09-23T10:00:00Z","ends_at":"2026-09-23T11:00:00Z"}' >/dev/null
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
ev = next(e for e in rows if e.get('summary') == 'Categorized event')
assert ev['category_details'] == [{'slug': 'client', 'name': 'Client work', 'color': 'blue'}], ev['category_details']
assert ev['categories'] == ['client', 'unregistered'], ev['categories']
" || fail "category_details missing from event list"

step "Rename cascades: events re-tagged in the row's scope"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/categories/$CAT_ID" -d '{"slug":"customers"}' >/dev/null
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
ev = next(e for e in rows if e.get('summary') == 'Categorized event')
assert 'client' not in ev['categories'] and 'customers' in ev['categories'], ev['categories']
assert ev['category_details'] == [{'slug': 'customers', 'name': 'Client work', 'color': 'blue'}], ev['category_details']
" || fail "rename did not cascade to events"

step "Category delete leaves event strings intact"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -X DELETE "$BASE/api/categories/$CAT_ID" -o /dev/null
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z" \
  | python3 -c "
import json,sys
rows = json.load(sys.stdin)
ev = next(e for e in rows if e.get('summary') == 'Categorized event')
assert 'customers' in ev['categories'] and ev.get('category_details') == [], ev
" || fail "delete unexpectedly changed event data"

step "Tenant-wide category create requires admin; bad color rejected"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/bob.jar" -H "X-CSRF-Token: $(csrf bob)" \
  -H 'content-type: application/json' -X POST "$BASE/api/categories" -d '{"slug":"x","name":"X","color":"blue"}')
[ "$CODE" = 403 ] || fail "non-admin tenant-wide create should 403, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -H 'content-type: application/json' -X POST "$BASE/api/categories" \
  -d "{\"calendar_id\":\"$CAL\",\"slug\":\"x\",\"name\":\"X\",\"color\":\"#ff0000\"}")
[ "$CODE" = 400 ] || fail "off-palette color should 400, got $CODE"

step "Tenant-wide categories show in calendar-scoped lists; PATCH carries categories"
# 'holiday' was minted tenant-wide while alice was admin (pre-demote)
TENANT_CAT=$(curl -s -b "$DATA/alice.jar" "$BASE/api/categories?calendar_id=$CAL" \
  | python3 -c "import json,sys;print([c['slug'] for c in json.load(sys.stdin) if c['slug']=='holiday' and c['calendar_id'] is None][0])")
[ "$TENANT_CAT" = "holiday" ] || fail "tenant-wide category missing from calendar-scoped list"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H "If-Match: $ETAG" \
  -H 'content-type: application/json' -X PATCH "$BASE/api/events/$EV_ID" \
  -d '{"summary":"API event","categories":["holiday"]}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['categories']==['holiday'], d['categories']" \
  || fail "PATCH did not apply categories"

step "/categories redirects to its index tab"
OUT=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/categories")
[ "$OUT" = "303 $BASE/?tab=categories" ] || fail "/categories should 303 to /?tab=categories, got $OUT"

# ============ 6. admin user management ============
step "Attachment round-trip: upload, list, download, meta, delete"
ATT_DATA=$(printf 'hello-attachment' | base64)
ATT=$(curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events/$EV_ID/attachments" \
  -d "{\"filename\":\"att.txt\",\"content_type\":\"text/plain\",\"data\":\"$ATT_DATA\"}")
ATT_ID=$(echo "$ATT" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
[ -n "$ATT_ID" ] || fail "attachment upload failed: $ATT"
curl -s -b "$DATA/alice.jar" "$BASE/api/attachments/$ATT_ID" | grep -q "hello-attachment" \
  || fail "attachment download did not return the original bytes"
curl -s -b "$DATA/alice.jar" "$BASE/api/attachments/$ATT_ID/meta" | grep -q '"att.txt"' \
  || fail "attachment meta missing filename"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events/$EV_ID/attachments" \
  | grep -q "$ATT_ID" || fail "attachment not listed on its event"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" \
  -X DELETE "$BASE/api/attachments/$ATT_ID")
[ "$CODE" = 200 ] || fail "attachment delete, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" "$BASE/api/attachments/$ATT_ID")
[ "$CODE" = 404 ] || fail "deleted attachment should 404, got $CODE"

step "Login attempt limiter locks an identity out after repeated failures"
register gateuser gateuser@example.com password123
for _ in $(seq 1 10); do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/login" \
    -H 'content-type: application/json' -d '{"username_or_email":"gateuser","password":"wrong"}')
done
[ "$CODE" = 401 ] || fail "failed logins should 401 before lockout, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/login" \
  -H 'content-type: application/json' -d '{"username_or_email":"gateuser","password":"password123"}')
[ "$CODE" = 403 ] || fail "locked identity must 403 even with the correct password, got $CODE"

step "create-admin CLI seeds an is_admin user"
register admin admin@example.com adminpass1 # already promoted in section 3; login refreshes the session

step "Non-admin is forbidden from admin API"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/alice.jar" "$BASE/api/admin/users")
[ "$CODE" = 403 ] || fail "expected 403 for non-admin on /api/admin/users, got $CODE"

step "Admin can list users"
curl -s -b "$DATA/admin.jar" "$BASE/api/admin/users" | grep -q '"alice"' || fail "admin user list missing alice"

step "Admin can create a user"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" -H "X-CSRF-Token: $(csrf admin)" \
  -H 'content-type: application/json' -X POST "$BASE/api/admin/users" \
  -d '{"username":"carol","email":"carol@example.com","password":"password789"}')
[ "$CODE" = 201 ] || fail "admin create user, got $CODE"

step "Admin can disable a user; disabled user cannot log in"
CAROL_ID=$(curl -s -b "$DATA/admin.jar" "$BASE/api/admin/users" \
  | python3 -c "import json,sys;print([u for u in json.load(sys.stdin) if u['username']=='carol'][0]['id'])")
curl -s -b "$DATA/admin.jar" -H "X-CSRF-Token: $(csrf admin)" -H 'content-type: application/json' \
  -X PATCH "$BASE/api/admin/users/$CAROL_ID" -d '{"disabled":true}' >/dev/null
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/login" -H 'content-type: application/json' \
  -d '{"username_or_email":"carol","password":"password789"}')
[ "$CODE" = 401 ] || fail "disabled user should not log in, got $CODE"

step "Admin cannot demote/disable their own account"
ADMIN_ID=$(curl -s -b "$DATA/admin.jar" "$BASE/api/auth/me" | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" -H "X-CSRF-Token: $(csrf admin)" \
  -H 'content-type: application/json' -X PATCH "$BASE/api/admin/users/$ADMIN_ID" -d '{"is_admin":false}')
[ "$CODE" = 400 ] || fail "self-demote should be rejected, got $CODE"

# ============ 7. restart survival ============
step "Reminders survive a process restart (done criterion)"
# Event due 25s out with an in-app VALARM; the server is killed before the
# trigger passes. After the restart, the scan's 2-minute lookback must still
# create the notification, and the old session must still work.
DUE=$(python3 -c "import datetime;print((datetime.datetime.utcnow()+datetime.timedelta(seconds=25)).strftime('%Y%m%dT%H%M%SZ'))")
DTEND=$(python3 -c "import datetime;print((datetime.datetime.utcnow()+datetime.timedelta(seconds=85)).strftime('%Y%m%dT%H%M%SZ'))")
printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//interop//EN\r\nBEGIN:VEVENT\r\nUID:restart-alarm@interop\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:%s\r\nDTEND:%s\r\nSUMMARY:Restart alarm probe\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:PT0S\r\nDESCRIPTION:fire\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' "$DUE" "$DTEND" > "$DATA/restart.ics"
UUIDR=$(uuidgen)
curl -s -u "$AUTH" -X PUT "$BASE/calendars/alice/work/$UUIDR.ics" -H 'content-type: text/calendar' \
  --data-binary @"$DATA/restart.ics" -o /dev/null
kill "$SRV_PID" 2>/dev/null
# setsid daemonized the server: poll its port down instead of wait()ing on it.
for _ in $(seq 1 25); do
  curl -s -m 1 "$BASE/healthz" >/dev/null 2>&1 || break
  sleep 0.2
done
DATABASE_URL="$TEST_DB_URL" \
BIND_ADDR="127.0.0.1:$PORT" APP_ENCRYPTION_KEY="$APPKEY" RUST_LOG=warn \
  setsid "$BIN" serve >>"$DATA/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 150); do curl -s -m 1 "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -s "$BASE/healthz" | grep -q ok || { echo "--- server.log:" >&2; tail -n 40 "$DATA/server.log" >&2; fail "server did not restart"; }
curl -s -b "$DATA/alice.jar" "$BASE/api/auth/me" | grep -q '"username"' \
  || fail "session did not survive the restart"
for _ in $(seq 1 120); do
  curl -s -b "$DATA/alice.jar" "$BASE/api/notifications" | grep -q "Restart alarm probe" && break
  sleep 1
done
curl -s -b "$DATA/alice.jar" "$BASE/api/notifications" | grep -q "Restart alarm probe" \
  || fail "in-app reminder did not fire after the restart"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/events" \
  -d '{"summary":"post-restart event","starts_at":"2026-09-24T10:00:00Z","ends_at":"2026-09-24T11:00:00Z"}' \
  | grep -q '"id"' || fail "job chain did not re-arm after restart"

# ============ 8. response schemas vs live responses ============
step "Live responses validate against the generated OpenAPI schemas"
curl -s "$BASE/api/openapi.json" > "$DATA/openapi.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/auth/me" > "$DATA/r-me.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars" > "$DATA/r-calendars.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/events" > "$DATA/r-events.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/occurrences" > "$DATA/r-occurrences.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$TASKCAL/tasks" > "$DATA/r-tasks.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/tasks/$TASK_ID" > "$DATA/r-task.json"
curl -s -b "$DATA/alice.jar" -H "X-CSRF-Token: $(csrf alice)" -H 'content-type: application/json' \
  -X POST "$BASE/api/calendars/$CAL/journals" \
  -d '{"summary":"schema-validation journal","status":"FINAL"}' > "$DATA/r-journal.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/changes" > "$DATA/r-changes.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/notifications" > "$DATA/r-notifications.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/search?q=event" > "$DATA/r-search.json"
curl -s -b "$DATA/alice.jar" "$BASE/api/categories" > "$DATA/r-categories.json"
python3 "$ROOT/tests/interop/validate_responses.py" "$DATA/openapi.json" \
  "/api/auth/me" get 200 "$DATA/r-me.json" \
  "/api/calendars" get 200 "$DATA/r-calendars.json" \
  "/api/calendars/{id}/events" get 200 "$DATA/r-events.json" \
  "/api/calendars/{id}/occurrences" get 200 "$DATA/r-occurrences.json" \
  "/api/calendars/{id}/tasks" get 200 "$DATA/r-tasks.json" \
  "/api/tasks/{id}" get 200 "$DATA/r-task.json" \
  "/api/calendars/{id}/journals" post 201 "$DATA/r-journal.json" \
  "/api/changes" get 200 "$DATA/r-changes.json" \
  "/api/notifications" get 200 "$DATA/r-notifications.json" \
  "/api/search" get 200 "$DATA/r-search.json" \
  "/api/categories" get 200 "$DATA/r-categories.json" \
  || fail "response schema validation"

echo "ALL INTEROP CHECKS PASSED"