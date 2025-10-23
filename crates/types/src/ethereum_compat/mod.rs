//! Ethereum Deneb Specification Compatibility Layer
//!
//! This module provides types and utilities for Ethereum EIP-4844 blob compatibility.
//! These types mirror the Ethereum consensus spec (Deneb fork) to enable:
//! - Blob data exchange with Ethereum-based tools and clients
//! - Verification by Ethereum light clients and block explorers
//! - RPC compatibility with Ethereum blob APIs
//! - Cross-client testing against Lighthouse, Prysm, etc.
//!
//! ## Architecture Note
//!
//! Similar to bridge architectures in other ecosystems:
//! - **Cosmos**: IBC bridge modules (LCP/TOKI) translate between Tendermint and Ethereum
//! - **Polkadot**: Bridge pallets (Snowbridge) handle Ethereum compatibility
//! - **Bitcoin**: WBTC contract wraps BTC as ERC-20 without changing Bitcoin
//!
//! This compatibility layer is **SEPARATE** from Ultramarine's core consensus types
//! (Value, ValueMetadata, ProposalPart). It exists purely to "speak Deneb" when
//! gossiping and verifying blobs.
//!
//! ### Encapsulation Guarantee
//!
//! Types in this module:
//! - ✅ Are used ONLY within BlobSidecar and blob verification paths
//! - ✅ Never leak into core consensus (Value, Malachite, etc.)
//! - ✅ Can be feature-gated or swapped without affecting consensus
//! - ✅ Follow Ethereum naming for spec compatibility
//!
//! ## Phase 4: Full Ethereum-Compatible BlobSidecar
//!
//! References:
//! - Ethereum Consensus Specs: `consensus-specs/specs/deneb/`
//! - Lighthouse: `lighthouse/consensus/types/src/`
//! - EIP-4844: https://eips.ethereum.org/EIPS/eip-4844

pub mod beacon_block_body;
pub mod beacon_header;
pub mod merkle;

// Re-export key types for internal use
pub use beacon_block_body::BeaconBlockBodyMinimal;
pub use beacon_header::{BeaconBlockHeader, SignedBeaconBlockHeader};
pub use merkle::{generate_kzg_commitment_inclusion_proof, verify_kzg_commitment_inclusion_proof};
