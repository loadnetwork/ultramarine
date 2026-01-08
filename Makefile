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
# LOAD_RETH_IMAGE          ?= docker.io/loadnetwork/load-reth:v0.1.2
LOAD_RETH_IMAGE := load-reth:local
EL_DATADIRS              := rethdata/0 rethdata/1 rethdata/2
EL_IPC_SOCK_DIRS         := ipc/0 ipc/1 ipc/2
export LOAD_RETH_IMAGE

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

.PHONY: audit audit-online
AUDIT_DB_DIR ?= target/advisory-db

audit: ## Run security audit on dependencies.
	@echo "$(YELLOW)Running security audit$(NC)"
	@if [ -n "$$CI" ]; then \
		cargo audit --db $(AUDIT_DB_DIR); \
	elif [ -d "$(AUDIT_DB_DIR)/.git" ]; then \
		cargo audit --db $(AUDIT_DB_DIR) --offline; \
	else \
		echo "$(YELLOW)Skipping cargo audit (no local advisory DB at $(AUDIT_DB_DIR)). Run 'make audit-online' to fetch it.$(NC)"; \
	fi

audit-online: ## Fetch advisory DB and run cargo-audit (requires network).
	@mkdir -p $(AUDIT_DB_DIR)
	cargo audit --db $(AUDIT_DB_DIR)

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
	$(MAKE) reset-el-state
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
	$(MAKE) reset-el-state
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
	# Build ultramarine image once and bring up EL (load-reth) and CL via Compose.
	docker build -t ultramarine:local .
	LOCAL_UID=$(shell id -u) LOCAL_GID=$(shell id -g) docker compose -f compose.ipc.yaml up -d
	./scripts/add_peers.sh
	@echo "👉 Grafana dashboard is expected at http://localhost:3000"
	@echo "Ultramarine containers (ultramarine0/1/2) are started via Compose and use Engine IPC via Docker volumes."
	@echo "Logs: docker logs -f ultramarine0|1|2"
	# Note: In IPC mode, JWT is not required by Ultramarine (Engine IPC doesn’t use it).
	# Eth JSON-RPC remains HTTP; Engine IPC is used only by Ultramarine.

.PHONY: reset-el-state
reset-el-state: ## Wipe and recreate EL datadirs/socket mounts to match the latest genesis.
	@echo "$(YELLOW)Resetting load-reth data directories to match assets/genesis.json$(NC)"
	rm -rf ./rethdata ./ipc
	mkdir -p $(addprefix ./,$(EL_DATADIRS)) $(addprefix ./,$(EL_IPC_SOCK_DIRS))

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
clean-net: stop ## Clean local testnet data. Requires CONFIRM=YES.
	@if [ "$(CONFIRM)" != "YES" ]; then \
		echo "$(RED)WARNING: This will delete all local testnet data!$(NC)"; \
		echo "  - ./assets/genesis.json"; \
		echo "  - ./nodes/"; \
		echo "  - ./rethdata/"; \
		echo "  - ./monitoring/data-*"; \
		echo "  - ./ipc/"; \
		echo ""; \
		echo "To proceed, run: $(YELLOW)make clean-net CONFIRM=YES$(NC)"; \
		exit 1; \
	fi
	rm -rf ./assets/genesis.json
	rm -rf ./nodes
	rm -rf ./rethdata
	rm -rf ./monitoring/data-grafana
	rm -rf ./monitoring/data-prometheus
	rm -rf ./ipc

.PHONY: clean-net-ipc
clean-net-ipc: stop-ipc ## Clean local testnet data (IPC). Requires CONFIRM=YES.
	@if [ "$(CONFIRM)" != "YES" ]; then \
		echo "$(RED)WARNING: This will delete all local testnet data!$(NC)"; \
		echo "  - ./assets/genesis.json"; \
		echo "  - ./nodes/"; \
		echo "  - ./rethdata/"; \
		echo "  - ./monitoring/data-*"; \
		echo "  - ./ipc/"; \
		echo ""; \
		echo "To proceed, run: $(YELLOW)make clean-net-ipc CONFIRM=YES$(NC)"; \
		exit 1; \
	fi
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
spam-blobs: ## Spam all three EL nodes with blob transactions (60s @ 50 tps per EL, 6 blobs per tx).
	@echo "⚙️  Building ultramarine-utils spammer binary..."
	@cargo build --quiet --bin ultramarine-utils
	@echo "🚀 Spamming blob txs against load-reth RPCs"
	@set -e; \
	  SPAM_TIME="$${SPAM_TIME:-60}"; \
	  SPAM_RATE="$${SPAM_RATE:-50}"; \
	  SPAM_BLOBS_PER_TX="$${SPAM_BLOBS_PER_TX:-6}"; \
	  SPAM_RPC_URLS="$${SPAM_RPC_URLS:-http://127.0.0.1:8545 http://127.0.0.1:18545 http://127.0.0.1:28545}"; \
	  if [ -n "$${SPAM_PRIVATE_KEY:-}" ]; then \
	    set -- $$SPAM_RPC_URLS; \
	    if [ "$$#" -ne 1 ]; then \
	      echo "error: SPAM_PRIVATE_KEY requires exactly one RPC URL (set SPAM_RPC_URLS to a single URL)" >&2; \
	      exit 2; \
	    fi; \
	  fi; \
	  i=0; \
	  for url in $$SPAM_RPC_URLS; do \
	    echo "→ blasting $$url"; \
	    if [ -n "$${SPAM_PRIVATE_KEY:-}" ]; then \
	      target/debug/ultramarine-utils spam \
	        --time=$$SPAM_TIME \
	        --rate=$$SPAM_RATE \
	        --rpc-url=$$url \
	        --blobs \
	        --blobs-per-tx=$$SPAM_BLOBS_PER_TX \
	        --private-key=$${SPAM_PRIVATE_KEY} \
	        & \
	    else \
	      target/debug/ultramarine-utils spam \
	        --time=$$SPAM_TIME \
	        --rate=$$SPAM_RATE \
	        --rpc-url=$$url \
	        --blobs \
	        --blobs-per-tx=$$SPAM_BLOBS_PER_TX \
	        --signer-index=$$i \
	        & \
	    fi; \
	    i=$$((i+1)); \
	  done; \
	  wait

# Test Architecture:
# - Tier0 (3 tests, ~8-10s): Component tests in consensus crate
#   * blob_roundtrip: Happy path baseline
#   * blob_sync_commitment_mismatch: Negative path validation
#   * blob_pruning: Retention logic
# - Tier1 (14 tests, via itest-node): Full integration with networking/WAL/libp2p
# To run all tests: make itest && make itest-node

# Allow overriding offline mode (CI can set CARGO_NET_OFFLINE=false on cold caches).
CARGO_NET_OFFLINE ?= true

TIER0_TESTS := \
	blob_roundtrip \
	blob_sync_commitment_mismatch \
	blob_pruning

.PHONY: itest
itest: ## Run Tier 0 component tests (3 fast smoke checks, ~8-10s total).
	@echo "🧪 Running Tier 0 component smoke tests (3 tests)..."
	@set -e; for test in $(TIER0_TESTS); do \
		echo "→ $$test"; \
		cargo test -p ultramarine-consensus --test $$test -- --nocapture; \
	done

.PHONY: itest-quick
itest-quick: ## Run Tier 0 component tests (quiet output).
	@set -e; for test in $(TIER0_TESTS); do \
		cargo test -p ultramarine-consensus --test $$test; \
	done

.PHONY: itest-list
itest-list: ## List Tier 0 component tests.
	@set -e; for test in $(TIER0_TESTS); do \
		cargo test -p ultramarine-consensus --test $$test -- --list; \
	done

.PHONY: itest-node
itest-node: ## Run full-node (Tier 1) integration tests (process-isolated for determinism).
	@echo "$(GREEN)Running Tier 1 full-node integration tests (14 tests, process-isolated)...$(NC)"
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_blob_quorum_roundtrip -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_validator_restart_recovers -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_restart_mid_height -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_new_node_sync -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_multi_height_valuesync_restart -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_restart_multi_height_rebuilds -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_restream_multiple_rounds_cleanup -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_commitment_mismatch -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_restream_multi_validator -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_inclusion_proof_failure -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_blob_blobless_sequence_behaves -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_store_pruning_retains_recent_heights -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_sync_package_roundtrip -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_value_sync_proof_failure -- --ignored
	@echo "$(GREEN)✅ All 14 Tier 1 tests passed!$(NC)"

.PHONY: itest-node-archiver
itest-node-archiver: ## Run Tier 1 archiver/prune full-node integration tests.
	@echo "$(GREEN)Running Tier 1 archiver/prune integration tests (5 tests, process-isolated)...$(NC)"
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_archiver_mock_provider_smoke -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_followers_prune_after_proposer_notices -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_archiver_provider_failure_retries -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_archiver_recover_pending_jobs_api -- --ignored
	@CARGO_NET_OFFLINE=$(CARGO_NET_OFFLINE) cargo test -p ultramarine-test --test full_node node_harness::full_node_archiver_auth_token_transmitted -- --ignored
	@echo "$(GREEN)✅ All 5 Tier 1 archiver/prune tests passed!$(NC)"

# -----------------------------------------------------------------------------
# Infra (multi-host) helpers

NET_DEFAULT_FILE ?= infra/.net
NET ?= $(shell test -f "$(NET_DEFAULT_FILE)" && cat "$(NET_DEFAULT_FILE)")
SECRETS_FILE ?=
LINES ?= 200
LIMIT ?=
SSH_KEY ?=
NET_DIR ?= $(abspath infra/networks/$(NET))
NET_CONFIG ?= $(NET_DIR)/net.mk
ANSIBLE_CONFIG_PATH ?= infra/ansible/ansible.cfg
ANSIBLE_INVENTORY ?= $(NET_DIR)/inventory.yml
ANSIBLE_PLAYBOOKS ?= infra/ansible/playbooks
RESTART_ON_DEPLOY ?= false
APPLY_FIREWALL ?= false
PROMETHEUS_BIND ?= 127.0.0.1
PROMETHEUS_PORT ?= 9090
PROMETHEUS_SCRAPE_INTERVAL ?= 5s
GRAFANA_BIND ?= 127.0.0.1
GRAFANA_PORT ?= 3000
GRAFANA_ADMIN_PASSWORD ?=
EL_HTTP_BIND ?= 0.0.0.0
ROLL_CONFIRM ?=
STORAGE_WIPE ?= false
APT_DISABLE_PROXY ?= false
DATA_MOUNTPOINT ?= /var/lib/loadnet
DATA_SOURCE_MOUNTPOINT ?= /home
DATA_SOURCE_DIR ?= $(DATA_SOURCE_MOUNTPOINT)/loadnet
DATA_DEVICES ?=
DATA_RAID_LEVEL ?= 1
MOVE_DOCKER_DATAROOT ?= false
DOCKER_DATAROOT ?= $(DATA_MOUNTPOINT)/docker
BIND_VAR_LOG ?= false
LOG_DIR ?= $(DATA_MOUNTPOINT)/log
JOURNAL_VACUUM_SIZE ?= 1G
SOPS_AGE_KEY_FILE ?= $(HOME)/.config/sops/age/keys.txt
SOPS_AGE_RECIPIENT ?=
SECRETS_PLAINTEXT ?= $(NET_DIR)/secrets.yaml
SECRETS_ENCRYPTED ?= $(NET_DIR)/secrets.sops.yaml
REMOVE_PLAINTEXT ?= false
WIPE_CONFIRM ?=
WIPE_STATE ?= true
WIPE_MONITORING ?= true
WIPE_CONTAINERS ?= true
WIPE_FIREWALL ?= false
WIPE_NODES ?=
EXTRA_VARS ?=

-include $(NET_CONFIG)

ifeq ($(strip $(SECRETS_FILE)),)
ifeq ($(origin SECRETS_FILE),file)
ifneq ("$(wildcard $(SECRETS_ENCRYPTED))","")
SECRETS_FILE := $(SECRETS_ENCRYPTED)
endif
endif
endif

.PHONY: require-net
require-net:
	@if [ -z "$(NET)" ]; then \
		echo "NET is required (e.g. NET=fibernet or run 'make net-use NET=fibernet')."; \
		exit 1; \
	fi

.PHONY: net-use
net-use: ## Set the default network for infra targets (writes infra/.net).
	@if [ -z "$(NET)" ]; then \
		echo "NET is required (e.g. NET=fibernet)."; \
		exit 1; \
	fi
	@printf "%s\n" "$(NET)" > "$(NET_DEFAULT_FILE)"
	@echo "$(GREEN)Default net set to $(NET) ($(NET_DEFAULT_FILE))$(NC)"

.PHONY: net-unset
net-unset: ## Clear the default network selection (removes infra/.net).
	@rm -f "$(NET_DEFAULT_FILE)"
	@echo "$(YELLOW)Default net cleared ($(NET_DEFAULT_FILE) removed)$(NC)"

.PHONY: net-show
net-show: ## Show the active infra defaults (NET, NET_DIR, SECRETS_FILE, NET_CONFIG).
	@echo "NET=$(if $(NET),$(NET),<unset>)"
	@echo "NET_DIR=$(NET_DIR)"
	@echo "NET_CONFIG=$(NET_CONFIG) $$(test -f "$(NET_CONFIG)" && echo '[loaded]' || echo '[missing]')"
	@echo "SECRETS_FILE=$(if $(SECRETS_FILE),$(SECRETS_FILE),<unset>)"

.PHONY: net-validate
net-validate: require-net ## Validate infra manifest (NET=<net>).
	cargo run --quiet -p ultramarine-netgen --bin netgen -- validate --manifest infra/manifests/$(NET).yaml

.PHONY: net-plan
net-plan: require-net ## Generate infra lockfile + inventory without secrets (dry-run for bootstrap) (NET=<net>).
	cargo run --quiet -p ultramarine-netgen --bin netgen -- gen --manifest infra/manifests/$(NET).yaml --out-dir infra/networks/$(NET) --allow-missing-archiver-tokens

.PHONY: net-gen
net-gen: require-net ## Generate infra lockfile + bundle (NET=<net>, optional SECRETS_FILE=<path>).
	@if [ -n "$(SECRETS_FILE)" ]; then \
		SOPS_AGE_KEY_FILE="$(SOPS_AGE_KEY_FILE)" cargo run --quiet -p ultramarine-netgen --bin netgen -- gen --manifest infra/manifests/$(NET).yaml --out-dir infra/networks/$(NET) --secrets-file "$(SECRETS_FILE)"; \
	else \
		cargo run --quiet -p ultramarine-netgen --bin netgen -- gen --manifest infra/manifests/$(NET).yaml --out-dir infra/networks/$(NET); \
	fi

.PHONY: net-secrets-encrypt
net-secrets-encrypt: ## Encrypt plaintext secrets.yaml -> secrets.sops.yaml (NET=<net>, SOPS_AGE_RECIPIENT=<age1...>).
	@if [ -z "$(SOPS_AGE_RECIPIENT)" ]; then \
		echo "SOPS_AGE_RECIPIENT is required (age public key)"; \
		exit 1; \
	fi
	@if [ ! -f "$(SECRETS_PLAINTEXT)" ]; then \
		echo "Missing plaintext secrets file: $(SECRETS_PLAINTEXT)"; \
		exit 1; \
	fi
	@if ! command -v sops >/dev/null 2>&1; then \
		echo "sops not found; install it first"; \
		exit 1; \
	fi
	@if [ ! -f "$(SOPS_AGE_KEY_FILE)" ]; then \
		echo "Age key file not found: $(SOPS_AGE_KEY_FILE)"; \
		exit 1; \
	fi
	cp "$(SECRETS_PLAINTEXT)" "$(SECRETS_ENCRYPTED)"
	SOPS_AGE_KEY_FILE="$(SOPS_AGE_KEY_FILE)" sops --encrypt --age "$(SOPS_AGE_RECIPIENT)" -i "$(SECRETS_ENCRYPTED)"
	@if [ "$(REMOVE_PLAINTEXT)" = "true" ]; then \
		rm -f "$(SECRETS_PLAINTEXT)"; \
		echo "Removed plaintext secrets file."; \
	else \
		echo "Plaintext secrets left at $(SECRETS_PLAINTEXT); delete it when done."; \
	fi

.PHONY: net-bootstrap
net-bootstrap: require-net ## Bootstrap (no secrets): plan + storage + doctor (NET=<net>, optional LIMIT=<host_id>).
	@$(MAKE) net-plan NET=$(NET)
	@$(MAKE) net-storage NET=$(NET) LIMIT=$(LIMIT) MOVE_DOCKER_DATAROOT=$(MOVE_DOCKER_DATAROOT)
	@$(MAKE) net-doctor-pre NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)

.PHONY: net-launch
net-launch: require-net ## Go-live from scratch: bootstrap + gen + deploy + health (NET=<net>, SECRETS_FILE=<path>, optional LIMIT=<host_id>).
	@if [ -z "$(SECRETS_FILE)" ]; then \
		echo "SECRETS_FILE is required for net-launch (validators need archiver bearer tokens)."; \
		echo "Use 'make net-bootstrap NET=$(NET)' if you want to prep hosts before secrets exist."; \
		exit 1; \
	fi
	@$(MAKE) net-bootstrap NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT) MOVE_DOCKER_DATAROOT=$(MOVE_DOCKER_DATAROOT)
	@$(MAKE) net-gen NET=$(NET) SECRETS_FILE=$(SECRETS_FILE)
	@$(MAKE) net-deploy NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT) APPLY_FIREWALL=$(APPLY_FIREWALL) RESTART_ON_DEPLOY=true
	@$(MAKE) net-doctor-post NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)
	@$(MAKE) net-health NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)

.PHONY: net-update
net-update: require-net ## One-command update: gen + deploy + roll + health (NET=<net>, SECRETS_FILE=<path>, optional LIMIT=<host_id>).
	@if [ -z "$(SECRETS_FILE)" ]; then \
		echo "SECRETS_FILE is required for net-update (validators need archiver bearer tokens)."; \
		exit 1; \
	fi
	@$(MAKE) net-gen NET=$(NET) SECRETS_FILE=$(SECRETS_FILE)
	@$(MAKE) net-apply NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT) APPLY_FIREWALL=$(APPLY_FIREWALL)
	@$(MAKE) net-roll NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)
	@$(MAKE) net-health NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)

.PHONY: net-update-secrets
net-update-secrets: require-net ## Rotate archiver secrets only (NET=<net>, SECRETS_FILE=<path>). Keys are preserved.
	@if [ -z "$(SECRETS_FILE)" ]; then \
		echo "$(RED)SECRETS_FILE is required$(NC)"; \
		echo "  Example: make net-update-secrets NET=$(NET) SECRETS_FILE=infra/networks/$(NET)/secrets.sops.yaml"; \
		exit 1; \
	fi
	@echo "$(YELLOW)Regenerating network bundle with updated secrets...$(NC)"
	@echo "$(GREEN)Note: Validator keys and P2P keys are preserved (netgen reuses existing keys).$(NC)"
	@$(MAKE) net-gen NET=$(NET) SECRETS_FILE=$(SECRETS_FILE)
	@echo ""
	@echo "$(GREEN)Secrets updated successfully.$(NC)"
	@echo "To apply changes, run: $(YELLOW)make net-deploy NET=$(NET)$(NC)"

.PHONY: net-apply
net-apply: require-net ## Deploy artifacts + units without restarts (NET=<net>, NET_DIR=<dir>).
	@$(MAKE) net-deploy NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT) APPLY_FIREWALL=$(APPLY_FIREWALL) RESTART_ON_DEPLOY=false

.PHONY: net-redeploy
net-redeploy: require-net ## Deploy artifacts + restart services (NET=<net>, NET_DIR=<dir>).
	@$(MAKE) net-deploy NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT) APPLY_FIREWALL=$(APPLY_FIREWALL) RESTART_ON_DEPLOY=true

.PHONY: net-deploy
net-deploy: require-net ## Deploy artifacts + systemd units via Ansible (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/deploy.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) restart_on_deploy=$(RESTART_ON_DEPLOY) apply_firewall=$(APPLY_FIREWALL) loadnet_apt_disable_proxy=$(APT_DISABLE_PROXY) loadnet_prometheus_bind=$(PROMETHEUS_BIND) loadnet_prometheus_port=$(PROMETHEUS_PORT) loadnet_prometheus_scrape_interval=$(PROMETHEUS_SCRAPE_INTERVAL) loadnet_grafana_bind=$(GRAFANA_BIND) loadnet_grafana_port=$(GRAFANA_PORT) loadnet_grafana_admin_password=$(GRAFANA_ADMIN_PASSWORD) loadnet_el_http_bind=$(EL_HTTP_BIND)"

.PHONY: net-roll
net-roll: require-net ## Rolling restart via Ansible (serial=1) (NET=<net>, NET_DIR=<dir>, ROLL_CONFIRM=YES if small validator set).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/roll.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) roll_confirm=$(ROLL_CONFIRM)"

.PHONY: net-up
net-up: require-net ## Start services via systemd (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/up.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-down
net-down: require-net ## Stop services via systemd (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/down.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-status
net-status: require-net ## Show systemd status for services (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/status.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-logs
net-logs: ## Tail systemd logs (NET=<net>, NET_DIR=<dir>, LINES=<n>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/logs.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) lines=$(LINES)"

.PHONY: net-clean-logs
net-clean-logs: require-net ## Vacuum journald + rotate/truncate syslog and restart EL/CL (NET=<net>, NET_DIR=<dir>, optional JOURNAL_VACUUM_SIZE=<size>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/clean_logs.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) loadnet_journal_vacuum_size=$(JOURNAL_VACUUM_SIZE)"

.PHONY: net-health
net-health: require-net ## Health check: services active + Engine IPC socket + height moving (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/health.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-doctor
net-doctor: require-net ## Post-deploy diagnostics (units/current/layout/listeners) (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/doctor.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-doctor-pre
net-doctor-pre: require-net ## Pre-deploy checks (OS/storage/docker) (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/doctor_pre.yml \
		-e "net=$(NET) net_dir=$(NET_DIR)"

.PHONY: net-doctor-post
net-doctor-post: ## Post-deploy checks (units/current/layout/listeners) (NET=<net>, NET_DIR=<dir>).
	@$(MAKE) net-doctor NET=$(NET) NET_DIR=$(NET_DIR) LIMIT=$(LIMIT)
.PHONY: net-firewall
net-firewall: require-net ## Apply host firewall (UFW) rules for required P2P ports (NET=<net>, NET_DIR=<dir>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/firewall.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) loadnet_apt_disable_proxy=$(APT_DISABLE_PROXY) loadnet_prometheus_bind=$(PROMETHEUS_BIND) loadnet_prometheus_port=$(PROMETHEUS_PORT) loadnet_grafana_bind=$(GRAFANA_BIND) loadnet_grafana_port=$(GRAFANA_PORT) loadnet_el_http_bind=$(EL_HTTP_BIND)"

.PHONY: net-storage
net-storage: require-net ## Storage bootstrap (non-destructive by default; set STORAGE_WIPE=true + DATA_DEVICES=/dev/disk/by-id/...) (NET=<net>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/storage.yml \
		-e "loadnet_storage_wipe=$(STORAGE_WIPE) loadnet_data_mountpoint=$(DATA_MOUNTPOINT) loadnet_data_source_mountpoint=$(DATA_SOURCE_MOUNTPOINT) loadnet_data_source_dir=$(DATA_SOURCE_DIR) loadnet_data_devices=$(DATA_DEVICES) loadnet_data_raid_level=$(DATA_RAID_LEVEL) loadnet_move_docker_dataroot=$(MOVE_DOCKER_DATAROOT) loadnet_docker_dataroot=$(DOCKER_DATAROOT) loadnet_apt_disable_proxy=$(APT_DISABLE_PROXY) loadnet_bind_var_log=$(BIND_VAR_LOG) loadnet_log_dir=$(LOG_DIR)"

.PHONY: net-wipe
net-wipe: require-net ## Destroy+clean hosts (destructive): WIPE_CONFIRM=YES (WIPE_STATE=true|false, WIPE_MONITORING=true|false, WIPE_CONTAINERS=true|false, WIPE_FIREWALL=true|false, EXTRA_VARS="k=v ...", LIMIT=<host_id>).
	ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook $(if $(SSH_KEY),--private-key "$(SSH_KEY)",) $(if $(LIMIT),-l $(LIMIT),) -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/wipe.yml \
		-e "net=$(NET) net_dir=$(NET_DIR) wipe_confirm=$(WIPE_CONFIRM) wipe_state=$(WIPE_STATE) wipe_monitoring=$(WIPE_MONITORING) wipe_containers=$(WIPE_CONTAINERS) wipe_firewall=$(WIPE_FIREWALL) wipe_nodes=$(WIPE_NODES)" \
		$(if $(EXTRA_VARS),-e "$(EXTRA_VARS)",)

.PHONY: infra-checks
infra-checks: ## Run infra checks (netgen build + ansible syntax-checks if available).
	cargo fmt --all
	cargo build -q -p ultramarine-netgen
	@if command -v ansible-playbook >/dev/null 2>&1; then \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/deploy.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/roll.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/up.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/down.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/firewall.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/storage.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/wipe.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/health.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/doctor.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/clean_logs.yml -e "net=$(NET) net_dir=$(NET_DIR)" && \
		ANSIBLE_CONFIG=$(ANSIBLE_CONFIG_PATH) ansible-playbook --syntax-check -i $(ANSIBLE_INVENTORY) $(ANSIBLE_PLAYBOOKS)/doctor_pre.yml -e "net=$(NET) net_dir=$(NET_DIR)"; \
	else \
		echo "ansible-playbook not found; skipping syntax-checks (install ansible-core on the controller)"; \
	fi
