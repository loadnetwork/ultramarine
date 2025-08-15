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
