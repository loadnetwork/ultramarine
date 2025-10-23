//! Value Metadata for Consensus Voting
//!
//! This module defines the lightweight metadata structure that consensus votes on.
//! The critical design principle is: **consensus messages must be small (~2KB)**.
//!
//! ## Architecture Decision
//!
//! Instead of voting on the full execution payload + blob data (potentially MBs),
//! consensus votes on a hash of this lightweight metadata structure:
//!
//! ```text
//! Value {
//!     value: u64,                      // Hash of ValueMetadata (~8 bytes)
//!     metadata: ValueMetadata,         // Lightweight metadata (~2KB)
//! }
//!
//! ValueMetadata {
//!     execution_payload_header: ExecutionPayloadHeader,  // ~516 bytes
//!     blob_kzg_commitments: Vec<KzgCommitment>,         // 48 bytes × 6-9 = ~288-432 bytes
//!     blob_count: u8,                                    // 1 byte
//!     total_blob_bytes: u32,                             // 4 bytes
//! }
//!
//! Total: ~800-1000 bytes << 2KB target ✅
//! ```
//!
//! ## Data Flow
//!
//! ```text
//! 1. Execution Layer generates block:
//!    ExecutionPayloadV3 + BlobsBundle
//!           │
//!           ├─> Extract: ExecutionPayloadHeader (from_payload())
//!           └─> Extract: Vec<KzgCommitment> (from bundle)
//!
//! 2. Create ValueMetadata:
//!    ValueMetadata::new(header, commitments)
//!
//! 3. Consensus votes on:
//!    Value::new(metadata) → hash(metadata)
//!
//! 4. Full data streams separately:
//!    ProposalPart::Data → ExecutionPayload
//!    ProposalPart::BlobSidecar → Blobs
//! ```
//!
//! ## Why This Design?
//!
//! - **Small consensus messages**: Voting messages stay under 2KB
//! - **Fast hashing**: Computing consensus hash is < 1ms
//! - **Efficient verification**: Validators can verify metadata without full blobs
//! - **Bandwidth optimization**: Consensus gossip is lightweight
//!
//! ## References
//!
//! - FINAL_PLAN.md Phase 2: Value Refactor
//! - EIP-4844: https://eips.ethereum.org/EIPS/eip-4844

use core::fmt;

use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{
    blob::{BYTES_PER_BLOB, KzgCommitment, MAX_BLOBS_PER_BLOCK},
    engine_api::ExecutionPayloadHeader,
    proto,
};

/// Lightweight metadata about a proposed value
///
/// This is what gets voted on in consensus (keeps messages ~2KB).
/// The full execution payload and blob data are streamed separately
/// via ProposalParts to avoid bloating consensus messages.
///
/// ## Fields
///
/// - `execution_payload_header`: Block metadata without transactions (~516 bytes)
/// - `blob_kzg_commitments`: Commitments to blobs (48 bytes each, max 6-9)
/// - `blob_count`: Number of blobs included (for quick validation)
/// - `total_blob_bytes`: Total size of blob data (for bandwidth estimation)
///
/// ## Size Calculation
///
/// ```text
/// ExecutionPayloadHeader:     ~516 bytes
/// KzgCommitments (6 blobs):   ~288 bytes (48 × 6)
/// blob_count:                    1 byte
/// total_blob_bytes:              4 bytes
/// ─────────────────────────────────────
/// Total:                      ~809 bytes
///
/// With 9 blobs (Electra):    ~953 bytes
/// ```
///
/// Both well under 2KB target ✅
///
/// ## Example
///
/// ```rust,ignore
/// use ultramarine_types::{
///     value_metadata::ValueMetadata,
///     engine_api::ExecutionPayloadHeader,
///     blob::KzgCommitment,
/// };
///
/// // From execution layer response
/// let (payload, blobs_bundle) = execution_client
///     .generate_block_with_blobs(&latest_block)
///     .await?;
///
/// // Extract lightweight header
/// let header = ExecutionPayloadHeader::from_payload(&payload);
///
/// // Extract commitments
/// let commitments = blobs_bundle
///     .map(|b| b.commitments)
///     .unwrap_or_default();
///
/// // Create metadata for consensus voting
/// let metadata = ValueMetadata::new(header, commitments);
///
/// // Verify size is reasonable
/// assert!(metadata.size_bytes() < 2000); // < 2KB
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValueMetadata {
    /// Execution payload header (block hash, state root, etc.)
    ///
    /// **Purpose**: Contains all essential block metadata needed for validation
    /// without the full transaction list.
    ///
    /// **Size**: ~516 bytes
    pub execution_payload_header: ExecutionPayloadHeader,

    /// KZG commitments for blobs (48 bytes each, max 6-9 blobs)
    ///
    /// **Purpose**: These are the cryptographic commitments that:
    /// 1. Allow verifying blobs match the metadata
    /// 2. Are included in the execution payload
    /// 3. Enable validators to check blob availability
    ///
    /// **Size**: 48 bytes per blob (288-432 bytes for 6-9 blobs)
    pub blob_kzg_commitments: Vec<KzgCommitment>,

    /// Number of blobs included
    ///
    /// **Purpose**: Quick validation that the correct number of blobs
    /// were received via ProposalParts streaming.
    ///
    /// **Invariant**: Must equal `blob_kzg_commitments.len()`
    pub blob_count: u8,

    /// Total size of blob data in bytes
    ///
    /// **Purpose**: Bandwidth estimation and metrics.
    ///
    /// **Calculation**: `blob_count * 131_072`
    pub total_blob_bytes: u32,
}

impl ValueMetadata {
    /// Creates a new ValueMetadata from execution payload header and blob commitments
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let header = ExecutionPayloadHeader::from_payload(&payload);
    /// let commitments = blobs_bundle.commitments;
    ///
    /// let metadata = ValueMetadata::new(header, commitments);
    /// ```
    pub fn new(
        execution_payload_header: ExecutionPayloadHeader,
        blob_kzg_commitments: Vec<KzgCommitment>,
    ) -> Self {
        let blob_count = blob_kzg_commitments.len() as u8;
        let total_blob_bytes = (blob_count as u32) * BYTES_PER_BLOB as u32;

        Self { execution_payload_header, blob_kzg_commitments, blob_count, total_blob_bytes }
    }

    /// Calculate consensus hash for voting
    ///
    /// This is the hash that validators vote on. It's computed by hashing:
    /// 1. The execution payload header
    /// 2. All blob KZG commitments
    ///
    /// ## Implementation Note
    ///
    /// Uses `DefaultHasher` (SipHash) for consistency with existing `Value` hashing.
    /// This is NOT a cryptographic hash - it's just for consensus message identity.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let metadata = ValueMetadata::new(header, commitments);
    /// let hash = metadata.consensus_hash();
    ///
    /// // This hash becomes the Value.value field
    /// let value = Value::new(metadata);
    /// assert_eq!(value.value, hash);
    /// ```
    pub fn consensus_hash(&self) -> u64 {
        use std::{
            collections::hash_map::DefaultHasher,
            hash::{Hash, Hasher},
        };

        let mut hasher = DefaultHasher::new();

        // Hash execution payload header
        // Note: ExecutionPayloadHeader fields are hashed individually
        self.execution_payload_header.block_hash.0.hash(&mut hasher);
        self.execution_payload_header.parent_hash.0.hash(&mut hasher);
        self.execution_payload_header.state_root.0.hash(&mut hasher);
        self.execution_payload_header.receipts_root.0.hash(&mut hasher);
        self.execution_payload_header.logs_bloom.0.hash(&mut hasher);
        self.execution_payload_header.block_number.hash(&mut hasher);
        self.execution_payload_header.gas_limit.hash(&mut hasher);
        self.execution_payload_header.gas_used.hash(&mut hasher);
        self.execution_payload_header.timestamp.hash(&mut hasher);
        self.execution_payload_header.base_fee_per_gas.to_le_bytes::<32>().hash(&mut hasher);
        self.execution_payload_header.blob_gas_used.hash(&mut hasher);
        self.execution_payload_header.excess_blob_gas.hash(&mut hasher);
        self.execution_payload_header.prev_randao.0.hash(&mut hasher);
        self.execution_payload_header.fee_recipient.into_inner().hash(&mut hasher);

        // Hash all blob commitments
        for commitment in &self.blob_kzg_commitments {
            commitment.as_bytes().hash(&mut hasher);
        }

        hasher.finish()
    }

    /// Estimate size in bytes
    ///
    /// This is useful for:
    /// 1. Verifying we stay under 2KB target
    /// 2. Metrics and monitoring
    /// 3. Network bandwidth estimation
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let metadata = ValueMetadata::new(header, commitments);
    /// let size = metadata.size_bytes();
    ///
    /// // Verify size constraint
    /// assert!(size < 2000, "ValueMetadata too large: {} bytes", size);
    /// ```
    pub fn size_bytes(&self) -> usize {
        self.execution_payload_header.size_bytes()  // ~516 bytes
            + (self.blob_count as usize * 48)       // Commitments
            + 1                                      // blob_count
            + 4 // total_blob_bytes
    }

    /// Validate the metadata structure
    ///
    /// ## Checks
    ///
    /// 1. `blob_count` matches `blob_kzg_commitments.len()`
    /// 2. `blob_count` is within fork limits (6 for Deneb, 9 for Electra)
    /// 3. `total_blob_bytes` is correctly calculated
    ///
    /// ## Errors
    ///
    /// Returns an error string if any validation fails.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let metadata = ValueMetadata::new(header, commitments);
    ///
    /// // Validate before using in consensus
    /// metadata.validate()?;
    /// ```
    pub fn validate(&self) -> Result<(), String> {
        // Check 1: blob_count matches commitment count
        if self.blob_count as usize != self.blob_kzg_commitments.len() {
            return Err(format!(
                "blob_count mismatch: field={}, commitments={}",
                self.blob_count,
                self.blob_kzg_commitments.len()
            ));
        }

        // Check 2: Within protocol limit
        // This chain enforces a practical limit of 1024 blobs per block
        if self.blob_count as usize > MAX_BLOBS_PER_BLOCK {
            return Err(format!(
                "Too many blobs: got {}, max is {} (protocol limit)",
                self.blob_count, MAX_BLOBS_PER_BLOCK
            ));
        }

        // Check 3: total_blob_bytes is correct
        let expected_bytes = (self.blob_count as u32) * BYTES_PER_BLOB as u32;
        if self.total_blob_bytes != expected_bytes {
            return Err(format!(
                "total_blob_bytes mismatch: got {}, expected {}",
                self.total_blob_bytes, expected_bytes
            ));
        }

        Ok(())
    }
}

impl fmt::Display for ValueMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ValueMetadata(block={:?}, blobs={}, size={}KB)",
            self.execution_payload_header.block_hash,
            self.blob_count,
            self.total_blob_bytes / 1024
        )
    }
}

/// Protobuf conversion for ValueMetadata
///
/// This enables ValueMetadata to be serialized/deserialized for consensus messages.
impl Protobuf for ValueMetadata {
    type Proto = proto::ValueMetadata;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let execution_payload_header =
            ExecutionPayloadHeader::from_proto(proto.execution_payload_header.ok_or_else(
                || ProtoError::missing_field::<Self::Proto>("execution_payload_header"),
            )?)?;

        let blob_kzg_commitments: Result<Vec<KzgCommitment>, ProtoError> =
            proto.blob_kzg_commitments.into_iter().map(KzgCommitment::from_proto).collect();
        let blob_kzg_commitments = blob_kzg_commitments?;

        let blob_count = proto.blob_count as u8;
        let total_blob_bytes = proto.total_blob_bytes;

        Ok(Self { execution_payload_header, blob_kzg_commitments, blob_count, total_blob_bytes })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let execution_payload_header = Some(self.execution_payload_header.to_proto()?);

        let blob_kzg_commitments: Result<Vec<proto::KzgCommitment>, ProtoError> =
            self.blob_kzg_commitments.iter().map(|c| c.to_proto()).collect();
        let blob_kzg_commitments = blob_kzg_commitments?;

        Ok(proto::ValueMetadata {
            execution_payload_header,
            blob_kzg_commitments,
            blob_count: self.blob_count as u32,
            total_blob_bytes: self.total_blob_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        address::Address,
        aliases::{B256, Bloom, Bytes},
    };

    /// Helper to create a test ExecutionPayloadHeader
    fn create_test_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::from([1u8; 32]),
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            receipts_root: B256::from([4u8; 32]),
            logs_bloom: Bloom::from([0u8; 256]),
            block_number: 100,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 1234567890,
            base_fee_per_gas: crate::aliases::U256::from(1000000000u64),
            extra_data: Bytes::from(vec![1u8; 8]),
            transactions_root: B256::from([7u8; 32]),
            withdrawals_root: B256::from([8u8; 32]),
            blob_gas_used: 262144, // 2 blobs worth
            excess_blob_gas: 0,
            prev_randao: B256::from([5u8; 32]),
            fee_recipient: Address::new([6u8; 20]),
        }
    }

    #[test]
    fn test_value_metadata_creation() {
        let header = create_test_header();
        let commitments = vec![KzgCommitment::new([1u8; 48]), KzgCommitment::new([2u8; 48])];

        let metadata = ValueMetadata::new(header.clone(), commitments);

        assert_eq!(metadata.blob_count, 2);
        assert_eq!(metadata.total_blob_bytes, 2 * 131_072);
        assert_eq!(metadata.blob_kzg_commitments.len(), 2);
        assert_eq!(metadata.execution_payload_header, header);
    }

    #[test]
    fn test_value_metadata_size() {
        let header = create_test_header();
        let commitments = vec![
            KzgCommitment::new([1u8; 48]),
            KzgCommitment::new([2u8; 48]),
            KzgCommitment::new([3u8; 48]),
        ];

        let metadata = ValueMetadata::new(header, commitments);
        let size = metadata.size_bytes();

        // Should be roughly: 516 (header) + 144 (3 × 48) + 5 (fields) = 665 bytes
        assert!(size < 2000, "Metadata too large: {} bytes", size);
        assert!(size > 600, "Size calculation seems wrong: {} bytes", size);
    }

    #[test]
    fn test_value_metadata_validation() {
        let header = create_test_header();
        let commitments = vec![KzgCommitment::new([1u8; 48]), KzgCommitment::new([2u8; 48])];

        let metadata = ValueMetadata::new(header, commitments);
        assert!(metadata.validate().is_ok());
    }

    #[test]
    fn test_value_metadata_validation_fails_count_mismatch() {
        let header = create_test_header();
        let commitments = vec![KzgCommitment::new([1u8; 48])];

        let mut metadata = ValueMetadata::new(header, commitments);
        metadata.blob_count = 5; // Wrong count

        assert!(metadata.validate().is_err());
    }

    #[test]
    fn test_value_metadata_validation_fails_too_many_blobs() {
        let header = create_test_header();
        // Create more than MAX_BLOBS_PER_BLOCK (1024) to trigger validation error
        let commitments = vec![KzgCommitment::new([1u8; 48]); MAX_BLOBS_PER_BLOCK + 1];

        let metadata = ValueMetadata::new(header, commitments);
        assert!(metadata.validate().is_err());
    }

    #[test]
    fn test_consensus_hash_deterministic() {
        let header = create_test_header();
        let commitments = vec![KzgCommitment::new([1u8; 48]), KzgCommitment::new([2u8; 48])];

        let metadata1 = ValueMetadata::new(header.clone(), commitments.clone());
        let metadata2 = ValueMetadata::new(header, commitments);

        // Same metadata should produce same hash
        assert_eq!(metadata1.consensus_hash(), metadata2.consensus_hash());
    }

    #[test]
    fn test_consensus_hash_different_for_different_data() {
        let header1 = create_test_header();
        let mut header2 = create_test_header();
        header2.block_number = 101; // Different block number

        let commitments = vec![KzgCommitment::new([1u8; 48])];

        let metadata1 = ValueMetadata::new(header1, commitments.clone());
        let metadata2 = ValueMetadata::new(header2, commitments);

        // Different metadata should produce different hashes
        assert_ne!(metadata1.consensus_hash(), metadata2.consensus_hash());
    }

    #[test]
    fn test_empty_blobs_metadata() {
        let header = create_test_header();
        let commitments = vec![]; // No blobs

        let metadata = ValueMetadata::new(header, commitments);

        assert_eq!(metadata.blob_count, 0);
        assert_eq!(metadata.total_blob_bytes, 0);
        assert!(metadata.validate().is_ok());
    }
}
