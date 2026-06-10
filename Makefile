.PHONY: pre-commit fmt clippy test

pre-commit: fmt clippy test

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --workspace
