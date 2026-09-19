#!/usr/bin/env bash
# Integration checks for per-collection component sets and client-chosen
# resource filenames (docs/TASKS_JOURNALS_DESIGN.md, stage 2). Needs docker
# (throwaway postgres:16-alpine) and a built target/debug/calendar-server.
#
# Usage: tests/interop/components_hrefs.sh
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SP=$(mktemp -d); PORT=${PORT:-18130}; PGPORT=${PGPORT:-55441}; B=http://127.0.0.1:$PORT
NAME=calstack-comp-hrefs
FAILS=0
ok()   { echo "  ok   $1"; }
bad()  { echo "  FAIL $1"; FAILS=$((FAILS+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

docker rm -f $NAME >/dev/null 2>&1
docker run -d --rm --name $NAME -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=rig \
  -p 127.0.0.1:$PGPORT:5432 postgres:16-alpine >/dev/null
# The image restarts postgres once after init; wait for the second "ready".
for _ in $(seq 1 60); do
  [ "$(docker logs $NAME 2>&1 | grep -c "ready to accept connections")" -ge 2 ] && break; sleep 1
done
DATABASE_URL=postgres://postgres@127.0.0.1:$PGPORT/rig BIND_ADDR=127.0.0.1:$PORT RUST_LOG=warn DAV_CAPTURE_DIR="$SP/capture" \
  APP_ENCRYPTION_KEY=11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff \
  "$ROOT/target/debug/calendar-server" serve >"$SP/server.log" 2>&1 &
PID=$!
cleanup() { kill $PID 2>/dev/null; docker stop $NAME >/dev/null 2>&1; rm -rf "$SP"; }
trap cleanup EXIT
for _ in $(seq 1 50); do curl -s -m1 $B/healthz >/dev/null 2>&1 && break; sleep 0.3; done
curl -sf $B/healthz >/dev/null || { echo "server did not start:"; tail -5 "$SP/server.log"; exit 1; }

curl -s -X POST $B/api/auth/register -H 'content-type: application/json' \
  -d '{"username":"alice","email":"alice@example.com","password":"password123"}' >/dev/null
# App passwords are admin-only.
docker exec $NAME psql -U postgres rig -qc "UPDATE users SET is_admin = true WHERE username='alice'"
J=$SP/alice.jar
CSRF=$(curl -s -c $J -X POST $B/api/auth/login -H 'content-type: application/json' -d '{"username_or_email":"alice","password":"password123"}' | jq -r .csrf_token)
PW=$(curl -s -b $J -H "X-CSRF-Token: $CSRF" -H 'content-type: application/json' -X POST $B/api/auth/app-passwords -d '{"name":"rig"}' | jq -r .password)
A="alice:$PW"
code() { curl -s -o /dev/null -w '%{http_code}' -u "$A" "$@"; }
ev() { printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//rig//EN\r\nBEGIN:VEVENT\r\nUID:%s\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:20260915T090000Z\r\nDTEND:20260915T100000Z\r\nSUMMARY:%s\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' "$1" "$2"; }
put() { ev "$2" "$3" | curl -s -o /dev/null -w '%{http_code}' -u "$A" -X PUT "$B/calendars/alice/$1" -H 'content-type: text/calendar' --data-binary @-; }
propfind() { curl -s -u "$A" -X PROPFIND "$B/calendars/alice/$1/" -H 'Depth: 0' --data-binary \
  '<?xml version="1.0"?><D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:displayname/><C:supported-calendar-component-set/></D:prop></D:propfind>'; }
sync() { curl -s -u "$A" -X REPORT "$B/calendars/alice/${2:-work}/" -H 'content-type: application/xml' --data-binary \
  "<?xml version=\"1.0\"?><D:sync-collection xmlns:D=\"DAV:\"><D:sync-token>$1</D:sync-token><D:prop><D:getetag/></D:prop></D:sync-collection>"; }

echo "== MKCALENDAR body applied"
[ "$(code -X MKCALENDAR $B/calendars/alice/tasks/ -H 'content-type: application/xml' --data-binary \
 '<?xml version="1.0"?><C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:set><D:prop><D:displayname>My Tasks</D:displayname><C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set></D:prop></D:set></C:mkcalendar>')" = 201 ] && ok "MKCALENDAR 201" || bad "MKCALENDAR 201"
P=$(propfind tasks)
check "displayname from body" 'echo "$P" | grep -q "My Tasks"'
check "component set = VTODO only" '[ "$(echo "$P" | grep -o "comp name=\"[A-Z]*\"" | tr "\n" " ")" = "comp name=\"VTODO\" " ]'
check "MKCALENDAR without body keeps all three" '[ "$(code -X MKCALENDAR $B/calendars/alice/work/)" = 201 ] && [ "$(propfind work | grep -o "comp name=\"[A-Z]*\"" | wc -l)" = 3 ]'

echo "== property text stored as plain text"
check "no-body MKCALENDAR leaves no XML in description" 'curl -s -b $J $B/api/calendars | jq -e "length>0" >/dev/null && ! curl -s -b $J $B/api/calendars | jq -r ".[].description // \"\"" | grep -q "<"'
check "PROPPATCH displayname stores plain text" 'code -X PROPPATCH $B/calendars/alice/work/ --data-binary "<?xml version=\"1.0\"?><D:propertyupdate xmlns:D=\"DAV:\"><D:set><D:prop><D:displayname>Work &amp; Co</D:displayname></D:prop></D:set></D:propertyupdate>" >/dev/null; [ "$(curl -s -b $J $B/api/calendars | jq -r ".[] | select(.slug==\"work\") | .name")" = "Work & Co" ]'

echo "== PUT component gate"
check "VEVENT into VTODO-only list -> 403 + precondition body" 'ev g1 x | curl -s -u "$A" -X PUT $B/calendars/alice/tasks/$(uuidgen).ics -H "content-type: text/calendar" --data-binary @- -i | grep -q "403" && ev g1 x | curl -s -u "$A" -X PUT $B/calendars/alice/tasks/$(uuidgen).ics -H "content-type: text/calendar" --data-binary @- | grep -q "supported-calendar-component"'
check "…and nothing was stored" '[ "$(docker exec $NAME psql -U postgres rig -Atc "select count(*) from events e join calendars c on c.id=e.calendar_id where c.slug='"'"'tasks'"'"'")" = 0 ]'

echo "== client-chosen filenames"
check "PUT non-UUID filename -> 201" '[ "$(put work/my-event.ics n1@x "Non uuid")" = 201 ]'
check "GET by that filename" '[ "$(code "$B/calendars/alice/work/my-event.ics")" = 200 ]'
check "GET body is the event" 'curl -s -u "$A" "$B/calendars/alice/work/my-event.ics" | grep -q "SUMMARY:Non uuid"'
check "filename with space + @ (percent-encoded URL)" '[ "$(put "work/a%20b@c.ics" n2@x Spaced)" = 201 ] && curl -s -u "$A" "$B/calendars/alice/work/a%20b@c.ics" | grep -q "SUMMARY:Spaced"'
check "update in place (PUT again -> 204)" '[ "$(put work/my-event.ics n1@x Renamed)" = 204 ]'
check "still one resource at that name" 'curl -s -u "$A" "$B/calendars/alice/work/my-event.ics" | grep -q "SUMMARY:Renamed"'
U=$(uuidgen)
check "canonical {uuid}.ics still works" '[ "$(put work/$U.ics c1@x Canon)" = 201 ] && curl -s -u "$A" "$B/calendars/alice/work/$U.ics" | grep -q "SUMMARY:Canon"'
check "PROPFIND depth 1 lists the custom filename" 'curl -s -u "$A" -X PROPFIND "$B/calendars/alice/work/" -H "Depth: 1" --data-binary "<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\"><D:prop><D:getetag/></D:prop></D:propfind>" | grep -q "my-event.ics"'

echo "== sync-collection (prefixed sync-token must be incremental)"
S1=$(sync "")
check "initial sync lists custom + encoded names" 'echo "$S1" | grep -q "my-event.ics" && echo "$S1" | grep -q "a%20b%40c.ics"'
T=$(echo "$S1" | grep -o '<D:sync-token>[0-9]*' | grep -o '[0-9]*')
put work/later.ics n3@x Later >/dev/null
S2=$(sync "$T")
check "incremental: only the new resource" 'echo "$S2" | grep -q "later.ics" && ! echo "$S2" | grep -q "my-event.ics"'

echo "== delete"
check "DELETE by filename -> 204" '[ "$(code -X DELETE "$B/calendars/alice/work/my-event.ics")" = 204 ]'
check "GET after delete -> 404" '[ "$(code "$B/calendars/alice/work/my-event.ics")" = 404 ]'
S3=$(sync "$T")
check "sync reports the deletion under its filename" 'echo "$S3" | grep -A1 "my-event.ics" | grep -q "404"'

echo "== series: master + overrides are one resource"
putraw() { curl -s -o /dev/null -w '%{http_code}' -u "$A" -X PUT "$B/calendars/alice/$1" -H 'content-type: text/calendar' --data-binary "$2"; }
getics() { curl -s -u "$A" "$B/calendars/alice/$1" | tr -d '\r'; }
etag() { curl -s -D- -o /dev/null -u "$A" "$B/calendars/alice/$1" | tr -d '\r' | grep -i '^etag:'; }
hrefs() { curl -s -u "$A" -X PROPFIND "$B/calendars/alice/$1/" -H 'Depth: 1' --data-binary '<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:propfind>' | grep -o '\.ics</D:href>' | wc -l; }
VC=$'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//rig//EN\r\n'
SER=$'BEGIN:VEVENT\r\nUID:series-1@x\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:20260915T090000Z\r\nDTEND:20260915T100000Z\r\nRRULE:FREQ=DAILY;COUNT=5\r\nSUMMARY:Series\r\nEND:VEVENT\r\n'
OV1=$'BEGIN:VEVENT\r\nUID:series-1@x\r\nDTSTAMP:20260911T120000Z\r\nRECURRENCE-ID:20260916T090000Z\r\nDTSTART:20260916T110000Z\r\nDTEND:20260916T120000Z\r\nSUMMARY:Moved\r\nEND:VEVENT\r\n'
OV2=$'BEGIN:VEVENT\r\nUID:series-1@x\r\nDTSTAMP:20260911T120000Z\r\nRECURRENCE-ID:20260917T090000Z\r\nDTSTART:20260917T090000Z\r\nDTEND:20260917T100000Z\r\nSUMMARY:Renamed\r\nEND:VEVENT\r\n'
END=$'END:VCALENDAR\r\n'
code -X MKCALENDAR $B/calendars/alice/series/ >/dev/null
check "PUT master + 2 overrides -> 201" '[ "$(putraw series/s.ics "$VC$SER$OV1$OV2$END")" = 201 ]'
check "GET returns all three VEVENTs" '[ "$(getics series/s.ics | grep -c "^BEGIN:VEVENT")" = 3 ]'
check "…with both RECURRENCE-IDs" 'getics series/s.ics | grep -q "^RECURRENCE-ID:20260916T090000Z" && getics series/s.ics | grep -q "^RECURRENCE-ID:20260917T090000Z"'
check "collection lists the series once" '[ "$(hrefs series)" = 1 ]'
T2=$(sync "" series | grep -o '<D:sync-token>[0-9]*' | grep -o '[0-9]*')
check "initial sync reports one href" '[ "$(sync "" series | grep -o "\.ics</D:href>" | wc -l)" = 1 ]'
E1=$(etag series/s.ics)
check "re-PUT without one override -> 204" '[ "$(putraw series/s.ics "$VC$SER$OV1$END")" = 204 ]'
check "…the removed override is gone" '[ "$(getics series/s.ics | grep -c "^BEGIN:VEVENT")" = 2 ] && ! getics series/s.ics | grep -q "RECURRENCE-ID:20260917"'
check "…etag changed and sync reports the series" '[ "$(etag series/s.ics)" != "$E1" ] && sync "$T2" series | grep -q "s.ics"'
check "override-only PUT -> 403" '[ "$(putraw series/o.ics "$VC$OV1$END")" = 403 ]'
check "mixed UIDs in one resource -> 403" '[ "$(putraw series/m.ics "$VC$SER${OV1/series-1@x/other@x}$END")" = 403 ]'
MID=$(docker exec $NAME psql -U postgres rig -Atc "select id from events where uid='series-1@x' and master_event_id is null")
CAL=$(curl -s -b $J $B/api/calendars | jq -r '.[]|select(.slug=="series")|.id')
E2=$(etag series/s.ics)
check "API-created override -> 201" '[ "$(curl -s -o /dev/null -w "%{http_code}" -b $J -H "X-CSRF-Token: $CSRF" -H "content-type: application/json" -X POST $B/api/calendars/$CAL/events -d "{\"uid\":\"series-1@x\",\"summary\":\"API override\",\"starts_at\":\"2026-09-18T09:00:00Z\",\"ends_at\":\"2026-09-18T10:00:00Z\",\"master_event_id\":\"$MID\",\"recurrence_id\":\"2026-09-18T09:00:00\"}")" = 201 ]'
check "…it refreshes the master etag and joins the resource" '[ "$(etag series/s.ics)" != "$E2" ] && getics series/s.ics | grep -q "SUMMARY:API override" && [ "$(hrefs series)" = 1 ]'

echo "== floating times"
FL=$'BEGIN:VEVENT\r\nUID:float-1@x\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:20260920T090000\r\nDTEND:20260920T100000\r\nSUMMARY:Floating\r\nEND:VEVENT\r\n'
check "floating PUT -> 201" '[ "$(putraw series/f.ics "$VC$FL$END")" = 201 ]'
check "floating times come back bare (no Z, no TZID)" 'getics series/f.ics | grep -q "^DTSTART:20260920T090000$" && getics series/f.ics | grep -q "^DTEND:20260920T100000$" && ! getics series/f.ics | grep -q TZID'

echo "== delete then re-PUT the same UID"
check "DELETE master -> 204" '[ "$(code -X DELETE "$B/calendars/alice/series/s.ics")" = 204 ]'
check "same UID, same filename -> 201 (brought back)" '[ "$(putraw series/s.ics "$VC$SER$OV1$END")" = 201 ] && [ "$(getics series/s.ics | grep -c "^BEGIN:VEVENT")" = 2 ]'
check "DELETE again, same UID under a new filename -> 201" '[ "$(code -X DELETE "$B/calendars/alice/series/s.ics")" = 204 ] && [ "$(putraw series/s2.ics "$VC$SER$END")" = 201 ] && [ "$(code "$B/calendars/alice/series/s2.ics")" = 200 ] && [ "$(code "$B/calendars/alice/series/s.ics")" = 404 ]'
check "UID still live at another filename -> 403" '[ "$(putraw series/s3.ics "$VC$SER$END")" = 403 ]'

echo "== discovery starts with PROPFIND on the well-known URLs (RFC 6764)"
check "PROPFIND /.well-known/caldav -> 308 /calendars/" '[ "$(curl -s -o /dev/null -w "%{http_code} %{redirect_url}" -X PROPFIND $B/.well-known/caldav)" = "308 $B/calendars/" ]'
check "PROPFIND /.well-known/carddav -> 308 /contacts/" '[ "$(curl -s -o /dev/null -w "%{http_code} %{redirect_url}" -X PROPFIND $B/.well-known/carddav)" = "308 $B/contacts/" ]'

echo "== dev request capture (DAV_CAPTURE_DIR)"
FIRST_PUT=$(grep -l "^PUT /calendars/alice/work/my-event.ics" "$SP"/capture/*.txt | head -1)
check "a PUT is captured with its response status" '[ -n "$FIRST_PUT" ] && grep -q "^status: 201" "$FIRST_PUT"'
check "the request body is captured" 'grep -rq "SUMMARY:Non uuid" "$SP/capture"'
check "credentials are never written" '! grep -rqF -e "$PW" -e password123 -e "Basic " "$SP/capture" && grep -rq "^authorization: <redacted>" "$SP/capture"'
check "/api traffic is not captured" '! grep -rq "^POST /api/" "$SP/capture"'

echo "== server health"
check "no failed jobs / errors in log" '! grep -qE "job failed|ERROR" "$SP/server.log"'
echo; [ $FAILS = 0 ] && echo "ALL PASS" || { echo "$FAILS FAILED"; exit 1; }
