#![forbid(unsafe_code)]
#![deny(trivial_casts, trivial_numeric_casts)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(missing_docs)]

pub mod address;
pub mod context;
pub mod genesis;
pub mod height;
pub mod proposal;
pub mod proposal_part;
pub mod proto;
pub mod signing;
pub mod sync;
pub mod validator_set;
pub mod value;
pub mod vote;

pub mod aliases;
pub mod codec;
pub mod engine_api;
pub mod utils;

// Phase 1: EIP-4844 blob types
// Added as part of blob sidecar integration (FINAL_PLAN.md Phase 1)
// These types are shared across execution, consensus, and node crates
pub mod blob;

// Phase 2: Lightweight value metadata for consensus voting
// Added as part of blob sidecar integration (FINAL_PLAN.md Phase 2)
// Keeps consensus messages small (~2KB) instead of potentially MBs
pub mod value_metadata;

// Phase 4: Ethereum Deneb specification compatibility layer
// Added as part of blob sidecar Phase 4 (FINAL_PLAN.md Phase 4)
// This module contains Ethereum-specific types (BeaconBlockHeader, merkle proofs)
// that are used ONLY within BlobSidecar for spec compatibility.
// These types DO NOT leak into core Ultramarine consensus (Value, Malachite, etc.)
pub(crate) mod ethereum_compat;
