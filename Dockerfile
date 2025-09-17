# syntax=docker.io/docker/dockerfile:1.7-labs

# ----------------------------------------------------------------------------
# Ultramarine container
#
# This multi-stage Dockerfile builds the `ultramarine` binary and produces a
# lean runtime image. The build stages use
# `cargo-chef` to cache dependency builds and accept environment variables
# (`BUILD_PROFILE`, `FEATURES`, `RUSTFLAGS`) for flexibility.  The final
# runtime stage uses an Ubuntu base to provide a familiar libc environment.

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /usr/src/ultramarine

# OCI labels identify the source and provide metadata.  Update the
# description as Ultramarine evolves.
LABEL org.opencontainers.image.source="https://github.com/loadnetwork/ultramarine"
LABEL org.opencontainers.image.description="Ultramarine is a Rust implementation of the Load Network consensus client."
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# System dependencies required for building certain crates (e.g. rocksdb)
# Should be kept in sync with `.github/workflows` and `Makefile` tooling.
RUN apt-get update && \
  apt-get -y upgrade && \
  apt-get install -y --no-install-recommends \
    libclang-dev \
    pkg-config \
    protobuf-compiler && \
  rm -rf /var/lib/apt/lists/*

# Planner stage: generate a cargo-chef recipe.  Exclude git metadata and
# previously generated dist artefacts to maximise cache hits.
FROM chef AS planner
COPY --exclude=.git --exclude=dist . .
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: restore cached dependencies and compile ultramarine.
FROM chef AS builder
WORKDIR /usr/src/ultramarine
COPY --from=planner /usr/src/ultramarine/recipe.json recipe.json

# Build profile, release by default.  Set via `--build-arg` at build time.
ARG BUILD_PROFILE=release
ENV BUILD_PROFILE=${BUILD_PROFILE}

# Extra Cargo flags (e.g. `-Zbuild-std` when cross-compiling).
ARG RUSTFLAGS=""
ENV RUSTFLAGS=${RUSTFLAGS}

# Extra Cargo features (e.g. `--features jemalloc`).
ARG FEATURES=""
ENV FEATURES=${FEATURES}

# Ensure a recent Rust toolchain compatible with workspace dependencies (e.g. redb >=3 requires 1.89).
RUN rustup toolchain install 1.89.0 && rustup default 1.89.0

# Cook the dependency graph.  This runs `cargo build` on a synthetic
# workspace defined by the recipe, caching compiled dependencies in
# subsequent builds.
RUN cargo chef cook --profile ${BUILD_PROFILE} --features "${FEATURES}" --recipe-path recipe.json

# Copy the actual source code into the build context and compile the
# `ultramarine` binary.  We exclude `.git` and `dist` to avoid invalidating
# the cargo cache.  The `--locked` flag enforces Cargo.lock usage.
COPY --exclude=.git --exclude=dist . .
RUN cargo build --profile ${BUILD_PROFILE} --features "${FEATURES}" --locked --bin ultramarine

# Copy the resulting binary into a known location within the builder image.
RUN cp target/${BUILD_PROFILE}/ultramarine /usr/src/ultramarine/ultramarine

# Final runtime stage: use an Ubuntu base for predictable glibc support.  A
# scratch or distroless image could be used once ultramarine stabilises and
# static linking is enabled.
FROM ubuntu:24.04 AS runtime
WORKDIR /usr/src/ultramarine

# Copy the built binary from the previous stage.
COPY --from=builder /usr/src/ultramarine/ultramarine /usr/local/bin/ultramarine

# Expose ports commonly used by Ethereum consensus/execution clients.  Adjust
# as the ultramarine API evolves.
EXPOSE 30303 9001

ENTRYPOINT ["/usr/local/bin/ultramarine"]
