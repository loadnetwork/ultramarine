//! State Synchronization Types for EIP-4844 Blob Support
//!
//! This module provides the data structures needed for state synchronization when
//! blocks contain blob sidecars. It enables lagging peers to catch up by receiving
//! both execution payloads and blob data in sync responses.
//!
//! ## Pre-V0 Design
//!
//! This is a minimal "get it working" implementation that:
//! - Always bundles full data (no pruning yet)
//! - Uses simple two-variant enum: Full vs MetadataOnly
//! - Provides safe fallback if data is missing
//!
//! ## Architecture
//!
//! ```text
//! Malachite: RawDecidedValue { value_bytes: Bytes, certificate }
//!                                     ▲
//!                                     │
//!                    SyncedValuePackage serialized into value_bytes
//!                                     │
//!              ┌──────────────────────┴───────────────────────┐
//!              │                                               │
//!         Full {                                     MetadataOnly {
//!           execution_payload_ssz: Bytes,              value: Value
//!           blob_sidecars: Vec<BlobSidecar>          }
//!         }
//! ```
//!
//! ## Usage
//!
//! **Server Side** (GetDecidedValue):
//! ```rust,ignore
//! let package = SyncedValuePackage::Full {
//!     execution_payload_ssz: payload_bytes,
//!     blob_sidecars: blobs,
//! };
//! let value_bytes = package.encode()?;
//! ```
//!
//! **Client Side** (ProcessSyncedValue):
//! ```rust,ignore
//! let package = SyncedValuePackage::decode(&value_bytes)?;
//! match package {
//!     SyncedValuePackage::Full { execution_payload_ssz, blob_sidecars } => {
//!         // Store and verify
//!     }
//!     SyncedValuePackage::MetadataOnly { value } => {
//!         // Handle metadata-only (fallback)
//!     }
//! }
//! ```
//!
//! ## Future V0
//!
//! The v0 implementation will add:
//! - Archival status tracking
//! - Retention-aware sync
//! - RestreamProposal support
//! - Peer scoring

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{proposal_part::BlobSidecar, value::Value};

/// Synced block data package for state synchronization
///
/// This enum encapsulates all data needed to sync a decided block, including
/// both the execution payload and blob sidecars. It's designed to fit into
/// Malachite's `RawDecidedValue.value_bytes` field.
///
/// ## Pre-V0 Simplification
///
/// In pre-v0, we always expect `Full` variant since pruning is not yet implemented.
/// The `MetadataOnly` variant exists as a safety fallback.
///
/// ## Serialization
///
/// Uses bincode for efficient binary serialization. The encoded bytes are placed
/// directly into `RawDecidedValue.value_bytes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncedValuePackage {
    /// Full block data (execution payload + blobs)
    ///
    /// Used when: Blobs are available locally (not pruned yet)
    ///
    /// Contains everything needed to import the block:
    /// - Value metadata (for consensus voting)
    /// - ExecutionPayload bytes (raw bytes from storage)
    /// - Blob sidecars with KZG proofs
    ///
    /// **Size**: ~131KB per blob + execution payload (~few KB) + metadata (~2KB)
    Full {
        /// Value metadata (for consensus)
        ///
        /// This is the Value that consensus needs to vote on. Including it
        /// in the Full variant simplifies the receiving side - they don't need
        /// to reconstruct it from blob commitments.
        value: Value,

        /// Raw execution payload bytes from storage
        ///
        /// These are the exact bytes stored via `store_undecided_block_data()`,
        /// which can be directly passed to `store_undecided_block_data()` on the
        /// receiving side.
        execution_payload_ssz: Bytes,

        /// Blob sidecars (each ~131KB + proofs)
        ///
        /// These contain:
        /// - Blob data (131,072 bytes)
        /// - KZG commitment (48 bytes)
        /// - KZG proof (48 bytes)
        /// - Blob index
        blob_sidecars: Vec<BlobSidecar>,
    },

    /// Metadata-only (blobs not available)
    ///
    /// Fallback when: Execution payload or blobs are missing.
    ///
    /// In pre-v0 this shouldn't happen (no pruning yet), but provides a
    /// safe degradation path. The syncing peer will receive just the Value
    /// metadata and should skip EL import (assuming EL synced independently).
    ///
    /// **Size**: ~2KB (just Value metadata)
    MetadataOnly {
        /// Just the Value (metadata: header + commitments)
        ///
        /// Contains `ValueMetadata` which includes:
        /// - ExecutionPayloadHeader (lightweight, no transactions)
        /// - KZG commitments (48 bytes each)
        value: Value,
    },
}

impl SyncedValuePackage {
    /// Check if this is a full package with blobs
    ///
    /// # Returns
    ///
    /// - `true` if this is `Full` variant with payload and blobs
    /// - `false` if this is `MetadataOnly` variant
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if package.is_full() {
    ///     // Process full data
    /// } else {
    ///     // Handle metadata-only fallback
    /// }
    /// ```
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    /// Get the execution payload bytes if available
    ///
    /// # Returns
    ///
    /// - `Some(&Bytes)` if this is `Full` variant
    /// - `None` if this is `MetadataOnly` variant
    pub fn execution_payload(&self) -> Option<&Bytes> {
        match self {
            Self::Full { execution_payload_ssz, .. } => Some(execution_payload_ssz),
            Self::MetadataOnly { .. } => None,
        }
    }

    /// Get blob sidecars if available
    ///
    /// # Returns
    ///
    /// - `Some(&[BlobSidecar])` if this is `Full` variant
    /// - `None` if this is `MetadataOnly` variant
    pub fn blob_sidecars(&self) -> Option<&[BlobSidecar]> {
        match self {
            Self::Full { blob_sidecars, .. } => Some(blob_sidecars),
            Self::MetadataOnly { .. } => None,
        }
    }

    /// Get the Value metadata
    ///
    /// # Returns
    ///
    /// - `Some(&Value)` for both `Full` and `MetadataOnly` variants
    ///
    /// **Note**: Always returns the Value, as it's needed for consensus voting.
    pub fn value_metadata(&self) -> &Value {
        match self {
            Self::Full { value, .. } => value,
            Self::MetadataOnly { value } => value,
        }
    }

    /// Encode this package to bytes using bincode
    ///
    /// # Returns
    ///
    /// Bytes suitable for placing in `RawDecidedValue.value_bytes`
    ///
    /// # Errors
    ///
    /// Returns error if bincode serialization fails (rare, usually indicates
    /// a programming error like invalid data).
    pub fn encode(&self) -> Result<Bytes, String> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(|e| format!("Failed to encode SyncedValuePackage: {}", e))
    }

    /// Decode a package from bytes using bincode
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes from `RawDecidedValue.value_bytes`
    ///
    /// # Returns
    ///
    /// Decoded `SyncedValuePackage`
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Bytes are not valid bincode
    /// - Bytes represent a different type
    /// - Data is corrupted
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        bincode::deserialize(bytes)
            .map_err(|e| format!("Failed to decode SyncedValuePackage: {}", e))
    }

    /// Estimate size in bytes of this package
    ///
    /// # Returns
    ///
    /// Approximate size in bytes:
    /// - `Full`: value.size() + execution_payload.len() + (blob_count * 131KB) + overhead
    /// - `MetadataOnly`: ~2KB
    pub fn estimated_size(&self) -> usize {
        match self {
            Self::Full { value, execution_payload_ssz, blob_sidecars } => {
                value.size_bytes()
                    + execution_payload_ssz.len()
                    + blob_sidecars.iter().map(|b| b.size_bytes()).sum::<usize>()
                    + 100 // Overhead for enum tag, lengths, etc.
            }
            Self::MetadataOnly { value } => {
                value.size_bytes() + 50 // Overhead
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{Blob, KzgCommitment, KzgProof, BYTES_PER_BLOB};

    #[test]
    fn test_synced_value_package_full_is_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: Bytes::from(vec![1u8; 1024]),
            blob_sidecars: vec![],
        };

        assert!(package.is_full());
        assert!(package.execution_payload().is_some());
        assert!(package.blob_sidecars().is_some());
    }

    #[test]
    fn test_synced_value_package_metadata_only_is_not_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::MetadataOnly { value: value.clone() };

        assert!(!package.is_full());
        assert!(package.execution_payload().is_none());
        assert!(package.blob_sidecars().is_none());
    }

    #[test]
    fn test_encode_decode_roundtrip_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let payload = Bytes::from(vec![1u8; 1024]);
        let blob = Blob::new(vec![0u8; BYTES_PER_BLOB].into()).unwrap();
        let sidecar = BlobSidecar::new(0, blob, KzgCommitment([2u8; 48]), KzgProof([3u8; 48]));

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload.clone(),
            blob_sidecars: vec![sidecar],
        };

        // Encode
        let encoded = package.encode().expect("Failed to encode");
        assert!(!encoded.is_empty());

        // Decode
        let decoded = SyncedValuePackage::decode(&encoded).expect("Failed to decode");

        // Verify
        assert_eq!(package, decoded);
        assert!(decoded.is_full());
    }

    #[test]
    fn test_encode_decode_roundtrip_metadata_only() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::MetadataOnly { value: value.clone() };

        // Encode
        let encoded = package.encode().expect("Failed to encode");
        assert!(!encoded.is_empty());

        // Decode
        let decoded = SyncedValuePackage::decode(&encoded).expect("Failed to decode");

        // Verify
        assert_eq!(package, decoded);
        assert!(!decoded.is_full());
    }

    #[test]
    fn test_encode_decode_roundtrip_multiple_blobs() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let payload = Bytes::from(vec![1u8; 2048]);
        let mut sidecars = Vec::new();

        // Create 3 test blobs
        for i in 0..3 {
            let blob = Blob::new(vec![i; BYTES_PER_BLOB].into()).unwrap();
            let sidecar =
                BlobSidecar::new(i, blob, KzgCommitment([i + 1; 48]), KzgProof([i + 2; 48]));
            sidecars.push(sidecar);
        }

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload,
            blob_sidecars: sidecars,
        };

        // Roundtrip
        let encoded = package.encode().unwrap();
        let decoded = SyncedValuePackage::decode(&encoded).unwrap();

        assert_eq!(package, decoded);

        // Verify blob count
        let blobs = decoded.blob_sidecars().unwrap();
        assert_eq!(blobs.len(), 3);
        for (i, sidecar) in blobs.iter().enumerate() {
            assert_eq!(sidecar.index, i as u8);
        }
    }

    #[test]
    fn test_estimated_size_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let payload = Bytes::from(vec![1u8; 1024]);
        let blob = Blob::new(vec![0u8; BYTES_PER_BLOB].into()).unwrap();
        let sidecar = BlobSidecar::new(0, blob, KzgCommitment([2u8; 48]), KzgProof([3u8; 48]));

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload,
            blob_sidecars: vec![sidecar],
        };

        let size = package.estimated_size();

        // Should be roughly: value (~2KB) + 1024 (payload) + 131169 (blob sidecar) + overhead
        assert!(size > 132_000);
        assert!(size < 135_000);
    }

    #[test]
    fn test_estimated_size_metadata_only() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::MetadataOnly { value };

        let size = package.estimated_size();

        // Should be very small, just metadata
        assert!(size < 3000); // Less than 3KB
    }

    #[test]
    fn test_decode_invalid_bytes() {
        let invalid_bytes = vec![255u8; 100]; // Random garbage

        let result = SyncedValuePackage::decode(&invalid_bytes);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to decode"));
    }

    #[test]
    fn test_decode_empty_bytes() {
        let empty_bytes: &[u8] = &[];

        let result = SyncedValuePackage::decode(empty_bytes);

        assert!(result.is_err());
    }
}
