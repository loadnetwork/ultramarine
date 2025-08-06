# Heavily inspired by Reth: https://github.com/paradigmxyz/reth/Makefile
.DEFAULT_GOAL := help

# Cargo features for builds.
FEATURES ?=

# Cargo profile for builds.
PROFILE ?= release

# Extra flags for Cargo.
CARGO_INSTALL_EXTRA_FLAGS ?=

CARGO_TARGET_DIR ?= target

##@ Help
.PHONY: help
help: # Display this help.
	@awk 'BEGIN {FS = ":.*#"; printf "Usage:\n  make \033[34m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?#/ { printf "  \033[34m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

##@ Build
.PHONY: build
build: # Build the Ultramarine binary into `target` directory.
	cargo build --bin ultramarine --features "$(FEATURES)" --profile "$(PROFILE)"

.PHONY: install
install: # Build and install the Ultramarine binary under `~/.cargo/bin`.
	cargo install --path bin/ultramarine --force --locked \
		--features "$(FEATURES)" \
		--profile "$(PROFILE)" \
		$(CARGO_INSTALL_EXTRA_FLAGS)

##@ Others
.PHONY: clean
clean: # Run `cargo clean`.
	cargo clean

.PHONY: lint
lint: # Run `clippy` and `rustfmt`.
	cargo +nightly fmt --all
	cargo clippy --all --all-targets --features "$(FEATURES)" --no-deps -- --deny warnings

	# cargo sort
	cargo sort --grouped --workspace

.PHONY: build-debug
build-debug: ## Build the ultramarine binary into `target/debug` directory.
	cargo build --bin ultramarine --features "$(FEATURES)"

.PHONY: test
test: # Run all tests.
	cargo test --workspace -- --nocapture

clean-deps:
	cargo +nightly udeps --workspace --tests --all-targets --release --exclude ef-tests

pr:
	make lint && \
	make test
