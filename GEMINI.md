
# Ultramarine — AI Contributor Guide (Rust & Performance Friendly)

This guide is optimized for models (and humans!) contributing to the **Ultramarine** repository. It explains the repo layout, the development workflow, Makefile targets, CI/CD, and preserves **Rust‑specific engineering and performance guidance** so your contributions are correct, fast, and easy to review.

---

## What Ultramarine is (today)

Ultramarine is a **Rust workspace** intended to evolve into a consensus client. The workspace currently contains a CLI binary and four library crates that are ready to be filled with real logic:

```
bin/ultramarine    # binary crate (default member)
crates/consensus   # consensus logic (stub)
crates/execution   # execution layer integration (stub)
crates/node        # orchestration / bootstrapping (stub)
crates/types       # shared types, serialization (stub)
crates/cli         # cli
```

The root `Cargo.toml` unifies metadata (edition 2024, version, license, MSRV 1.88), centralizes lints, and specifies build profiles (e.g., release uses thin-LTO and a single codegen unit for better performance).

---

## Local setup & prerequisites

- **Rust**: 1.88+ (toolchain pinned in CI).
    
- **GNU Make**: 4.x.
    
- **Optional**: Docker and [`cross`](https://github.com/cross-rs/cross) for multi‑arch builds.
    
- **One‑time**: install dev tools the Makefile expects:
    

```bash
make tools
# installs: cargo-watch, cargo-deny, cargo-audit, cargo-outdated, cargo-sort,
# cargo-udeps (nightly), cargo-nextest, cargo-llvm-cov, typos-cli, dprint
```

---

## Everyday development workflow

Use the Makefile to get consistent flags and the same checks CI runs:

```bash
# Build & run
make build                # optimized build
make build-debug          # debug build
make run ARGS="--help"    # pass args to the CLI
make dev                  # autoreload loop via cargo-watch

# Quality gates
make fmt-check            # nightly rustfmt --check
RUSTFLAGS="-D warnings" make clippy
make sort-check           # cargo-sort sanity for Cargo.toml
make lint                 # fmt + clippy + typos + dprint/toml

# Tests & coverage
make test                 # unit + doc tests
make test-nextest         # faster runs using nextest
make cov-report-html      # generate + open HTML coverage

# Docs
make doc                  # public API docs
make rustdocs             # exhaustive docs incl. private items

# Cross & Docker
make build-aarch64-unknown-linux-gnu
make docker-build
make docker-build-push-latest

# Pre-PR / CI parity
make pr                   # lint → tests → audit
make ci                   # mirrors CI checks locally
```

---

## Makefile cheatsheet

|Target|Purpose|
|---|---|
|`build`, `build-debug`, `run`, `dev`|Compile & run workflows (with watch)|
|`fmt`, `fmt-check`, `clippy`, `sort`, `lint`, `fix-lint`|Formatting & linting suite|
|`test`, `test-nextest`, `bench`|Testing & benchmarks|
|`cov`, `cov-report-html`|Coverage (lcov + HTML)|
|`deps-check`, `deps-outdated`, `audit`, `deny`|Dependency hygiene & security|
|`docker-build`, `docker-build-push(-latest)`|Local / multi‑arch images|
|`tools`, `tools-check`|Install/verify helper tools|
|`pr`, `ci`, `pre-commit`|Local equivalents of CI/PR gates|

> Pro tip: run `make help` to see all targets with short descriptions.

---

## CI/CD overview

- **CI** (push/PR): format (nightly rustfmt), `cargo-sort`, clippy (warnings = errors), tests, docs, optional coverage (on `main` or when PR title includes `[coverage]`). Artifacts (docs/coverage) are uploaded.
    
- **Conventional Commits**: PRs must pass commit‑message validation (`feat:`, `fix:`, `refactor:`, `docs:`, etc.).
    
- **Docker**: PRs (non‑draft) do a smoke build; pushes/tags build **multi‑arch images** (amd64/arm64) and can push when configured.
    
- **Release**: pushing a `v*` tag (or dispatching with a `tag` input) creates a GitHub Release with generated notes.
    
- **Dependabot**: weekly updates for Cargo deps and GitHub Actions.
    

Keep PRs **small and focused**. The pipeline is fast when you are.

---

## Rust style & correctness guidelines

- **Edition/MSRV**: target **edition 2024**, MSRV **1.88** (same as CI).
    
- **Formatting**: `cargo +nightly fmt --all`. Use the repo’s `rustfmt.toml` (import granularity, line widths, etc.) to avoid churn.
    
- **Lints**:
    
    - Workspace **Rust lints** include `unused-must-use = "deny"`, `missing-docs = "warn"`, `rust-2018-idioms = "deny"`.
        
    - **Clippy** is part of the gates; aim for zero warnings. Prefer idiomatic refactoring over suppressing lints.
        
- **Docs**: public APIs should have `///` docs; keep examples compile‑checked with doctests where helpful.
    

---

## Performance playbook (preserved & expanded)

**Design for hot paths**

- Avoid heap work in tight loops. Use stack where practical. Pre‑size collections (`with_capacity`) when the bound is known or well‑estimated.
    
- Prefer **borrowing over owning**. Accept `&[u8]`/`&str`/`&T` rather than `Vec<u8>`/`String`/`T` when you don’t need ownership.
    
- Minimize `clone()`/`to_vec()`/`to_string()`. If needed, comment why the copy is required.
    

**Data layout & types**

- Favor **small, copyable types** in hot structs (e.g., `u64`, `u32`, fixed‑size arrays). Keep layout compact; avoid nested `Option<Option<T>>` and large enums with hot/cold variants mixed.
    
- Use `Option<NonZeroU64>` etc. to leverage niche optimizations.
    
- For cheap shared buffers, consider `bytes::Bytes` (cheap clone) or `Arc<[u8]>` when many readers need the same data.
    

**Generics & dispatch**

- Prefer **static dispatch** (generics / trait bounds) for hot paths; use **dyn Trait** only when you need runtime polymorphism and the call isn’t performance critical.
    
- Reserve `#[inline]`/`#[inline(always)]` for **measured** wins; over‑inlining can bloat code and hurt i‑cache.
    

**Branching & error paths**

- Mark unlikely paths with `#[cold]` (and optionally `#[inline(never)]`) on error/slow functions to improve code locality.
    
- Use `Result<T, E>` and early `?` returns for straight‑line happy paths.
    

**Concurrency**

- **CPU‑bound**: use a thread‑pool or crates like `rayon` for data‑parallel transforms; don’t block async executors with heavy compute.
    
- **I/O‑bound**: use `tokio` where you need async networking or disk I/O; keep blocking work behind `spawn_blocking` or dedicated threads.
    
- Consider `parking_lot` mutexes for lower overhead in highly contended sections (measure!).
    

**Collections**

- Use `HashMap/HashSet` judiciously; pre‑reserve when size is known. For small fixed sizes, arrays or `SmallVec` can beat heap‑allocated `Vec`.
    
- For performance‑critical hashing, specialized hashers (e.g., `ahash`) can be faster—balance this against DoS resilience if input is adversarial.
    

**I/O & serialization**

- Batch I/O; avoid per‑item syscalls. Use buffered readers/writers.
    
- Serialize with `serde` (derive early), prefer zero‑copy deserialization when possible.
    

**Logging & metrics**

- Cheap logging in hot paths: guard with level checks or use spans with care. Avoid building large strings eagerly; use structured fields.
    
- Instrument with counters/gauges/timers for feedback loops during performance work.
    

**Build settings**

- **Release** profile is already tuned: `lto = "thin"`, `codegen-units = 1`. For local profiling, try `RUSTFLAGS="-C target-cpu=native"`; don’t hard‑code it in CI (keeps portability).
    
- If you add optional features later (e.g., `jemalloc`, `asm-*`), keep them **opt‑in** and measured.
    

**Benchmarking & profiling**

- Add Criterion benchmarks (`benches/`) for hot code; run via `cargo bench` or `make bench`.
    
- Use `cargo-llvm-cov` for coverage; prefer benchmarking in **release** with stable inputs.
    
- For deep dives: `perf`/`dtrace`/`vtune` (system‑dependent). Consider `cargo-flamegraph` locally (not required by CI).
    

---

## Testing strategy

- **Unit tests** near code (`#[cfg(test)]` modules).
    
- **Integration tests** in a `tests/` directory per crate once public surfaces exist.
    
- **Property tests** (e.g., `proptest`) for critical invariants and encoding/decoding.
    
- **Fuzzing** (later): good for network parsing and state machines (e.g., `cargo fuzz`).
    
- **Benchmarks** (Criterion) for hot algorithms and data transformations.
    

**Example unit test skeleton**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_two_numbers() {
        assert_eq!(add(2, 2), 4);
    }
}
```

---

## Contribution patterns that work well here

- **Small targeted fixes**: prefer surgical PRs over sweeping refactors.
    
- **API surfacing**: start by defining traits/structs in `types` and `consensus`; keep crates decoupled.
    
- **Make code generic** where reuse across modules is expected; document trait bounds and add minimal examples to docs.
    
- **Add tests** alongside new code; include at least one failure case.
    

> If you propose multiple changes, break them into multiple PRs: (1) prep/refactor (no behavior change), (2) feature, (3) follow‑up cleanup.

---

## PR etiquette (agent‑ready)

1. **Branch**: `feat-…` / `fix-…` / `refactor-…`.
    
2. **Diffs**: show minimal unified diffs per file; keep imports tidy and follow formatting.
    
3. **Validation**: include an executable plan:
    
    ```bash
    make fmt-check && make sort-check
    RUSTFLAGS="-D warnings" make clippy
    make test
    ```
    
4. **Performance‑sensitive changes**: explain expected impact; if possible, include a micro‑benchmark + result table or a before/after `perf` snippet.
    
5. **Commit messages**: Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, etc.). The bot will check this.
    

---

---

## Quick reference — commands

```bash
# Formatting & linting
make fmt-check
RUSTFLAGS="-D warnings" make clippy
make sort-check
make lint

# Tests & coverage
make test
make test-nextest
make cov-report-html

# Build & run
make build
make run ARGS="--help"
make dev

# Cross & container
make build-aarch64-unknown-linux-gnu
make docker-build
make docker-build-push-latest

# Dependency & security
make deps-outdated
make deps-check
make audit
make deny

# Docs
make doc
make rustdocs

# Pre-PR
make pr
```

---

