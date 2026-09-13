# Changes

- [x] Use AM/PM instead of 24hr time in the web ui
- [x] Do not show a calendar unless it is selected
- [x] Page height should be 100% of the viewport with no scrollbars. Same for width. (USe bootstrap methods)

# Fixes

- [x] Calendar sidebar collapses into the far left sidebar - this should be redesigned
- [x] What exactly are tasks? That should be removed.
- [x] The datetime selector in the calendar webui is using 24hr time, not AM/PM - this was missed during the above changes.
- [ ] When the calendar first loads, no events show. Click on the calendar label again and the events show. Strange bug
  - Root cause is inside the vendored bs-calendar 2.4.0 bundle (confirmed the latest upstream release, github.com/ThomasDev-de/bs-calendar — not a version we're behind on): its week view computes a wrong internal fetch/paint date range (verified: same wrong range regardless of `startDate`/`date` construction options, `setDate()`/`setToday()`, or destroy+reconstruct). That same wrong range gates which returned appointments get painted, not just what gets fetched. Our `url`/`requestData` usage matches their documented contract exactly, so this isn't a misuse on our side.
  - Applied and kept: lazy-construct the widget only once a calendar is actually selected (was being built while hidden), and compute the fetch's date range from the rendered day-header cells (`.wc-day-header[data-date]`) instead of trusting the plugin's own `requestData.fromDate/toDate`. This makes the outgoing `/occurrences` request correct, but appointments still don't render — the separate paint-side range check inside the bundle is unreachable from any public API and needs a vendor patch or a fixed release upstream. Worth filing as an issue against the library.
  - Default view switched to `month` (2026-09-12). Confirmed the same upstream defect hits month view too, and worse: on first load it rendered "June 2026" with the 10th highlighted as today, instead of September 2026/the 12th. No DOM-based fetch-range mitigation applied for month view yet (only week view has one) — this is a known, accepted limitation, not a regression.