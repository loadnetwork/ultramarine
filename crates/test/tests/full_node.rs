//! Tier 1 full-node harness entry point.
//!
//! Keeps the heavy network-based scenarios isolated so developers can run them
//! explicitly with `cargo test -p ultramarine-test --test full_node`.

#[path = "full_node/node_harness.rs"]
mod node_harness;
