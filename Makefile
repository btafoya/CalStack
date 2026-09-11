fmt:
	cargo fmt --all

check:
	cargo check --workspace --all-features

test:
	cargo test --workspace --all-features

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

verify: fmt check lint test
