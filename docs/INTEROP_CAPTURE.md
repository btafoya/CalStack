# Client fixture session

Purpose: capture what real clients send so tasks and journals (docs/TASKS_JOURNALS_DESIGN.md, stage 3+) are built on evidence, not memory. It settles risks R2 (recurring-task completion), R3 (`extra_props` fidelity), R4 (client quirks) and the filename question (decision 5). It also validates stage 2b (recurring event overrides, floating times) against real clients.

## Run it

```
tests/interop/capture-dev.sh
```

Starts a throwaway Postgres (docker) and the debug server with `DAV_CAPTURE_DIR` set, creates a disposable `dev` account, and prints the server URL, username and an app password. One file per DAV request lands in `captures/<timestamp>/` (git-ignored): method, URL, headers, body and the response status. `Authorization`/`Cookie` are redacted and `/api` is never captured, but bodies are your calendar content, so use throwaway data in the clients. Ctrl-C stops everything.

**Reachability.** The script binds loopback only. Phones and desktops need a URL they can reach:
- Recommended: a TLS reverse proxy in front of `127.0.0.1:8080` (your existing Caddy works), then use `https://your-host`.
- Short session only: `BIND_ADDR=0.0.0.0 tests/interop/capture-dev.sh`. That is plain HTTP with **open registration**, and on a host with a public IP it is reachable by anyone. Stop it as soon as you are done.
- iOS/macOS may refuse plain HTTP CalDAV accounts (unverified); prefer TLS for Apple.

## Important: tasks are rejected for now

Until stage 3, the server answers `403` to any VTODO/VJOURNAL PUT. That is fine: the rejected request body is exactly the fixture. But a client that has an upload rejected may stop or retry, so it cannot show what it sends for the *next* step. Work around it:

- **One scenario, one item.** Each scenario below uses its own task/journal so an earlier rejection does not block it.
- **Do state-changing steps offline.** For anything of the form "create X, then complete/edit it", pause sync first (airplane mode; or turn off auto-sync in DAVx5), create the item and perform the follow-up steps, then resume sync. The first upload then carries the final state, which is what we need to see.
- Name items exactly as written (`R4 date+time`, ...) so they are easy to find in `grep -l` over the captures.

## Checklist

Do only the clients you have. After each client, note in `captures/<timestamp>/NOTES.txt` its name, version, and what you entered as the server URL.

### Discovery (every client)
Add the account. Try the bare host first (`https://your-host`), then the full `.../calendars/` URL if it fails. The captures show which URLs and methods each client used and where it stopped.

### Events (validates stage 2b), any calendar app
- E1 timed event with a 15-minute alert.
- E2 all-day event.
- E3 weekly recurring event; then change **one** occurrence's time ("this event only").
- E4 delete **one** occurrence of E3's series.
- E5 floating-time event (Apple: Time Zone Support off; Thunderbird: timezone "Floating").

### Apple Reminders (iOS/macOS)
- R1 create a list.
- R2 `R2 plain` reminder.
- R3 `R3 date only`.
- R4 `R4 date+time`.
- R5 `R5 alert` (early reminder).
- R6 `R6 priority high`.
- R7 `R7 daily repeat`: complete it once (offline).
- R8 `R8 subtask`: indent a reminder under another (likely unavailable over CalDAV; note what the app allows).
- R9 delete a reminder; edit a title.

### Tasks.org via DAVx5
- T1 create a task list.
- T2 `T2 plain`.
- T3 `T3 due date`, `T4 due date+time`, `T5 start+due`.
- T6 `T6 priority`, `T7 tag`, `T8 description` (multi-line).
- T9 `T9 parent` with a `T9 child` subtask.
- T10 `T10 repeat from due`: daily, complete once (offline).
- T11 `T11 repeat from completion`: daily, complete once (offline).
- T12 complete a parent that has an open subtask (offline).
- T13 delete a parent that has subtasks (offline).
- T14 drag to reorder two tasks.

### jtx Board via DAVx5
- J1 journal dated today with a description.
- J2 journal with a two-paragraph description.
- J3 note with **no** date.
- J4 journal with a category; J5 journal set to Final, another to Draft.
- J6 a task (VTODO) in the same collection.
- J7 delete a journal.

### Thunderbird
- B1 add the CalDAV calendar (note whether tasks and journals are offered).
- B2 task with no dates; B3 task with a due date.
- B4 recurring task, completed once (offline).
- B5 recurring event with one occurrence changed.

## Turn captures into fixtures

Tell me the capture directory. I read the files, extract what each client actually sends (filenames, VTODO/VJOURNAL shapes, recurring completion, X- properties), and update the design's open risks. Fixtures worth keeping go to `tests/interop/fixtures/<client>/`. Before anything is committed, remove personal data: the capture bodies contain whatever you typed, and headers include your host name.
