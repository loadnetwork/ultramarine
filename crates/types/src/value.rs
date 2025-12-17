use core::fmt;

use bytes::Bytes;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{
    // Phase 2: Import types needed for Value refactor
    aliases::Bytes as AlloyBytes,
    blob::KzgCommitment,
    engine_api::ExecutionPayloadHeader,
    proto,
    // Phase 2: Import ValueMetadata for lightweight consensus voting
    value_metadata::ValueMetadata,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Serialize, Deserialize)]
pub struct ValueId(u64);

impl ValueId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

impl From<u64> for ValueId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

impl Protobuf for ValueId {
    type Proto = proto::ValueId;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(ValueId::new(proto.value))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::ValueId { value: self.0 })
    }
}

/// The value to decide on
///
/// ## Phase 2 Refactor: Lightweight Metadata
///
/// **CRITICAL**: This structure has been refactored to contain only lightweight
/// metadata (~2KB) instead of full execution payload + blob data (potentially MBs).
///
/// ### Before (Phase 1):
/// ```rust,ignore
/// Value {
///     value: u64,              // Hash of extensions
///     extensions: Bytes,       // Full data (could be MBs with blobs!)
/// }
/// ```
///
/// ### After (Phase 2):
/// ```rust,ignore
/// Value {
///     value: u64,                  // Hash of metadata
///     metadata: ValueMetadata,     // Lightweight (~2KB)
/// }
/// ```
///
/// ### Migration Path:
///
/// Old code that accessed `value.extensions` must be updated:
///
/// ```rust,ignore
/// // ❌ Old (Phase 1):
/// let data = value.extensions;
///
/// // ✅ New (Phase 2):
/// let header = &value.metadata.execution_payload_header;
/// let commitments = &value.metadata.blob_kzg_commitments;
/// ```
///
/// ### Data Flow:
///
/// 1. **Block Proposal**: ```text Execution Layer ↓ (payload, blobs) → Extract → ValueMetadata ↓
///    Value::new(metadata) → Consensus votes on hash(metadata) ↓ Full data streams separately via
///    ProposalParts ```
///
/// 2. **Consensus Voting**:
///    - Validators vote on `Value.value` (u64 hash)
///    - Consensus messages contain `Value.metadata` (~2KB)
///    - Full payload + blobs stream via separate channel
///
/// 3. **Block Import**:
///    - Retrieve full payload from ProposalPart::Data
///    - Retrieve blobs from ProposalPart::BlobSidecar
///    - Verify against metadata commitments
///    - Import to execution layer
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Value {
    /// Consensus hash of the metadata
    ///
    /// This is the u64 hash that validators vote on. It's computed by
    /// hashing the lightweight metadata structure.
    ///
    /// **Why u64?** Malachite's consensus protocol requires a hashable
    /// identifier for values. We use SipHash (via DefaultHasher) for speed.
    pub value: u64,

    /// Lightweight metadata (~2KB) - NOT full blob data
    ///
    /// **CRITICAL**: This contains only the essential information needed
    /// for consensus voting:
    /// - Execution payload header (~516 bytes)
    /// - Blob KZG commitments (48 bytes × 6-9)
    /// - Blob count and size metadata
    ///
    /// The full execution payload and blob data are streamed separately
    /// to keep consensus messages small and efficient.
    pub metadata: ValueMetadata,

    /// DEPRECATED: Legacy extensions field for backward compatibility
    ///
    /// **Phase 2 Note**: This field is kept temporarily for backward
    /// compatibility during migration. It should always be empty in new code.
    ///
    /// **TODO**: Remove this field once all code is migrated to use metadata.
    ///
    /// See FINAL_PLAN.md Phase 2 for migration guide.
    #[serde(default)]
    #[deprecated(note = "Use metadata field instead. This will be removed in future versions.")]
    pub extensions: Bytes,
}

impl Value {
    /// Creates a new Value from metadata
    ///
    /// This is the primary constructor for Phase 2. It takes lightweight
    /// metadata and computes the consensus hash.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// use ultramarine_types::{
    ///     value::Value,
    ///     value_metadata::ValueMetadata,
    ///     engine_api::ExecutionPayloadHeader,
    ///     blob::KzgCommitment,
    /// };
    ///
    /// // From execution layer
    /// let (payload, blobs_bundle) = execution_client
    ///     .generate_block_with_blobs(&latest_block)
    ///     .await?;
    ///
    /// // Extract metadata
    /// let header = ExecutionPayloadHeader::from_payload(&payload, None)?;
    /// let commitments = blobs_bundle
    ///     .map(|b| b.commitments)
    ///     .unwrap_or_default();
    ///
    /// // Create value for consensus
    /// let metadata = ValueMetadata::new(header, commitments);
    /// let value = Value::new(metadata);
    ///
    /// // Verify size is reasonable
    /// assert!(value.size_bytes() < 3000); // < 3KB
    /// ```
    pub fn new(metadata: ValueMetadata) -> Self {
        // Validate metadata during development to catch inconsistent state early
        // This is a debug_assert! so it's compiled out in release builds
        debug_assert!(
            metadata.validate().is_ok(),
            "Invalid metadata passed to Value::new(): {:?}",
            metadata.validate().err()
        );

        let value = metadata.consensus_hash();
        Self {
            value,
            metadata,
            #[allow(deprecated)]
            extensions: Bytes::new(), // Empty for new values
        }
    }

    /// Legacy constructor from bytes (Phase 1 compatibility)
    ///
    /// **DEPRECATED**: This method is kept for backward compatibility
    /// during Phase 1 → Phase 2 migration.
    ///
    /// **New code should use `Value::new(metadata)` instead.**
    ///
    /// ## Migration Note
    ///
    /// If you have code like:
    /// ```rust,ignore
    /// let value = Value::from_bytes(data);
    /// ```
    ///
    /// Update it to:
    /// ```rust,ignore
    /// let metadata = ValueMetadata::new(header, commitments);
    /// let value = Value::new(metadata);
    /// ```
    #[deprecated(note = "Use Value::new(metadata) instead")]
    pub fn from_bytes(data: Bytes) -> Self {
        use std::{
            collections::hash_map::DefaultHasher,
            hash::{Hash, Hasher},
        };

        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);

        Self {
            value: hasher.finish(),
            // Create empty metadata as placeholder
            // This is a temporary solution during migration
            metadata: ValueMetadata::new(
                ExecutionPayloadHeader {
                    block_hash: Default::default(),
                    parent_hash: Default::default(),
                    state_root: Default::default(),
                    receipts_root: Default::default(),
                    logs_bloom: Default::default(),
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: 0,
                    base_fee_per_gas: Default::default(),
                    extra_data: AlloyBytes::from(Vec::<u8>::new()),
                    transactions_root: Default::default(),
                    withdrawals_root: Default::default(),
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                    prev_randao: Default::default(),
                    fee_recipient: crate::address::Address::repeat_byte(0),
                    requests_hash: None,
                },
                vec![],
            ),
            #[allow(deprecated)]
            extensions: data,
        }
    }

    pub fn id(&self) -> ValueId {
        ValueId(self.value)
    }

    pub fn size_bytes(&self) -> usize {
        std::mem::size_of_val(&self.value) + self.metadata.size_bytes()
    }

    /// Get blob commitments for verification (Phase 2 accessor)
    ///
    /// This is a convenience method to access the blob commitments
    /// from the metadata without accessing the field directly.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let commitments = value.blob_commitments();
    ///
    /// // Verify blobs match metadata
    /// for (sidecar, commitment) in sidecars.iter().zip(commitments) {
    ///     assert_eq!(&sidecar.kzg_commitment, commitment);
    /// }
    /// ```
    pub fn blob_commitments(&self) -> &[KzgCommitment] {
        &self.metadata.blob_kzg_commitments
    }

    /// Get execution payload header (Phase 2 accessor)
    ///
    /// This is a convenience method to access the execution payload header
    /// from the metadata.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let header = value.execution_payload_header();
    /// println!("Block hash: {:?}", header.block_hash);
    /// println!("Block number: {}", header.block_number);
    /// ```
    pub fn execution_payload_header(&self) -> &ExecutionPayloadHeader {
        &self.metadata.execution_payload_header
    }
}

impl malachitebft_core_types::Value for Value {
    type Id = ValueId;

    fn id(&self) -> ValueId {
        self.id()
    }
}

impl Protobuf for Value {
    type Proto = proto::Value;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let value = proto.value;
        let extensions = proto.extensions;

        // Deserialize metadata from protobuf
        let metadata = if let Some(metadata_proto) = proto.metadata {
            ValueMetadata::from_proto(metadata_proto)?
        } else {
            // Fallback for backward compatibility: create empty metadata
            ValueMetadata::new(
                ExecutionPayloadHeader {
                    block_hash: Default::default(),
                    parent_hash: Default::default(),
                    state_root: Default::default(),
                    receipts_root: Default::default(),
                    logs_bloom: Default::default(),
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: 0,
                    base_fee_per_gas: Default::default(),
                    extra_data: AlloyBytes::from(Vec::<u8>::new()),
                    transactions_root: Default::default(),
                    withdrawals_root: Default::default(),
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                    prev_randao: Default::default(),
                    fee_recipient: crate::address::Address::repeat_byte(0),
                    requests_hash: None,
                },
                vec![],
            )
        };

        #[allow(deprecated)]
        Ok(Value { value, metadata, extensions })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::Value {
            value: self.value,
            metadata: Some(self.metadata.to_proto()?),
            #[allow(deprecated)]
            extensions: self.extensions.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        address::Address,
        aliases::{B256, Bloom, Bytes, U256},
        constants::LOAD_EXECUTION_GAS_LIMIT,
    };

    fn create_test_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::from([1u8; 32]),
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            receipts_root: B256::from([4u8; 32]),
            logs_bloom: Bloom::from([5u8; 256]),
            block_number: 100,
            gas_limit: LOAD_EXECUTION_GAS_LIMIT,
            gas_used: LOAD_EXECUTION_GAS_LIMIT / 2,
            timestamp: 1234567890,
            base_fee_per_gas: U256::from(1000000000u64),
            extra_data: Bytes::from(vec![0u8; 4]),
            transactions_root: B256::from([7u8; 32]),
            withdrawals_root: B256::from([8u8; 32]),
            blob_gas_used: 262144,
            excess_blob_gas: 0,
            prev_randao: B256::from([6u8; 32]),
            fee_recipient: Address::new([7u8; 20]),
            requests_hash: None,
        }
    }

    fn create_test_commitments(count: usize) -> Vec<KzgCommitment> {
        (0..count)
            .map(|i| {
                let mut bytes = [0u8; 48];
                bytes[0] = i as u8;
                KzgCommitment(bytes)
            })
            .collect()
    }

    #[test]
    fn test_value_new() {
        let header = create_test_header();
        let commitments = create_test_commitments(3);
        let metadata = ValueMetadata::new(header, commitments);

        let value = Value::new(metadata.clone());

        assert_eq!(value.value, metadata.consensus_hash());
        assert_eq!(value.metadata, metadata);
        #[allow(deprecated)]
        {
            assert!(value.extensions.is_empty());
        }
    }

    #[test]
    fn test_value_accessors() {
        let header = create_test_header();
        let commitments = create_test_commitments(2);
        let metadata = ValueMetadata::new(header.clone(), commitments.clone());
        let value = Value::new(metadata);

        // Test blob_commitments accessor
        let retrieved_commitments = value.blob_commitments();
        assert_eq!(retrieved_commitments.len(), 2);
        assert_eq!(retrieved_commitments, &commitments[..]);

        // Test execution_payload_header accessor
        let retrieved_header = value.execution_payload_header();
        assert_eq!(retrieved_header.block_hash, header.block_hash);
        assert_eq!(retrieved_header.block_number, 100);
    }

    #[test]
    fn test_value_id() {
        let header = create_test_header();
        let metadata = ValueMetadata::new(header, vec![]);
        let value = Value::new(metadata);

        let id = value.id();
        assert_eq!(id.as_u64(), value.value);
    }

    #[test]
    fn test_value_size_bytes() {
        let header = create_test_header();
        let commitments = create_test_commitments(3);
        let metadata = ValueMetadata::new(header, commitments);
        let value = Value::new(metadata);

        let size = value.size_bytes();
        // Should be: u64 (8 bytes) + metadata size
        assert!(size > 8);
        assert!(size < 3000); // Should be under 3KB
    }

    #[test]
    fn test_value_protobuf_roundtrip() {
        let header = create_test_header();
        let commitments = create_test_commitments(4);
        let metadata = ValueMetadata::new(header, commitments);
        let original = Value::new(metadata);

        // Serialize to protobuf
        let proto = original.to_proto().expect("Failed to serialize");

        // Deserialize back
        let deserialized = Value::from_proto(proto).expect("Failed to deserialize");

        // Verify all fields match
        assert_eq!(deserialized.value, original.value);
        assert_eq!(deserialized.metadata, original.metadata);
        #[allow(deprecated)]
        {
            assert_eq!(deserialized.extensions, original.extensions);
        }
    }

    #[test]
    fn test_value_protobuf_backward_compatibility() {
        // Test that Value can deserialize from proto without metadata field
        let proto = proto::Value {
            value: 12345,
            metadata: None, // No metadata (old format)
            extensions: AlloyBytes::from(vec![1, 2, 3, 4]).into(),
        };

        let value = Value::from_proto(proto).expect("Failed to deserialize");

        // Should have empty metadata as fallback
        assert_eq!(value.value, 12345);
        assert_eq!(value.metadata.blob_count, 0);
        assert_eq!(value.metadata.total_blob_bytes, 0);
        #[allow(deprecated)]
        {
            assert_eq!(value.extensions.as_ref(), &[1, 2, 3, 4]);
        }
    }

    #[test]
    fn test_value_consensus_hash_consistency() {
        // Same metadata should produce same hash
        let header = create_test_header();
        let commitments = create_test_commitments(2);
        let metadata = ValueMetadata::new(header, commitments);

        let value1 = Value::new(metadata.clone());
        let value2 = Value::new(metadata);

        assert_eq!(value1.value, value2.value);
        assert_eq!(value1.id(), value2.id());
    }

    #[test]
    fn test_value_different_metadata_different_hash() {
        let header1 = create_test_header();
        let metadata1 = ValueMetadata::new(header1, vec![]);

        let mut header2 = create_test_header();
        header2.block_number = 101; // Different block number
        let metadata2 = ValueMetadata::new(header2, vec![]);

        let value1 = Value::new(metadata1);
        let value2 = Value::new(metadata2);

        assert_ne!(value1.value, value2.value);
        assert_ne!(value1.id(), value2.id());
    }

    #[test]
    fn test_value_with_multiple_blobs() {
        let header = create_test_header();
        let commitments = create_test_commitments(6); // Max blobs
        let metadata = ValueMetadata::new(header, commitments.clone());
        let value = Value::new(metadata);

        assert_eq!(value.blob_commitments().len(), 6);
        assert_eq!(value.metadata.blob_count, 6);
        assert_eq!(value.metadata.total_blob_bytes, 6 * 131_072);

        // Verify protobuf roundtrip with multiple blobs
        let proto = value.to_proto().expect("Failed to serialize");
        let deserialized = Value::from_proto(proto).expect("Failed to deserialize");

        assert_eq!(deserialized.blob_commitments().len(), 6);
        assert_eq!(deserialized.metadata.blob_count, 6);
    }

    #[test]
    fn test_value_implements_malachite_trait() {
        let header = create_test_header();
        let metadata = ValueMetadata::new(header, vec![]);
        let value = Value::new(metadata);

        // Should be able to call trait methods
        let id = value.id();
        assert_eq!(id.as_u64(), value.value);
    }

    #[test]
    fn test_value_ordering() {
        let header1 = create_test_header();
        let metadata1 = ValueMetadata::new(header1, vec![]);
        let value1 = Value::new(metadata1);

        let mut header2 = create_test_header();
        header2.block_number = 101;
        let metadata2 = ValueMetadata::new(header2, vec![]);
        let value2 = Value::new(metadata2);

        // Values should be orderable (required for Malachite)
        assert!(value1 != value2);
        // One should be less than the other (ordering is based on all fields)
        assert!(value1 < value2 || value2 < value1);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Invalid metadata passed to Value::new()")]
    fn test_value_new_validates_metadata_in_debug() {
        // Create metadata with inconsistent blob_count
        let header = create_test_header();
        let commitments = create_test_commitments(2);
        let mut metadata = ValueMetadata::new(header, commitments);

        // Corrupt the blob_count to make validation fail
        metadata.blob_count = 10; // Wrong! Should be 2

        // This should panic in debug builds due to debug_assert!
        let _value = Value::new(metadata);
    }

    #[test]
    fn test_value_new_with_valid_metadata() {
        // Verify that valid metadata passes validation
        let header = create_test_header();
        let commitments = create_test_commitments(3);
        let metadata = ValueMetadata::new(header, commitments);

        // Should not panic even in debug builds
        let value = Value::new(metadata.clone());

        assert_eq!(value.metadata, metadata);
        assert!(value.metadata.validate().is_ok());
    }
}
