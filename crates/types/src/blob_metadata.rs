//! Ethereum EIP-4844 Blob Metadata
//!
//! This module provides the compatibility bridge between Ultramarine's pure BFT consensus
//! and Ethereum's EIP-4844 blob format. It isolates all Ethereum-specific terminology
//! and conversion logic.
//!
//! ## Design Philosophy
//!
//! Layer 2 of the three-layer architecture:
//! - **Layer 1 (consensus_block_metadata)**: Pure BFT consensus state
//! - **Layer 2 (this module)**: Ethereum EIP-4844 compatibility bridge
//! - **Layer 3 (blob_engine)**: Prunable blob data storage
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │     LAYER 2: BLOB METADATA (Ethereum Compatibility)          │
//! │                    Keep Forever ♾️                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │ blob_metadata_decided:  height → BlobMetadata              │
//! │ blob_metadata_undecided: (h, r) → BlobMetadata              │
//! │                                                              │
//! │ Contains: parent_blob_root, kzg_commitments, execution header │
//! │ Purpose: EIP-4844 compatibility bridge                      │
//! │ Size: ~900 bytes per block (avg 6 blobs)                   │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Key Insight
//!
//! The conversion to Ethereum types (`BeaconBlockHeader`) happens ONLY when building
//! `BlobSidecar` for network transmission. The consensus layer never sees Ethereum types.
use std::convert::TryFrom;

use malachitebft_proto::{Error as ProtoError, Protobuf};

use crate::{
    address::Address,
    aliases::{B256, Bloom, Bytes, U256},
    blob::KzgCommitment,
    engine_api::ExecutionPayloadHeader,
    ethereum_compat::{BeaconBlockBodyMinimal, BeaconBlockHeader},
    height::Height,
    proto,
};

/// Ethereum EIP-4844 compatibility metadata
///
/// This is the bridge between Ultramarine consensus and Ethereum blob format.
/// Contains everything needed to build `SignedBeaconBlockHeader`.
/// Isolated from consensus layer for technology neutrality.
///
/// ## Fields
///
/// - `height`: Block height (maps to Ethereum `slot`)
/// - `parent_blob_root`: Chains blob headers together (hash of previous BeaconBlockHeader)
/// - `kzg_commitments`: KZG commitments for all blobs at this height
/// - `blob_count`: Number of blobs (0 for blobless blocks)
/// - `execution_payload_header`: Lightweight execution payload header (copied from ValueMetadata)
/// - `proposer_index_hint`: Optional proposer index to embed in Beacon headers
///
/// ## Size
///
/// Approximately 900 bytes (with 6 blobs average):
/// - height: 8 bytes
/// - parent_blob_root: 32 bytes
/// - kzg_commitments: 6 × 48 = 288 bytes
/// - blob_count: 2 bytes
/// - execution_payload_header: ~516 bytes
/// - proposer_index_hint: 8 bytes
/// - protobuf overhead: ~60 bytes
/// **Total: ~900 bytes (6 blobs), ~600 bytes (0 blobs)**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Block height (maps to Ethereum slot)
    pub height: Height,

    /// Parent blob header root (chains blob headers together)
    ///
    /// This is the `hash_tree_root()` of the previous block's `BeaconBlockHeader`.
    /// For height 0, this is `B256::ZERO`.
    pub parent_blob_root: B256,

    /// KZG commitments for all blobs at this height
    ///
    /// Empty vector for blobless blocks.
    pub blob_kzg_commitments: Vec<KzgCommitment>,

    /// Number of blobs (0 for blobless blocks)
    pub blob_count: u16,

    /// Lightweight execution payload header (copied from ValueMetadata)
    pub execution_payload_header: ExecutionPayloadHeader,

    /// Optional proposer index hint to embed into Beacon headers
    pub proposer_index_hint: Option<u64>,
}

impl BlobMetadata {
    /// Create new blob metadata
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `parent_blob_root` - Parent blob header root
    /// * `blob_kzg_commitments` - KZG commitments for blobs
    /// * `execution_payload_header` - Lightweight execution payload header (copied from
    ///   ValueMetadata)
    /// * `proposer_index_hint` - Optional proposer index to embed in Beacon headers
    pub fn new(
        height: Height,
        parent_blob_root: B256,
        blob_kzg_commitments: Vec<KzgCommitment>,
        execution_payload_header: ExecutionPayloadHeader,
        proposer_index_hint: Option<u64>,
    ) -> Self {
        let blob_count =
            u16::try_from(blob_kzg_commitments.len()).expect("blob count exceeds u16::MAX");

        let metadata = Self {
            height,
            parent_blob_root,
            blob_kzg_commitments,
            blob_count,
            execution_payload_header,
            proposer_index_hint,
        };

        debug_assert_eq!(usize::from(metadata.blob_count), metadata.blob_kzg_commitments.len());

        metadata
    }

    /// Create metadata for blobless block
    ///
    /// This is used when a block has no blobs but we still need to maintain
    /// the parent-root chain for future blob blocks.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `parent_blob_root` - Parent blob header root
    /// * `execution` - Execution payload header
    pub fn blobless(
        height: Height,
        parent_blob_root: B256,
        execution: &ExecutionPayloadHeader,
        proposer_index_hint: Option<u64>,
    ) -> Self {
        Self::new(height, parent_blob_root, Vec::new(), execution.clone(), proposer_index_hint)
    }

    /// Create genesis blob metadata for height 0
    ///
    /// Seeds the blob metadata store with a height 0 entry to satisfy the parent lookup
    /// requirement for the first blobbed block at height 1. Without this, nodes reject
    /// blob proposals at height 1 with "Missing decided BlobMetadata for parent height 0".
    ///
    /// Used during bootstrap when the store is empty after a clean init.
    ///
    /// # Important: Not Validated Against Actual Genesis Block
    ///
    /// This genesis `BlobMetadata` is a **consensus-layer bookkeeping entry**, NOT a
    /// representation of Reth's actual genesis block (from `genesis.json`). The values in
    /// the `ExecutionPayloadHeader` (gas limit, timestamps, etc.) are:
    ///
    /// - **Never validated** against Reth's real genesis block
    /// - **Arbitrary** - chosen for determinism, not accuracy
    /// - **Consistent** - all nodes use the same `genesis()` code
    ///
    /// The only requirement is that all nodes compute the **same hash** from this metadata.
    /// When height 1 arrives, it will compute `parent_blob_root = hash(genesis metadata)`,
    /// and as long as all nodes use identical values here, consensus is maintained.
    ///
    /// ## Layer Separation
    ///
    /// ```text
    /// Reth genesis.json (Layer 3)     BlobMetadata::genesis() (Layer 2)
    /// ├─ Actual chain state           ├─ Consensus bookkeeping only
    /// ├─ Account balances             ├─ Parent lookup artifact
    /// └─ Real gas limit               └─ Placeholder values (deterministic)
    /// ```
    ///
    /// # Returns
    ///
    /// BlobMetadata with:
    /// - height = 0
    /// - parent_blob_root = B256::ZERO
    /// - empty KZG commitments (no blobs at genesis)
    /// - minimal execution payload header (zero values, gas_limit=30M arbitrary)
    /// - proposer_index_hint = 0
    pub fn genesis() -> Self {
        let genesis_header = ExecutionPayloadHeader {
            block_hash: B256::ZERO,
            parent_hash: B256::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            block_number: 0,
            gas_limit: 30_000_000, // Standard default
            gas_used: 0,
            timestamp: 0,
            base_fee_per_gas: U256::ZERO,
            extra_data: Bytes::new(),
            transactions_root: B256::ZERO,
            withdrawals_root: B256::ZERO,
            blob_gas_used: 0,
            excess_blob_gas: 0,
            prev_randao: B256::ZERO,
            fee_recipient: Address::repeat_byte(0),
            requests_hash: None,
        };

        Self::blobless(Height::new(0), B256::ZERO, &genesis_header, Some(0))
    }

    /// Build Ethereum-compatible BeaconBlockHeader
    ///
    /// This is ONLY called when constructing BlobSidecars for network streaming.
    /// Consensus layer never calls this - it's an Ethereum compatibility shim.
    ///
    /// # Returns
    ///
    /// `BeaconBlockHeader` that can be used in `SignedBeaconBlockHeader`
    pub fn to_beacon_header(&self) -> BeaconBlockHeader {
        let proposer_index = self.proposer_index_hint.unwrap_or(0);
        BeaconBlockHeader {
            slot: self.height.as_u64(),
            proposer_index,
            parent_root: self.parent_blob_root,
            state_root: self.execution_payload_header.state_root,
            body_root: self.compute_body_root(),
        }
    }

    /// Compute body_root for BeaconBlockBody
    ///
    /// This computes the merkle root of a minimal beacon block body
    /// containing only the KZG commitments.
    fn compute_body_root(&self) -> B256 {
        let body = BeaconBlockBodyMinimal::from_ultramarine_data(
            self.blob_kzg_commitments.clone(),
            &self.execution_payload_header,
        );
        body.compute_body_root()
    }

    /// Compute blob root for parent chaining
    ///
    /// This is the `hash_tree_root()` of the `BeaconBlockHeader` and will be used
    /// as the `parent_blob_root` for the next block.
    ///
    /// # Returns
    ///
    /// Hash tree root that can be used as parent_blob_root for next block
    pub fn compute_blob_root(&self) -> B256 {
        self.to_beacon_header().hash_tree_root()
    }

    /// Check if this block has blobs
    pub fn has_blobs(&self) -> bool {
        self.blob_count > 0
    }

    /// Get number of blobs
    pub fn blob_count(&self) -> u16 {
        self.blob_count
    }

    /// Get height
    pub fn height(&self) -> Height {
        self.height
    }

    /// Get parent blob root
    pub fn parent_blob_root(&self) -> B256 {
        self.parent_blob_root
    }

    /// Get KZG commitments
    pub fn blob_kzg_commitments(&self) -> &[KzgCommitment] {
        &self.blob_kzg_commitments
    }

    /// Get execution state root
    pub fn execution_state_root(&self) -> B256 {
        self.execution_payload_header.state_root
    }

    /// Get execution block hash
    pub fn execution_block_hash(&self) -> B256 {
        self.execution_payload_header.block_hash
    }

    /// Get execution payload header
    pub fn execution_payload_header(&self) -> &ExecutionPayloadHeader {
        &self.execution_payload_header
    }

    /// Get proposer index hint (if any)
    pub fn proposer_index_hint(&self) -> Option<u64> {
        self.proposer_index_hint
    }
}

/// Protobuf encoding/decoding for BlobMetadata
///
/// This enables storage in the consensus store and potential network transmission.
impl Protobuf for BlobMetadata {
    type Proto = proto::BlobMetadata;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let blob_kzg_commitments = proto
            .kzg_commitments
            .into_iter()
            .map(|bytes| {
                KzgCommitment::from_slice(&bytes)
                    .map_err(|e| ProtoError::Other(format!("Invalid KZG commitment: {}", e)))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let blob_count = u16::try_from(proto.blob_count).map_err(|_| {
            ProtoError::Other(format!("blob_count {} exceeds u16::MAX", proto.blob_count))
        })?;

        if usize::from(blob_count) != blob_kzg_commitments.len() {
            return Err(ProtoError::Other(format!(
                "blob_count {} does not match number of commitments {}",
                blob_count,
                blob_kzg_commitments.len()
            )));
        }

        let execution_payload_header =
            ExecutionPayloadHeader::from_proto(proto.execution_payload_header.ok_or_else(
                || ProtoError::missing_field::<Self::Proto>("execution_payload_header"),
            )?)?;

        Ok(Self {
            height: Height::new(proto.height),
            parent_blob_root: B256::from_slice(&proto.parent_blob_root),
            blob_kzg_commitments,
            blob_count,
            execution_payload_header,
            proposer_index_hint: proto.proposer_index_hint,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::BlobMetadata {
            height: self.height.as_u64(),
            parent_blob_root: self.parent_blob_root.to_vec().into(),
            kzg_commitments: self
                .blob_kzg_commitments
                .iter()
                .map(|c| c.0.to_vec().into())
                .collect(),
            blob_count: self.blob_kzg_commitments.len() as u32,
            execution_payload_header: Some(self.execution_payload_header.to_proto()?),
            proposer_index_hint: self.proposer_index_hint,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bloom, Bytes, U256};
    use prost::Message;

    use super::*;
    use crate::address::Address;

    fn sample_execution_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::from([1u8; 32]),
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            receipts_root: B256::from([4u8; 32]),
            logs_bloom: Bloom::ZERO,
            block_number: 100,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 1234567890,
            base_fee_per_gas: U256::from(20),
            blob_gas_used: 0,
            excess_blob_gas: 0,
            prev_randao: B256::ZERO,
            fee_recipient: Address::new([5u8; 20]),
            extra_data: Bytes::new(),
            transactions_root: B256::from([6u8; 32]),
            withdrawals_root: B256::from([7u8; 32]),
            requests_hash: None,
        }
    }

    #[test]
    fn test_blob_metadata_creation() {
        let commitment = KzgCommitment::new([1u8; 48]);
        let metadata = BlobMetadata::new(
            Height::new(100),
            B256::from([2u8; 32]),
            vec![commitment],
            sample_execution_header(),
            Some(42),
        );

        assert_eq!(metadata.height(), Height::new(100));
        assert_eq!(metadata.blob_count(), 1);
        assert!(metadata.has_blobs());
        assert_eq!(metadata.blob_kzg_commitments().len(), 1);
        assert_eq!(metadata.proposer_index_hint(), Some(42));
    }

    #[test]
    fn test_blobless_metadata() {
        let execution_header = sample_execution_header();

        let metadata = BlobMetadata::blobless(
            Height::new(50),
            B256::from([10u8; 32]),
            &execution_header,
            None,
        );

        assert_eq!(metadata.height(), Height::new(50));
        assert_eq!(metadata.blob_count(), 0);
        assert!(!metadata.has_blobs());
        assert!(metadata.blob_kzg_commitments().is_empty());
        assert_eq!(metadata.execution_state_root(), execution_header.state_root);
        assert_eq!(metadata.execution_block_hash(), execution_header.block_hash);
    }

    #[test]
    fn test_to_beacon_header() {
        let commitment = KzgCommitment::new([1u8; 48]);
        let metadata = BlobMetadata::new(
            Height::new(100),
            B256::from([2u8; 32]),
            vec![commitment],
            sample_execution_header(),
            Some(7),
        );

        let beacon_header = metadata.to_beacon_header();

        assert_eq!(beacon_header.slot, 100);
        assert_eq!(beacon_header.proposer_index, 7);
        assert_eq!(beacon_header.parent_root, B256::from([2u8; 32]));
        assert_eq!(beacon_header.state_root, metadata.execution_state_root());
    }

    #[test]
    fn test_compute_blob_root() {
        let commitment = KzgCommitment::new([1u8; 48]);
        let metadata = BlobMetadata::new(
            Height::new(100),
            B256::from([2u8; 32]),
            vec![commitment],
            sample_execution_header(),
            Some(0),
        );

        let blob_root = metadata.compute_blob_root();

        assert_ne!(blob_root, B256::ZERO);

        let expected_root = metadata.to_beacon_header().hash_tree_root();
        assert_eq!(blob_root, expected_root);
    }

    #[test]
    fn test_parent_root_chaining() {
        let mut header1 = sample_execution_header();
        header1.block_hash = B256::from([11u8; 32]);
        let commitment1 = KzgCommitment::new([1u8; 48]);
        let metadata1 =
            BlobMetadata::new(Height::new(1), B256::ZERO, vec![commitment1], header1, Some(1));

        let blob_root1 = metadata1.compute_blob_root();

        let mut header2 = sample_execution_header();
        header2.block_hash = B256::from([22u8; 32]);
        let commitment2 = KzgCommitment::new([2u8; 48]);
        let metadata2 =
            BlobMetadata::new(Height::new(2), blob_root1, vec![commitment2], header2, Some(2));

        assert_eq!(metadata2.parent_blob_root(), blob_root1);
    }

    #[test]
    fn test_protobuf_roundtrip() {
        let commitment = KzgCommitment::new([1u8; 48]);
        let metadata = BlobMetadata::new(
            Height::new(123),
            B256::from([2u8; 32]),
            vec![commitment],
            sample_execution_header(),
            Some(0),
        );

        let proto = metadata.to_proto().expect("Failed to encode");
        let decoded = BlobMetadata::from_proto(proto).expect("Failed to decode");

        assert_eq!(metadata, decoded);
    }

    #[test]
    fn test_protobuf_roundtrip_blobless() {
        let execution_header = sample_execution_header();

        let metadata = BlobMetadata::blobless(
            Height::new(50),
            B256::from([10u8; 32]),
            &execution_header,
            Some(5),
        );

        let proto = metadata.to_proto().expect("Failed to encode");
        let decoded = BlobMetadata::from_proto(proto).expect("Failed to decode");

        assert_eq!(metadata, decoded);
    }

    #[test]
    fn test_protobuf_serialization_size() {
        let commitments: Vec<_> = (0..6).map(|i| KzgCommitment::new([i; 48])).collect();
        let metadata = BlobMetadata::new(
            Height::new(1000),
            B256::from([2u8; 32]),
            commitments,
            sample_execution_header(),
            None,
        );

        let proto = metadata.to_proto().expect("Failed to encode");
        let bytes = proto.encode_to_vec();

        assert!(
            bytes.len() < 1100,
            "BlobMetadata protobuf size {} unexpectedly large",
            bytes.len()
        );
        println!("BlobMetadata protobuf size (6 blobs): {} bytes", bytes.len());
    }

    #[test]
    fn test_protobuf_serialization_size_blobless() {
        let execution_header = sample_execution_header();
        let metadata = BlobMetadata::blobless(
            Height::new(1000),
            B256::from([2u8; 32]),
            &execution_header,
            None,
        );

        let proto = metadata.to_proto().expect("Failed to encode");
        let bytes = proto.encode_to_vec();

        assert!(
            bytes.len() < 700,
            "BlobMetadata (blobless) protobuf size {} unexpectedly large",
            bytes.len()
        );
        println!("BlobMetadata protobuf size (blobless): {} bytes", bytes.len());
    }

    #[test]
    fn test_multiple_blobs() {
        let commitments: Vec<_> = (0..10).map(|i| KzgCommitment::new([i; 48])).collect();
        let metadata = BlobMetadata::new(
            Height::new(100),
            B256::from([2u8; 32]),
            commitments.clone(),
            sample_execution_header(),
            None,
        );

        assert_eq!(metadata.blob_count(), 10);
        assert_eq!(metadata.blob_kzg_commitments().len(), 10);
        assert!(metadata.has_blobs());
    }
}
