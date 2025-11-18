# This Makefile drives every day development tasks for the `ultramarine`
# codebase.  It combines the ergonomics of cargo with the repeatability
# of a traditional build system. Where sensible
# we lean on cargo for dependency resolution and building, but we also
# provide a number of quality‑of‑life targets for reproducible builds,
# cross compilation, code quality and release packaging.


.DEFAULT_GOAL := help

# -----------------------------------------------------------------------------
# Configuration
#
# Most variables can be overridden on the command line, e.g.:
#     make FEATURES="foo bar" PROFILE=dev build
#
FEATURES                  ?=
PROFILE                   ?= release
CARGO_INSTALL_EXTRA_FLAGS ?=
CARGO_TARGET_DIR          ?= target
BINARY_NAME               ?= ultramarine
BIN_DIR                   ?= dist/bin
PROMETHEUS_CONFIG_DIR     := monitoring
PROMETHEUS_ACTIVE_CONFIG  := $(PROMETHEUS_CONFIG_DIR)/prometheus.yml
PROMETHEUS_HOST_CONFIG    := $(PROMETHEUS_CONFIG_DIR)/prometheus.host.yml
PROMETHEUS_IPC_CONFIG     := $(PROMETHEUS_CONFIG_DIR)/prometheus.ipc.yml

define sync_prometheus_config
	@if [ ! -f $(PROMETHEUS_ACTIVE_CONFIG) ] || ! cmp -s $1 $(PROMETHEUS_ACTIVE_CONFIG); then \
		echo "$(YELLOW)Syncing Prometheus config -> $(PROMETHEUS_ACTIVE_CONFIG) (source: $1)$(NC)"; \
		cp $1 $(PROMETHEUS_ACTIVE_CONFIG); \
	fi
endef

# When targeting Windows we never enable jemalloc by default because the
# allocator does not provide stable binaries on that platform. 
ifeq ($(OS),Windows_NT)
    FEATURES := $(filter-out jemalloc jemalloc-prof,$(FEATURES))
endif

# Colour escape sequences for nicer output.  
RED   := \033[0;31m
GREEN := \033[0;32m
YELLOW:= \033[1;33m
BLUE  := \033[0;34m
NC    := \033[0m

# -----------------------------------------------------------------------------
# Help
#
# The help target prints all available targets with a short description.  It
# scans Makefile comments of the form `target: ## description` and groups them
# under sections prefixed with `##@`.

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[34m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*##/ { printf "  \033[34m%-25s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

# -----------------------------------------------------------------------------
# Build

.PHONY: build
build: ## Build the main binary with the selected profile and features.
	@echo "$(GREEN)Building $(BINARY_NAME) with profile=$(PROFILE) and features=$(FEATURES)$(NC)"
	cargo build --bin $(BINARY_NAME) --features "$(FEATURES)" --profile "$(PROFILE)"

.PHONY: build-debug
build-debug: ## Build the main binary in debug mode.
	@echo "$(GREEN)Building $(BINARY_NAME) in debug mode$(NC)"
	cargo build --bin $(BINARY_NAME) --features "$(FEATURES)"

.PHONY: install
install: ## Build and install the main binary under ~/.cargo/bin.
	@echo "$(GREEN)Installing $(BINARY_NAME)$(NC)"
	cargo install --path bin/$(BINARY_NAME) --bin $(BINARY_NAME) --force --locked \
		--features "$(FEATURES)" \
		--profile "$(PROFILE)" \
		$(CARGO_INSTALL_EXTRA_FLAGS)

# A build target for reproducible builds. Mirroring that design improves
# determinism when distributing binaries.
.PHONY: build-reproducible
build-reproducible: ## Build the binary with reproducible flags (x86_64-unknown-linux-gnu only).
	SOURCE_DATE_EPOCH := $(shell git log -1 --pretty=%ct)
	RUSTFLAGS := --C target-feature=+crt-static -C link-arg=-static-libgcc \
		-C link-arg=-Wl,--build-id=none -C metadata='' \
		--remap-path-prefix $$(pwd)=.
	CARGO_INCREMENTAL := 0
	LC_ALL := C
	TZ := UTC
	@echo "$(GREEN)Building $(BINARY_NAME) reproducibly$(NC)"
	SOURCE_DATE_EPOCH=$${SOURCE_DATE_EPOCH} \
	RUSTFLAGS="${RUSTFLAGS}" \
	CARGO_INCREMENTAL=$${CARGO_INCREMENTAL} \
	LC_ALL=$${LC_ALL} TZ=$${TZ} \
	cargo build --bin $(BINARY_NAME) --features "$(FEATURES)" --profile "release" --locked --target x86_64-unknown-linux-gnu

# Cross compilation using `cross`.  Use: `make build-aarch64-unknown-linux-gnu`.
.PHONY: build-%
build-%: ## Cross compile the binary for the given target triple. Requires `cross` and Docker.
	@echo "$(GREEN)Cross compiling $(BINARY_NAME) for target $*$(NC)"
	RUSTFLAGS="-C link-arg=-lgcc -C link-arg=-static-libgcc" \
		cross build --bin $(BINARY_NAME) --target $* --features "$(FEATURES)" --profile "$(PROFILE)"

# -----------------------------------------------------------------------------
# Development

.PHONY: dev
dev: ## Run the binary in development mode with auto‑reload via cargo‑watch.
	@echo "$(GREEN)Running $(BINARY_NAME) in development mode$(NC)"
	cargo watch -x "run --bin $(BINARY_NAME) --features '$(FEATURES)' -- -vv"

.PHONY: run
run: ## Run the binary with optional ARGS (use: ARGS="--help").
	cargo run --bin $(BINARY_NAME) --features "$(FEATURES)" -- $(ARGS)

.PHONY: test
test: ## Run all tests with captured output.
	@echo "$(GREEN)Running tests$(NC)"
	cargo test --workspace --all-features -- --nocapture

.PHONY: test-doc
test-doc: ## Run documentation tests.
	cargo test --doc --workspace --all-features

# Use cargo‑nextest for faster and more reproducible unit tests. 
.PHONY: test-nextest
test-nextest: ## Run unit tests using cargo‑nextest.
	cargo install cargo-nextest --locked
	cargo nextest run --workspace --all-features

# Benchmarking via cargo bench.
.PHONY: bench
bench: ## Run benchmarks.
	@echo "$(GREEN)Running benchmarks$(NC)"
	cargo bench --workspace

# -----------------------------------------------------------------------------
# Coverage

# The following targets integrate cargo‑llvm‑cov to produce coverage reports.
.PHONY: cov
cov: ## Generate an lcov coverage report (lcov.info) using cargo‑llvm‑cov.
	rm -f lcov.info
	cargo llvm-cov nextest --lcov --output-path lcov.info --workspace --all-features

.PHONY: cov-report-html
cov-report-html: cov ## Generate a HTML coverage report and open it.
	cargo llvm-cov report --html
	@if command -v xdg-open >/dev/null 2>&1; then xdg-open target/llvm-cov/html/index.html; fi

# -----------------------------------------------------------------------------
# Code Quality

.PHONY: fmt
fmt: ## Format code using nightly rustfmt.
	@echo "$(GREEN)Formatting code$(NC)"
	cargo +nightly fmt --all

.PHONY: fmt-check
fmt-check: ## Check code formatting without modifying files.
	@echo "$(YELLOW)Checking code formatting$(NC)"
	cargo +nightly fmt --all -- --check

.PHONY: clippy
clippy: ## Run clippy lints with all targets and features.
	@echo "$(YELLOW)Running clippy$(NC)"
	cargo clippy --workspace --all-targets --features "$(FEATURES)" -- -D warnings

.PHONY: clippy-fix
clippy-fix: ## Run clippy and automatically fix warnings.
	cargo clippy --workspace --all-targets --features "$(FEATURES)" --fix --allow-dirty --allow-staged -- -D warnings

.PHONY: sort
sort: ## Sort dependencies in Cargo.toml files.
	@echo "$(GREEN)Sorting dependencies$(NC)"
	cargo sort --grouped --workspace

.PHONY: sort-check
sort-check: ## Check if dependencies are sorted.
	cargo sort --grouped --workspace --check

# Typos and TOML formatting linting. We wrap them here to ensure these tools
# exist before running.
.PHONY: lint-typos
lint-typos: ensure-typos ## Check for spelling mistakes using typos-cli.
	typos

.PHONY: ensure-typos
ensure-typos:
	@if ! command -v typos >/dev/null 2>&1; then \
		echo "typos not found. Please install it via \`cargo install typos-cli\`"; exit 1; fi

.PHONY: lint-toml
lint-toml: ensure-dprint ## Format all TOML files using dprint.
	dprint fmt

.PHONY: ensure-dprint
ensure-dprint:
	@if ! command -v dprint >/dev/null 2>&1; then \
		echo "dprint not found. Please install it via \`cargo install --locked dprint\`"; exit 1; fi

.PHONY: lint
lint: fmt clippy sort lint-typos lint-toml ## Run all linters.
	@echo "$(GREEN)✓ All lints passed$(NC)"

.PHONY: fix-lint
fix-lint: clippy-fix fmt ## Apply clippy suggestions and format code.
	@echo "$(GREEN)✓ Lints fixed$(NC)"

# -----------------------------------------------------------------------------
# Documentation

.PHONY: doc
doc: ## Generate and open public API documentation.
	@echo "$(GREEN)Generating documentation$(NC)"
	cargo doc --workspace --all-features --no-deps --open

.PHONY: doc-private
doc-private: ## Generate documentation including private items.
	cargo doc --workspace --all-features --no-deps --document-private-items --open

.PHONY: rustdocs
rustdocs: ## Generate exhaustive Rust documentation with additional flags.
	RUSTDOCFLAGS="\
	--cfg docsrs \
	--show-type-layout \
	--generate-link-to-definition \
	--enable-index-page -Zunstable-options -D warnings" \
	cargo +nightly docs --workspace --document-private-items

# Update CLI documentation or book.  If your project has a script under
# docs/cli/update.sh, this target will rebuild your CLI reference.  
.PHONY: update-book-cli
update-book-cli: build-debug ## Update generated CLI or book documentation.
	@echo "Updating CLI documentation..."
	@if [ -f docs/cli/update.sh ]; then \
		./docs/cli/update.sh $(CARGO_TARGET_DIR)/debug/$(BINARY_NAME); \
	fi

# -----------------------------------------------------------------------------
# Dependencies

.PHONY: deps-check
deps-check: ## Check for unused dependencies (requires cargo-udeps and nightly).
	@echo "$(YELLOW)Checking for unused dependencies$(NC)"
	cargo +nightly udeps --workspace --all-targets --all-features

.PHONY: deps-update
deps-update: ## Update dependencies to the latest compatible versions.
	@echo "$(GREEN)Updating dependencies$(NC)"
	cargo update

.PHONY: deps-outdated
deps-outdated: ## Check for outdated dependencies (requires cargo-outdated).
	@echo "$(YELLOW)Checking for outdated dependencies$(NC)"
	cargo outdated -R

.PHONY: audit
audit: ## Run security audit on dependencies.
	@echo "$(YELLOW)Running security audit$(NC)"
	cargo audit

.PHONY: deny
deny: ## Check dependencies against deny rules (requires cargo-deny).
	@echo "$(YELLOW)Checking dependency policies$(NC)"
	cargo deny check

# -----------------------------------------------------------------------------
# CI/CD

.PHONY: ci
ci: fmt-check sort-check clippy test doc ## Run all CI checks locally.
	@echo "$(GREEN)✓ All CI checks passed$(NC)"

.PHONY: pr
pr: lint test audit ## Run all checks before opening a PR.
	@echo "$(GREEN)✓ Ready for PR$(NC)"

.PHONY: pre-commit
pre-commit: fmt sort ## Format and sort before committing.
	@echo "$(GREEN)✓ Ready to commit$(NC)"

# -----------------------------------------------------------------------------
# Maintenance

.PHONY: clean
clean: ## Clean build artifacts.
	@echo "$(RED)Cleaning build artifacts$(NC)"
	cargo clean

.PHONY: clean-all
clean-all: clean ## Remove Cargo.lock and the entire target directory.
	@echo "$(RED)Cleaning all artifacts$(NC)"
	rm -f Cargo.lock
	rm -rf $(CARGO_TARGET_DIR)

.PHONY: tools
tools: ## Install common development tools used by the Makefile.
	@echo "$(GREEN)Installing development tools$(NC)"
	@echo "Installing cargo-watch..."; cargo install cargo-watch --locked
	@echo "Installing cargo-deny...";  cargo install cargo-deny --locked
	@echo "Installing cargo-audit..."; cargo install cargo-audit --locked
	@echo "Installing cargo-outdated..."; cargo install cargo-outdated --locked
	@echo "Installing cargo-sort..."; cargo install cargo-sort --locked
	@echo "Installing cargo-udeps..."; cargo install cargo-udeps --locked
	@echo "Installing cargo-nextest..."; cargo install cargo-nextest --locked
	@echo "Installing cargo-llvm-cov..."; cargo install cargo-llvm-cov --locked
	@echo "Installing typos-cli..."; cargo install typos-cli --locked
	@echo "Installing dprint..."; cargo install dprint --locked
	@echo "$(GREEN)✓ All tools installed$(NC)"

.PHONY: tools-check
tools-check: ## Verify that all development tools are installed.
	@echo "$(YELLOW)Checking development tools...$(NC)"
	@command -v cargo-watch   >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-watch"   || echo "$(RED)✗$(NC) cargo-watch"
	@command -v cargo-deny    >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-deny"    || echo "$(RED)✗$(NC) cargo-deny"
	@command -v cargo-audit   >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-audit"   || echo "$(RED)✗$(NC) cargo-audit"
	@command -v cargo-outdated>/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-outdated"|| echo "$(RED)✗$(NC) cargo-outdated"
	@command -v cargo-sort    >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-sort"    || echo "$(RED)✗$(NC) cargo-sort"
	@command -v cargo-udeps   >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-udeps"   || echo "$(RED)✗$(NC) cargo-udeps"
	@command -v cargo-nextest >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-nextest" || echo "$(RED)✗$(NC) cargo-nextest"
	@command -v cargo-llvm-cov>/dev/null 2>&1 && echo "$(GREEN)✓$(NC) cargo-llvm-cov"|| echo "$(RED)✗$(NC) cargo-llvm-cov"
	@command -v typos         >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) typos"         || echo "$(RED)✗$(NC) typos"
	@command -v dprint        >/dev/null 2>&1 && echo "$(GREEN)✓$(NC) dprint"        || echo "$(RED)✗$(NC) dprint"


.PHONY: ci-tools
ci-tools: ## Install only tools required for CI
	@echo "$(GREEN)Installing CI-specific tools$(NC)"
	@echo "Installing cargo-nextest..."; cargo install cargo-nextest --locked
	@echo "Installing cargo-deny...";  cargo install cargo-deny --locked
	@echo "Installing cargo-llvm-cov..."; cargo install cargo-llvm-cov --locked
	@echo "Installing typos-cli..."; cargo install typos-cli --locked
	@echo "$(GREEN)✓ CI tools installed$(NC)"

.PHONY: ci-lint
ci-lint: ## Run CI linting (without tools that require installation)
	@echo "$(GREEN)Running CI lints$(NC)"
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	typos

# -----------------------------------------------------------------------------
# Docker
GIT_TAG ?= $(shell (git describe --exact-match --tags 2>/dev/null) || echo sha-$$(git rev-parse --short=12 HEAD))

.PHONY: docker-build
docker-build: ## Build a Docker image for the current binary.
	@echo "$(GREEN)Building Docker image$(NC)"
	docker build -t $(BINARY_NAME):latest .

.PHONY: docker-run
docker-run: ## Run the Docker container.
	docker run -it --rm $(BINARY_NAME):latest

# Cross‑platform Docker builds akin to docker_build_push.  These
# targets build static binaries for amd64 and arm64, assemble them into a
# dist/bin directory, and invoke `docker buildx` to build and push a
# multi‑platform image【282672573575147†L255-L269】.  Adjust DOCKER_IMAGE_NAME
# as appropriate for your organisation.
DOCKER_IMAGE_NAME ?= ghcr.io/yourorg/$(BINARY_NAME)

# Internal helper to assemble release binaries and build/push images.
define docker_build_push
	$(MAKE) build-x86_64-unknown-linux-gnu
	mkdir -p $(BIN_DIR)/amd64
	cp $(CARGO_TARGET_DIR)/x86_64-unknown-linux-gnu/$(PROFILE)/$(BINARY_NAME) $(BIN_DIR)/amd64/$(BINARY_NAME)

	$(MAKE) build-aarch64-unknown-linux-gnu
	mkdir -p $(BIN_DIR)/arm64
	cp $(CARGO_TARGET_DIR)/aarch64-unknown-linux-gnu/$(PROFILE)/$(BINARY_NAME) $(BIN_DIR)/arm64/$(BINARY_NAME)

	docker buildx build --file ./Dockerfile.cross . \
		--platform linux/amd64,linux/arm64 \
		--tag $(DOCKER_IMAGE_NAME):$(1) \
		--tag $(DOCKER_IMAGE_NAME):$(2) \
		--provenance=false \
		--push
endef

.PHONY: docker-build-push
docker-build-push: ## Build and push a cross‑arch Docker image tagged with the latest git tag.
	$(call docker_build_push,$(GIT_TAG),$(GIT_TAG))

.PHONY: docker-build-push-latest
docker-build-push-latest: ## Build and push a cross‑arch Docker image tagged with `latest`.
	$(call docker_build_push,$(GIT_TAG),latest)

# -----------------------------------------------------------------------------
# Release

# Dry run releasing each crate.  The blueprint already defined release targets for
# multiple packages (ultramarine-types, ultramarine-common, ultramarine-node).
# We retain those here.
.PHONY: release-dry
release-dry: ## Perform a dry run of publishing crates.
	@echo "$(YELLOW)Dry run: checking release readiness$(NC)"
	cargo publish --dry-run -p ultramarine-types
	cargo publish --dry-run -p ultramarine-common
	cargo publish --dry-run -p ultramarine-node

.PHONY: release-patch
release-patch: ## Release a patch version (x.x.PATCH).
	@echo "$(GREEN)Releasing patch version$(NC)"
	cargo release patch --execute

.PHONY: release-minor
release-minor: ## Release a minor version (x.MINOR.x).
	@echo "$(GREEN)Releasing minor version$(NC)"
	cargo release minor --execute

# -----------------------------------------------------------------------------
# Shortcuts

.PHONY: b
b: build ## Shortcut for build.

.PHONY: t
t: test ## Shortcut for test.

.PHONY: r
r: run ## Shortcut for run.

.PHONY: c
c: clean ## Shortcut for clean.

.PHONY: l
l: lint ## Shortcut for lint.

# Silence the help output itself.
.SILENT: help

# -----------------------------------------------------------------------------
# Local testnet helpers (parity with malaketh-layered)

.PHONY: all
all: ## Build, generate genesis, start EL stack, wire peers, generate testnet, spawn nodes.
	$(call sync_prometheus_config,$(PROMETHEUS_HOST_CONFIG))
	$(MAKE) build-debug
	cargo run --bin ultramarine-utils -- genesis
	@# Ensure JWT secret exists for Engine API auth; create a secure 32‑byte hex if missing
	@if [ ! -f ./assets/jwtsecret ]; then \
		mkdir -p ./assets; \
		if command -v openssl >/dev/null 2>&1; then \
			echo "Generating JWT secret (assets/jwtsecret)"; \
			umask 077 && openssl rand -hex 32 > ./assets/jwtsecret; \
		else \
			echo "OpenSSL not found; generating JWT secret via dd/hexdump"; \
			umask 077 && dd if=/dev/urandom bs=32 count=1 2>/dev/null | hexdump -v -e '/1 "%%02x"' > ./assets/jwtsecret; \
		fi; \
	fi
	LOCAL_UID=$(shell id -u) LOCAL_GID=$(shell id -g) docker compose up -d
	./scripts/add_peers.sh
	# Generate distributed testnet configs suitable for Docker networking (service DNS names).
	
	cargo run --bin ultramarine -- testnet --nodes 3 --home nodes
	@echo "👉 Grafana dashboard is expected at http://localhost:3000"
	bash scripts/spawn.bash --nodes 3 --home nodes --app ultramarine --ignore-propose-timeout --no-build

.PHONY: all-http
all-http: all ## Alias of `all` for HTTP-based Engine/Eth endpoints.

.PHONY: all-ipc
all-ipc: ## Build, genesis, start EL stack with IPC, generate testnet, spawn nodes with Engine IPC.
	$(call sync_prometheus_config,$(PROMETHEUS_IPC_CONFIG))
	$(MAKE) build-debug
	cargo run --bin ultramarine-utils -- genesis
	@if [ ! -f ./assets/jwtsecret ]; then \
		mkdir -p ./assets; \
		if command -v openssl >/dev/null 2>&1; then \
			echo "Generating JWT secret (assets/jwtsecret)"; \
			umask 077 && openssl rand -hex 32 > ./assets/jwtsecret; \
		else \
			echo "OpenSSL not found; generating JWT secret via dd/hexdump"; \
			umask 077 && dd if=/dev/urandom bs=32 count=1 2>/dev/null | hexdump -v -e '/1 "%%02x"' > ./assets/jwtsecret; \
		fi; \
	fi
	# Generate distributed CL configs for Docker networking (service DNS names) over TCP.
	cargo run --bin ultramarine -- distributed-testnet --nodes 3 --home nodes --machines 10.250.42.10,10.250.42.11,10.250.42.12 --transport tcp
	# Build ultramarine image once and bring up EL (reth) and CL via Compose.
	docker build -t ultramarine:local .
	LOCAL_UID=$(shell id -u) LOCAL_GID=$(shell id -g) docker compose -f compose.ipc.yaml up -d
	./scripts/add_peers.sh
	@echo "👉 Grafana dashboard is expected at http://localhost:3000"
	@echo "Ultramarine containers (ultramarine0/1/2) are started via Compose and use Engine IPC via Docker volumes."
	@echo "Logs: docker logs -f ultramarine0|1|2"
	# Note: In IPC mode, JWT is not required by Ultramarine (Engine IPC doesn’t use it).
	# Eth JSON-RPC remains HTTP; Engine IPC is used only by Ultramarine.

.PHONY: wait-ipc
wait-ipc: ## Wait for Engine IPC sockets (./ipc/{0,1,2}/engine.ipc) to appear.
	@echo "Waiting for Engine IPC sockets (timeout 60s per node)..."
	@for i in 0 1 2; do \
		f=./ipc/$$i/engine.ipc; d=$$(dirname $$f); mkdir -p "$$d"; \
		for t in $$(seq 1 60); do \
			if [ -S "$$f" ]; then echo "✓ $$f"; break; fi; \
			sleep 1; \
			if [ "$$t" -eq 60 ]; then echo "Timed out waiting for $$f"; exit 1; fi; \
		done; \
	done

# -----------------------------------------------------------------------------
# JWT Secret helpers

.PHONY: jwt
jwt: ## Generate a 32‑byte JWT secret at assets/jwtsecret (does not overwrite)
	@mkdir -p ./assets
	@if [ -f ./assets/jwtsecret ]; then \
		echo "assets/jwtsecret already exists; use 'make jwt-force' to overwrite."; \
	else \
		if command -v openssl >/dev/null 2>&1; then \
			echo "Generating JWT secret (assets/jwtsecret)"; \
			umask 077 && openssl rand -hex 32 > ./assets/jwtsecret; \
		else \
			echo "OpenSSL not found; generating JWT secret via dd/hexdump"; \
			umask 077 && dd if=/dev/urandom bs=32 count=1 2>/dev/null | hexdump -v -e '/1 "%%02x"' > ./assets/jwtsecret; \
		fi; \
		echo "✓ Wrote ./assets/jwtsecret"; \
	fi

.PHONY: jwt-force
jwt-force: ## Force‑regenerate JWT secret at assets/jwtsecret (overwrites)
	@mkdir -p ./assets
	@if command -v openssl >/dev/null 2>&1; then \
		echo "Generating JWT secret (assets/jwtsecret)"; \
		umask 077 && openssl rand -hex 32 > ./assets/jwtsecret; \
	else \
		echo "OpenSSL not found; generating JWT secret via dd/hexdump"; \
		umask 077 && dd if=/dev/urandom bs=32 count=1 2>/dev/null | hexdump -v -e '/1 "%02x"' > ./assets/jwtsecret; \
	fi
	@echo "✓ Wrote ./assets/jwtsecret"

.PHONY: stop
stop: ## Stop the docker-compose stack.
	docker compose down

.PHONY: stop-ipc
stop-ipc: ## Stop the docker-compose stack for IPC.
	docker compose -f compose.ipc.yaml down -v

.PHONY: clean-net
clean-net: stop ## Clean local testnet data (genesis, nodes, EL data, monitoring data).
	rm -rf ./assets/genesis.json
	rm -rf ./nodes
	rm -rf ./rethdata
	rm -rf ./monitoring/data-grafana
	rm -rf ./monitoring/data-prometheus
	rm -rf ./ipc

.PHONY: clean-net-ipc
clean-net-ipc: stop-ipc ## Clean local testnet data for IPC.
	rm -rf ./assets/genesis.json
	rm -rf ./nodes
	rm -rf ./rethdata
	rm -rf ./monitoring/data-grafana
	rm -rf ./monitoring/data-prometheus
	rm -rf ./ipc

.PHONY: spam
spam: ## Spam the EL with transactions (60s @ 500 tps against default RPC).
	cargo run --bin ultramarine-utils -- spam --time=60 --rate=500 --rpc-url=http://127.0.0.1:8545

.PHONY: spam-blobs
spam-blobs: ## Spam the EL with EIP-4844 blob transactions (60s @ 50 tps, 128 blobs per tx).
	cargo run --bin ultramarine-utils -- spam --time=60 --rate=50 --rpc-url=http://127.0.0.1:8545 --blobs --blobs-per-tx=6

TIER0_TESTS := \
	blob_roundtrip \
	blob_restream \
	blob_restream_multi_round \
	blob_restart_multi_height \
	blob_restart_multi_height_sync \
	blob_blobless_sequence \
	blob_new_node_sync \
	blob_sync_failure \
	blob_sync_commitment_mismatch \
	blob_decided_el_rejection \
	blob_pruning \
	restart_hydrate \
	sync_package_roundtrip

.PHONY: itest
itest: ## Run Tier 0 integration tests (verbose).
	@echo "🧪 Running Tier 0 blob-state tests..."
	@set -e; for test in $(TIER0_TESTS); do \
		echo "→ $$test"; \
		cargo test -p ultramarine-test --test $$test -- --nocapture; \
	done

.PHONY: itest-quick
itest-quick: ## Run Tier 0 integration tests (quiet output).
	@set -e; for test in $(TIER0_TESTS); do \
		cargo test -p ultramarine-test --test $$test; \
	done

.PHONY: itest-list
itest-list: ## List Tier 0 integration tests.
	@set -e; for test in $(TIER0_TESTS); do \
		cargo test -p ultramarine-test --test $$test -- --list; \
	done

.PHONY: itest-node
itest-node: ## Run full-node (Tier 1) integration tests (process-isolated for determinism).
	@echo "$(GREEN)Running Tier 1 full-node integration tests (14 tests, process-isolated)...$(NC)"
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_blob_quorum_roundtrip -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_validator_restart_recovers -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_restart_mid_height -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_new_node_sync -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_multi_height_valuesync_restart -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_restart_multi_height_rebuilds -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_restream_multiple_rounds_cleanup -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_commitment_mismatch -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_restream_multi_validator -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_inclusion_proof_failure -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_blob_blobless_sequence_behaves -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_blob_pruning_retains_recent_heights -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_sync_package_roundtrip -- --ignored
	@CARGO_NET_OFFLINE=true cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_proof_failure -- --ignored
	@echo "$(GREEN)✅ All 14 Tier 1 tests passed!$(NC)"
