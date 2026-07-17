.PHONY: build test fmt lint check verify-release

build:
	cargo build --workspace

test: build
	cargo test --workspace

fmt:
	cargo fmt

lint:
	cargo fmt -- --check
	cargo clippy --workspace -- -D warnings

check: lint test

verify-release:
ifndef VERSION
	$(error VERSION not set; usage: make verify-release VERSION=vX.Y.Z)
endif
	@scripts/verify-release.sh "$(VERSION)"
