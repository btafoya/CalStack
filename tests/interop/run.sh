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
DATABASE_URL="postgres://postgres@127.0.0.1:$PGPORT/caltest" "$BIN" create-admin admin admin@example.com adminpass1 >/dev/null
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

step "Search returns hit after indexing"
curl -s -b "$DATA/alice.jar" "$BASE/api/search?q=API" | grep -q "API event" || fail "search"

step "iCal export via occurrences + exceptions"
REC=$(curl -s -b "$DATA/alice.jar" "$BASE/api/calendars/$CAL/occurrences?from=2026-09-01T00:00:00Z&to=2026-10-01T00:00:00Z")
echo "$REC" | grep -q '"occurrence"' || fail "occurrence expansion (got: $(echo "$REC" | head -c 200))"

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

step "Web UI: /rules, /admin and /providers pages exist (admin-gated since 00a9e13/3275b18)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/rules")
[ "$CODE" = 303 ] || fail "anonymous /rules should redirect, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -b "$DATA/admin.jar" "$BASE/rules")
[ "$CODE" = 200 ] || fail "/rules page, got $CODE"
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

step "/categories page exists"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/categories")
[ "$CODE" = 200 ] || fail "/categories page, got $CODE"

# ============ 6. admin user management ============
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

echo "ALL INTEROP CHECKS PASSED"