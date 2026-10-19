CARGO ?= cargo

.PHONY: build release install test check test-live

build:
	$(CARGO) build --locked

release:
	$(CARGO) build --release --locked

install:
	$(CARGO) install --path . --locked

test:
	$(CARGO) test --locked

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --locked --all-targets -- -D warnings
	$(CARGO) test --locked

test-live:
	$(CARGO) test --locked --test duckduckgo_live -- --ignored --nocapture
