#![forbid(unsafe_code)]
#![deny(trivial_casts, trivial_numeric_casts)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(missing_docs)]

pub mod address;
pub mod archive;
pub mod constants;
pub mod context;
pub mod genesis;
pub mod height;
pub mod proposal;
pub mod proposal_part;
pub mod proto;
pub mod signing;
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
pub mod ethereum_compat;

// Phase 5.1: State synchronization types for EIP-4844 blob support
// Added as part of pre-v0 sync implementation (FINAL_PLAN.md Phase 5.1)
// Enables lagging peers to sync by receiving both execution payloads and blob data
// in Malachite's RawDecidedValue.value_bytes
pub mod sync;

// Phase 4.1: Three-layer architecture - Layer 1 (Pure BFT Consensus)
// Added as part of blob header persistence redesign (PHASE4_PROGRESS.md)
// Pure consensus-layer metadata using Tendermint/Malachite terminology.
// Contains NO Ethereum types for technology neutrality.
pub mod consensus_block_metadata;

// Phase 4.1: Three-layer architecture - Layer 2 (Ethereum Compatibility)
// Added as part of blob header persistence redesign (PHASE4_PROGRESS.md)
// Ethereum EIP-4844 compatibility bridge that converts to BeaconBlockHeader
// only when needed for BlobSidecar construction.
pub mod blob_metadata;

// Phase 6: Archive/Prune configuration
// Added as part of Phase 6 archive/prune (PHASE6_ARCHIVE_PRUNE_FINAL.md)
// Configuration for the background archiver worker
pub mod archiver_config;
