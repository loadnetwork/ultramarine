Inspired by: <https://github.com/circlefin/malaketh-layered>

```
# Format code
cargo +nightly fmt --all

# Run lints
RUSTFLAGS="-D warnings" cargo +nightly clippy --workspace --all-features --locked

# Run tests
cargo nextest run --workspace

# Run specific benchmark
cargo bench --bench bench_name

# Build optimized binary
cargo build --release --features "jemalloc asm-keccak"

# Check compilation for all features
cargo check --workspace --all-features

# Check documentation
cargo docs --document-private-items
```

## ⚙️ Development workflow with `make`

The root-level **Makefile** wraps cargo, linting, testing, and release chores behind short, memorable commands.\
It gives every contributor identical tooling, flags problems early, and produces reproducible artefacts that match CI.

### Why use it?

- **One-liners** – build, test, lint, release, Docker, coverage… no long cargo flags.
- **Repro builds** – `make build-reproducible` → deterministic binaries.
- **Cross-compile** – `make build-<triple>` via `cross`.
- **Quality gates** – clippy + rustfmt + typos + TOML formatting in one `make lint`.
- **Fast tests & coverage** – `cargo-nextest` + `cargo-llvm-cov` integrated.
- **CI parity** – `make ci` runs the same steps locally that the pipeline enforces.
- **Docker & release helpers** – multi-arch `docker buildx` and semver publishing baked in.

### Quick-start

```bash
# one-time: install helper tools
make tools

# everyday
make build              # release build
make build-debug        # debug build
make run ARGS="--help"  # run with args
make dev                # auto-reloading dev loop
make lint               # clippy, fmt, typos, dprint
make test               # unit + doc tests
make cov-report-html    # HTML coverage
make build-reproducible # deterministic binary
make build-aarch64-unknown-linux-gnu  # cross-compile
make docker-build       # local image
make release-dry        # cargo publish dry-run
```

### Local Testnet

The Makefile includes targets for running a complete local testnet with either an HTTP or IPC based Engine API.

```bash
# Run a local testnet with Engine API over HTTP
make all

# Run a local testnet with Engine API over IPC (Docker)
make all-ipc

# Stop the testnet (use stop-ipc for the IPC variant)
make stop

# Clean the testnet data (use clean-net-ipc for the IPC variant)
make clean-net
```
For more details on the testnet setup, see `docs/DEV_WORKFLOW.md`.

### Blob Testing (EIP-4844)

Test blob sidecars with full observability:

```bash
# Start testnet
make all

# Run blob spam (60s @ 50 TPS, 6 blobs/tx)
make spam-blobs

# Check blob metrics
curl http://localhost:29000/metrics | grep blob_engine

# View Grafana dashboard at http://localhost:3000
# Look for the "Blob Engine" section with 9 panels
```

Blob metrics track verification, storage, lifecycle (promoted/dropped/pruned), and consensus integration. See `docs/DEV_WORKFLOW.md` for complete blob testing guide.

### Before a PR

```bash
make pr   # lint → tests → audit
```

### Docs

```bash
make doc              # public API docs
make rustdocs         # exhaustive docs (private items, extra flags)
make update-book-cli  # regenerate CLI/book docs (if script exists)
```

### Target categories

| Category     | Examples (short)                    |
| ------------ | ----------------------------------- |
| **Build**    | `build`, `build-%`, `install`       |
| **Dev**      | `dev`, `run`                        |
| **Testing**  | `test`, `test-nextest`, `bench`     |
| **Coverage** | `cov`, `cov-report-html`            |
| **Quality**  | `fmt`, `clippy`, `lint`             |
| **Deps**     | `deps-check`, `audit`               |
| **Docs**     | `doc`, `rustdocs`                   |
| **Docker**   | `docker-build`, `docker-build-push` |
| **Release**  | `release-dry`, `release-patch`      |

Run `make help` anytime to see all targets with a one-line description.

### Prerequisites

- Rust ≥ 1.74 (nightly toolchain recommended for rustfmt + clippy-fix).
- `cross` + Docker for cross targets & multi-arch images.
- GNU Make 4.x or newer.
- Optional helper tools (`cargo-nextest`, `cargo-llvm-cov`, `dprint`, `typos-cli`, …) —install via `make tools`.
