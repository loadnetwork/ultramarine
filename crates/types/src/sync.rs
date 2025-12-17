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
//!     value: decided_value.clone(),
//!     execution_payload_ssz: payload_bytes,
//!     blob_sidecars: blobs,
//!     execution_requests: vec![],
//! };
//! let value_bytes = package.encode()?;
//! ```
//!
//! **Client Side** (ProcessSyncedValue):
//! ```rust,ignore
//! let package = SyncedValuePackage::decode(&value_bytes)?;
//! match package {
//!     SyncedValuePackage::Full { execution_payload_ssz, blob_sidecars, execution_requests } => {
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
use malachitebft_proto::{Error as ProtoError, Protobuf};

use crate::{
    aliases::Bytes as AlloyBytes, archive::ArchiveNotice, proposal_part::BlobSidecar, proto,
    value::Value,
};

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
/// Uses Protobuf for network serialization (idiomatic for Malachite-based clients).
/// The encoded bytes are placed into `RawDecidedValue.value_bytes`.
///
/// Protobuf provides built-in schema versioning via:
/// - Field numbers for backward compatibility
/// - Optional fields for forward compatibility
/// - oneof for enum variants
#[derive(Clone, Debug, PartialEq, Eq)]
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

        /// Execution requests (EIP-7685) required for Prague hashing
        ///
        /// Stored as opaque byte arrays with the request type prepended.
        execution_requests: Vec<AlloyBytes>,

        /// Archive notices emitted post-commit for blobs at this height.
        archive_notices: Vec<ArchiveNotice>,
    },

    /// Metadata-only (blobs not available)
    ///
    /// Fallback when: Execution payload or blobs are missing, or blobs have been pruned.
    ///
    /// Used for pruned heights where actual blob data is no longer available locally.
    /// The syncing peer will receive the Value metadata and archive notices containing
    /// locators for fetching blobs from external archives.
    ///
    /// **Size**: ~2KB (Value metadata) + archive notices
    MetadataOnly {
        /// Just the Value (metadata: header + commitments)
        ///
        /// Contains `ValueMetadata` which includes:
        /// - ExecutionPayloadHeader (lightweight, no transactions)
        /// - KZG commitments (48 bytes each)
        value: Value,
        /// Archive notices with locators for pruned blobs
        ///
        /// When blobs have been pruned, these notices provide the storage
        /// locators where the blobs can be fetched from external archives.
        archive_notices: Vec<ArchiveNotice>,
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

    /// Get execution requests if available
    pub fn execution_requests(&self) -> Option<&[AlloyBytes]> {
        match self {
            Self::Full { execution_requests, .. } => Some(execution_requests),
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
            Self::MetadataOnly { value, .. } => value,
        }
    }

    /// Encode this package to bytes using Protobuf
    ///
    /// # Returns
    ///
    /// Bytes suitable for placing in `RawDecidedValue.value_bytes`
    ///
    /// # Errors
    ///
    /// Returns error if protobuf serialization fails
    pub fn encode(&self) -> Result<Bytes, String> {
        Protobuf::to_bytes(self).map_err(|e| format!("Failed to encode SyncedValuePackage: {}", e))
    }

    /// Decode a package from Protobuf bytes
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
    /// - Bytes are not valid protobuf
    /// - Required fields are missing
    /// - Data is corrupted
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        Protobuf::from_bytes(bytes)
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
            Self::Full {
                value,
                execution_payload_ssz,
                blob_sidecars,
                execution_requests,
                archive_notices: _,
            } => {
                value.size_bytes() +
                    execution_payload_ssz.len() +
                    execution_requests.iter().map(|r| r.len()).sum::<usize>() +
                    blob_sidecars.iter().map(|b| b.size_bytes()).sum::<usize>() +
                    100 // Overhead for enum tag, lengths, etc.
            }
            Self::MetadataOnly { value, archive_notices } => {
                value.size_bytes() +
                    archive_notices.len() * 200 + // Approximate size per notice
                    50 // Overhead
            }
        }
    }
}

/// Protobuf conversion for SyncedValuePackage
///
/// This enables network serialization using the standard Malachite codec pattern.
impl Protobuf for SyncedValuePackage {
    type Proto = proto::SyncedValuePackage;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        match proto.package {
            Some(proto::synced_value_package::Package::Full(full)) => {
                let value = full
                    .value
                    .ok_or_else(|| ProtoError::missing_field::<proto::FullPackage>("value"))
                    .and_then(Value::from_proto)?;

                let blob_sidecars = full
                    .blob_sidecars
                    .into_iter()
                    .map(BlobSidecar::from_proto)
                    .collect::<Result<Vec<_>, _>>()?;

                let execution_requests =
                    full.execution_requests.into_iter().map(AlloyBytes::from).collect();

                let archive_notices = full
                    .archive_notices
                    .into_iter()
                    .map(ArchiveNotice::from_proto)
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(SyncedValuePackage::Full {
                    value,
                    execution_payload_ssz: full.execution_payload_ssz,
                    blob_sidecars,
                    execution_requests,
                    archive_notices,
                })
            }
            Some(proto::synced_value_package::Package::MetadataOnly(metadata)) => {
                let value = metadata
                    .value
                    .ok_or_else(|| ProtoError::missing_field::<proto::MetadataOnlyPackage>("value"))
                    .and_then(Value::from_proto)?;

                let archive_notices = metadata
                    .archive_notices
                    .into_iter()
                    .map(ArchiveNotice::from_proto)
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(SyncedValuePackage::MetadataOnly { value, archive_notices })
            }
            None => Err(ProtoError::missing_field::<proto::SyncedValuePackage>("package")),
        }
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let package = match self {
            SyncedValuePackage::Full {
                value,
                execution_payload_ssz,
                blob_sidecars,
                execution_requests,
                archive_notices,
            } => {
                let proto_blob_sidecars = blob_sidecars
                    .iter()
                    .map(|sidecar| sidecar.to_proto())
                    .collect::<Result<Vec<_>, _>>()?;

                let proto_archive_notices = archive_notices
                    .iter()
                    .map(|notice| notice.to_proto())
                    .collect::<Result<Vec<_>, _>>()?;

                proto::synced_value_package::Package::Full(proto::FullPackage {
                    value: Some(value.to_proto()?),
                    execution_payload_ssz: execution_payload_ssz.clone(),
                    blob_sidecars: proto_blob_sidecars,
                    execution_requests: execution_requests
                        .iter()
                        .cloned()
                        .map(|req| req.0)
                        .collect(),
                    archive_notices: proto_archive_notices,
                })
            }
            SyncedValuePackage::MetadataOnly { value, archive_notices } => {
                let proto_archive_notices = archive_notices
                    .iter()
                    .map(|notice| notice.to_proto())
                    .collect::<Result<Vec<_>, _>>()?;

                proto::synced_value_package::Package::MetadataOnly(proto::MetadataOnlyPackage {
                    value: Some(value.to_proto()?),
                    archive_notices: proto_archive_notices,
                })
            }
        };

        Ok(proto::SyncedValuePackage { package: Some(package) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{BYTES_PER_BLOB, Blob, KzgCommitment, KzgProof};

    #[test]
    fn test_synced_value_package_full_is_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: Bytes::from(vec![1u8; 1024]),
            blob_sidecars: vec![],
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
        };

        assert!(package.is_full());
        assert!(package.execution_payload().is_some());
        assert!(package.blob_sidecars().is_some());
    }

    #[test]
    fn test_synced_value_package_metadata_only_is_not_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package =
            SyncedValuePackage::MetadataOnly { value: value.clone(), archive_notices: vec![] };

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
        let sidecar =
            BlobSidecar::from_bundle_item(0, blob, KzgCommitment([2u8; 48]), KzgProof([3u8; 48]));

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload.clone(),
            blob_sidecars: vec![sidecar],
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
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
        let package =
            SyncedValuePackage::MetadataOnly { value: value.clone(), archive_notices: vec![] };

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
            let sidecar = BlobSidecar::from_bundle_item(
                i as u16,
                blob,
                KzgCommitment([i + 1; 48]),
                KzgProof([i + 2; 48]),
            );
            sidecars.push(sidecar);
        }

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload,
            blob_sidecars: sidecars,
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
        };

        // Roundtrip
        let encoded = package.encode().unwrap();
        let decoded = SyncedValuePackage::decode(&encoded).unwrap();

        assert_eq!(package, decoded);

        // Verify blob count
        let blobs = decoded.blob_sidecars().unwrap();
        assert_eq!(blobs.len(), 3);
        for (i, sidecar) in blobs.iter().enumerate() {
            assert_eq!(sidecar.index, i as u16);
        }
    }

    #[test]
    fn test_estimated_size_full() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let payload = Bytes::from(vec![1u8; 1024]);
        let blob = Blob::new(vec![0u8; BYTES_PER_BLOB].into()).unwrap();
        let sidecar =
            BlobSidecar::from_bundle_item(0, blob, KzgCommitment([2u8; 48]), KzgProof([3u8; 48]));

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload,
            blob_sidecars: vec![sidecar],
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
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
        let package = SyncedValuePackage::MetadataOnly { value, archive_notices: vec![] };

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
        assert!(result.unwrap_err().contains("Failed to decode"));
    }

    #[test]
    fn test_protobuf_roundtrip_metadata_only() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let package = SyncedValuePackage::MetadataOnly { value, archive_notices: vec![] };

        // Encode using protobuf
        let encoded = package.encode().expect("Failed to encode");
        assert!(!encoded.is_empty());

        // Decode should succeed
        let decoded = SyncedValuePackage::decode(&encoded).expect("Failed to decode");

        // Should match original
        assert_eq!(package, decoded);
        assert!(!decoded.is_full());
    }

    #[test]
    fn test_protobuf_roundtrip_full_package() {
        #[allow(deprecated)]
        let value = Value::from_bytes(Bytes::from(vec![0u8; 32]));
        let payload = Bytes::from(vec![1u8; 1024]);
        let blob = Blob::new(vec![0u8; BYTES_PER_BLOB].into()).unwrap();
        let sidecar =
            BlobSidecar::from_bundle_item(0, blob, KzgCommitment([2u8; 48]), KzgProof([3u8; 48]));

        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload,
            blob_sidecars: vec![sidecar],
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
        };

        // Encode using protobuf
        let encoded = package.encode().expect("Failed to encode");
        assert!(!encoded.is_empty());

        // Decode
        let decoded = SyncedValuePackage::decode(&encoded).expect("Failed to decode");

        // Verify
        assert_eq!(package, decoded);
        assert!(decoded.is_full());
    }
}
