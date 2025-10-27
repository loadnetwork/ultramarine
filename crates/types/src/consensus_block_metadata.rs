//! Pure BFT Consensus Block Metadata
//!
//! This module contains metadata about consensus blocks using pure BFT terminology.
//! It deliberately avoids Ethereum-specific naming (like "slot", "proposer_index", etc.)
//! to maintain technology neutrality.
//!
//! ## Design Philosophy
//!
//! Layer 1 of the three-layer architecture:
//! - **Layer 1 (this module)**: Pure BFT consensus state
//! - **Layer 2 (blob_metadata)**: Ethereum EIP-4844 compatibility bridge
//! - **Layer 3 (blob_engine)**: Prunable blob data storage
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │        LAYER 1: CONSENSUS STATE (Pure BFT)                   │
//! │                    Keep Forever ♾️                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │ consensus_block_metadata: height → ConsensusBlockMetadata   │
//! │                                                              │
//! │ Naming: Tendermint/Malachite aligned (height, round)       │
//! │ Purpose: Pure BFT consensus decisions                       │
//! │ Size: ~200 bytes per block                                  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use malachitebft_core_types::Round;
//! use ultramarine_types::{
//!     address::Address, aliases::B256, consensus_block_metadata::ConsensusBlockMetadata,
//!     height::Height,
//! };
//!
//! let metadata = ConsensusBlockMetadata {
//!     height: Height::new(100),
//!     round: Round::new(0),
//!     proposer: Address::new([1u8; 20]),
//!     timestamp: 1234567890,
//!     validator_set_hash: B256::ZERO,
//!     execution_block_hash: B256::ZERO,
//!     gas_limit: 30_000_000,
//!     gas_used: 15_000_000,
//! };
//! ```

use malachitebft_core_types::Round;
use malachitebft_proto::{Error as ProtoError, Protobuf};

use crate::{address::Address, aliases::B256, height::Height, proto};

/// Pure consensus-layer block metadata
///
/// Contains ONLY what's relevant to Ultramarine's BFT consensus.
/// Uses Tendermint/Malachite terminology (height, round, proposer).
/// NO Ethereum types, NO blob-specific data.
///
/// ## Fields
///
/// - `height`: Block height (NOT "slot" - this is pure BFT terminology)
/// - `round`: Consensus round that decided this block
/// - `proposer`: Validator who proposed this block (NOT "proposer_index")
/// - `timestamp`: Unix timestamp when block was proposed
/// - `validator_set_hash`: Hash of active validator set at this height
/// - `execution_block_hash`: Execution layer block hash (from Engine API)
/// - `gas_limit`: Gas limit for this block (consensus-relevant)
/// - `gas_used`: Gas used in this block (consensus-relevant)
///
/// ## Size
///
/// Approximately 200 bytes:
/// - height: 8 bytes
/// - round: 4 bytes
/// - proposer: 20 bytes
/// - timestamp: 8 bytes
/// - validator_set_hash: 32 bytes
/// - execution_block_hash: 32 bytes
/// - gas_limit: 8 bytes
/// - gas_used: 8 bytes
/// - protobuf overhead: ~80 bytes
/// **Total: ~200 bytes**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsensusBlockMetadata {
    /// Block height (NOT "slot")
    pub height: Height,

    /// Consensus round that decided this block
    pub round: Round,

    /// Validator who proposed this block
    pub proposer: Address,

    /// Timestamp when block was proposed (Unix timestamp in seconds)
    pub timestamp: u64,

    /// Hash of active validator set at this height
    pub validator_set_hash: B256,

    /// Execution layer block hash (from Engine API)
    pub execution_block_hash: B256,

    /// Gas limit for this block
    pub gas_limit: u64,

    /// Gas used in this block
    pub gas_used: u64,
}

impl ConsensusBlockMetadata {
    /// Create new consensus block metadata
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `round` - Consensus round
    /// * `proposer` - Validator who proposed the block
    /// * `timestamp` - Unix timestamp (seconds)
    /// * `validator_set_hash` - Hash of validator set
    /// * `execution_block_hash` - Execution block hash
    /// * `gas_limit` - Gas limit
    /// * `gas_used` - Gas used
    pub fn new(
        height: Height,
        round: Round,
        proposer: Address,
        timestamp: u64,
        validator_set_hash: B256,
        execution_block_hash: B256,
        gas_limit: u64,
        gas_used: u64,
    ) -> Self {
        Self {
            height,
            round,
            proposer,
            timestamp,
            validator_set_hash,
            execution_block_hash,
            gas_limit,
            gas_used,
        }
    }

    /// Get block height
    pub fn height(&self) -> Height {
        self.height
    }

    /// Get consensus round
    pub fn round(&self) -> Round {
        self.round
    }

    /// Get proposer address
    pub fn proposer(&self) -> &Address {
        &self.proposer
    }

    /// Get timestamp
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Get validator set hash
    pub fn validator_set_hash(&self) -> B256 {
        self.validator_set_hash
    }

    /// Get execution block hash
    pub fn execution_block_hash(&self) -> B256 {
        self.execution_block_hash
    }

    /// Get gas limit
    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    /// Get gas used
    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }
}

/// Protobuf encoding/decoding for ConsensusBlockMetadata
///
/// This enables storage in the consensus store and potential network transmission.
impl Protobuf for ConsensusBlockMetadata {
    type Proto = proto::ConsensusBlockMetadata;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        if proto.round < 0 {
            return Err(ProtoError::Other(format!("round {} cannot be negative", proto.round)));
        }

        Ok(Self {
            height: Height::new(proto.height),
            round: Round::new(proto.round as u32),
            proposer: Address::from_proto(
                proto
                    .proposer
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("proposer"))?,
            )?,
            timestamp: proto.timestamp,
            validator_set_hash: B256::from_slice(&proto.validator_set_hash),
            execution_block_hash: B256::from_slice(&proto.execution_block_hash),
            gas_limit: proto.gas_limit,
            gas_used: proto.gas_used,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::ConsensusBlockMetadata {
            height: self.height.as_u64(),
            round: self.round.as_i64() as i32,
            proposer: Some(self.proposer.to_proto()?),
            timestamp: self.timestamp,
            validator_set_hash: self.validator_set_hash.to_vec().into(),
            execution_block_hash: self.execution_block_hash.to_vec().into(),
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
        })
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn test_consensus_block_metadata_creation() {
        let metadata = ConsensusBlockMetadata::new(
            Height::new(100),
            Round::new(0),
            Address::new([1u8; 20]),
            1234567890,
            B256::from([2u8; 32]),
            B256::from([3u8; 32]),
            30_000_000,
            15_000_000,
        );

        assert_eq!(metadata.height(), Height::new(100));
        assert_eq!(metadata.round(), Round::new(0));
        assert_eq!(metadata.timestamp(), 1234567890);
        assert_eq!(metadata.gas_limit(), 30_000_000);
        assert_eq!(metadata.gas_used(), 15_000_000);
    }

    #[test]
    fn test_consensus_block_metadata_accessors() {
        let proposer = Address::new([1u8; 20]);
        let validator_set_hash = B256::from([2u8; 32]);
        let execution_block_hash = B256::from([3u8; 32]);

        let metadata = ConsensusBlockMetadata {
            height: Height::new(42),
            round: Round::new(1),
            proposer,
            timestamp: 9999999,
            validator_set_hash,
            execution_block_hash,
            gas_limit: 50_000_000,
            gas_used: 25_000_000,
        };

        assert_eq!(*metadata.proposer(), proposer);
        assert_eq!(metadata.validator_set_hash(), validator_set_hash);
        assert_eq!(metadata.execution_block_hash(), execution_block_hash);
    }

    #[test]
    fn test_protobuf_roundtrip() {
        let metadata = ConsensusBlockMetadata::new(
            Height::new(123),
            Round::new(2),
            Address::new([5u8; 20]),
            1700000000,
            B256::from([6u8; 32]),
            B256::from([7u8; 32]),
            40_000_000,
            20_000_000,
        );

        // Encode to protobuf
        let proto = metadata.to_proto().expect("Failed to encode");

        // Decode from protobuf
        let decoded = ConsensusBlockMetadata::from_proto(proto).expect("Failed to decode");

        // Verify roundtrip
        assert_eq!(metadata, decoded);
    }

    #[test]
    fn test_protobuf_serialization_size() {
        let metadata = ConsensusBlockMetadata::new(
            Height::new(1000),
            Round::new(0),
            Address::new([1u8; 20]),
            1234567890,
            B256::from([2u8; 32]),
            B256::from([3u8; 32]),
            30_000_000,
            15_000_000,
        );

        let proto = metadata.to_proto().expect("Failed to encode");
        let bytes = proto.encode_to_vec();

        // Verify size is approximately 200 bytes (within reasonable range)
        assert!(
            bytes.len() < 250,
            "ConsensusBlockMetadata protobuf size {} exceeds 250 bytes",
            bytes.len()
        );
        println!("ConsensusBlockMetadata protobuf size: {} bytes", bytes.len());
    }

    #[test]
    fn test_clone_and_equality() {
        let metadata1 = ConsensusBlockMetadata::new(
            Height::new(100),
            Round::new(0),
            Address::new([1u8; 20]),
            1234567890,
            B256::ZERO,
            B256::ZERO,
            30_000_000,
            15_000_000,
        );

        let metadata2 = metadata1.clone();

        assert_eq!(metadata1, metadata2);
    }
}
