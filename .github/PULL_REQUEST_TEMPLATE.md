## What

<!-- One paragraph: what changed and why. Link the issue if there is one. -->

## Protocol impact

<!-- If this touches CalDAV/CardDAV/iCalendar behavior: which RFCs/sections, and what test covers the behavior? If not, write "none". -->

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `tests/interop/run.sh` (if protocol or API behavior changed)
- [ ] Migrations added/changed → applied to an empty database and upgrade-tested
- [ ] No secrets or private data in the diff