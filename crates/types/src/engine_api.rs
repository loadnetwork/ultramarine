#![allow(missing_docs)]

use alloy_rpc_types::Withdrawal;
use alloy_rpc_types_engine::ExecutionPayloadV3;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{
    address::Address,
    aliases::{B256, BlockHash, BlockNumber, BlockTimestamp, Bloom, Bytes, U256},
    proto,
    // Phase 2: KzgCommitment will be used in ValueMetadata
    // blob::KzgCommitment,
};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRequestBody<'a> {
    pub jsonrpc: &'a str,
    pub method: &'a str,
    pub params: serde_json::Value,
    pub id: serde_json::Value,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonResponseBody {
    pub jsonrpc: String,
    #[serde(default)]
    pub error: Option<JsonError>,
    #[serde(default)]
    pub result: serde_json::Value,
    pub id: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonPayloadAttributes {
    #[serde(with = "serde_utils::u64_hex_be")]
    pub timestamp: BlockTimestamp,
    pub prev_randao: B256,
    // #[serde(with = "serde_utils::address_hex")]
    pub suggested_fee_recipient: Address,
    pub withdrawals: Vec<JsonWithdrawal>,
    pub parent_beacon_block_root: BlockHash,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonWithdrawal {
    pub index: u64,
    pub validator_index: u64,
    pub address: Address,
    pub amount: u64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonExecutionPayloadV3 {
    pub parent_hash: B256,
    // #[serde(with = "serde_utils::address_hex")]
    pub fee_recipient: Address,
    pub state_root: B256,
    pub receipts_root: B256,
    // #[serde(with = "serde_logs_bloom")]
    pub logs_bloom: Bloom,
    pub prev_randao: B256,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub block_number: BlockNumber,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub gas_used: u64,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub timestamp: BlockTimestamp,
    pub extra_data: Bytes,
    #[serde(with = "serde_utils::u256_hex_be")]
    pub base_fee_per_gas: U256,
    pub block_hash: B256,
    // #[serde(with = "ssz_types::serde_utils::list_of_hex_var_list")]
    pub transactions: Vec<Bytes>,
    pub withdrawals: Vec<Withdrawal>,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub blob_gas_used: u64,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub excess_blob_gas: u64,
}

impl From<ExecutionPayloadV3> for JsonExecutionPayloadV3 {
    fn from(payload: ExecutionPayloadV3) -> Self {
        let v2 = payload.payload_inner;
        let v1 = v2.payload_inner;
        JsonExecutionPayloadV3 {
            parent_hash: v1.parent_hash,
            fee_recipient: v1.fee_recipient.into(),
            state_root: v1.state_root,
            receipts_root: v1.receipts_root,
            logs_bloom: v1.logs_bloom,
            prev_randao: v1.prev_randao,
            block_number: v1.block_number,
            gas_limit: v1.gas_limit,
            gas_used: v1.gas_used,
            timestamp: v1.timestamp,
            extra_data: v1.extra_data,
            base_fee_per_gas: v1.base_fee_per_gas,
            block_hash: v1.block_hash,
            transactions: v1.transactions,
            withdrawals: v2.withdrawals.into_iter().collect::<Vec<_>>(),
            blob_gas_used: payload.blob_gas_used,
            excess_blob_gas: payload.excess_blob_gas,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionBlock {
    #[serde(rename = "hash")]
    pub block_hash: BlockHash,

    #[serde(rename = "number", with = "serde_utils::u64_hex_be")]
    pub block_number: BlockNumber,

    pub parent_hash: BlockHash,

    #[serde(with = "serde_utils::u64_hex_be")]
    pub timestamp: BlockTimestamp,

    #[serde(rename = "mixHash")]
    pub prev_randao: B256,
}

/// Lightweight execution payload header (Phase 1 - EIP-4844 integration)
///
/// This contains only the essential metadata from an execution payload,
/// without the full transaction list or blob data. Used in `ValueMetadata`
/// to keep consensus messages small (~2KB instead of potentially MBs).
///
/// ## Purpose
///
/// In Phase 2, consensus will vote on `ValueMetadata` which contains:
/// - This `ExecutionPayloadHeader` (lightweight block metadata)
/// - `Vec<KzgCommitment>` (48 bytes each, max 6-9 blobs)
///
/// The full execution payload and blobs are streamed separately via
/// `ProposalPart::Data` and `ProposalPart::BlobSidecar`.
///
/// ## Extraction
///
/// Created from `ExecutionPayloadV3` via:
/// ```rust,ignore
/// let header = ExecutionPayloadHeader::from_payload(&payload);
/// ```
///
/// ## Size Estimate
///
/// ```text
/// - block_hash:        32 bytes
/// - parent_hash:       32 bytes
/// - state_root:        32 bytes
/// - receipts_root:     32 bytes
/// - logs_bloom:        256 bytes
/// - block_number:      8 bytes
/// - gas_limit:         8 bytes
/// - gas_used:          8 bytes
/// - timestamp:         8 bytes
/// - base_fee_per_gas:  32 bytes
/// - blob_gas_used:     8 bytes
/// - excess_blob_gas:   8 bytes
/// - prev_randao:       32 bytes
/// - fee_recipient:     20 bytes
/// ─────────────────────────────
/// Total: ~516 bytes (vs. full payload with txs = 100KB+)
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecutionPayloadHeader {
    /// Block hash (keccak256 of RLP-encoded header)
    pub block_hash: BlockHash,

    /// Parent block hash
    pub parent_hash: BlockHash,

    /// State root after executing this block
    pub state_root: B256,

    /// Receipts root (Merkle root of transaction receipts)
    pub receipts_root: B256,

    /// Bloom filter for quick log searches
    pub logs_bloom: Bloom,

    /// Block number
    pub block_number: BlockNumber,

    /// Gas limit for this block
    pub gas_limit: u64,

    /// Gas used by all transactions
    pub gas_used: u64,

    /// Block timestamp (Unix seconds)
    pub timestamp: BlockTimestamp,

    /// Base fee per gas (EIP-1559)
    pub base_fee_per_gas: U256,

    /// Blob gas used (EIP-4844)
    pub blob_gas_used: u64,

    /// Excess blob gas for fee calculation (EIP-4844)
    pub excess_blob_gas: u64,

    /// Previous RANDAO value (PoS randomness)
    pub prev_randao: B256,

    /// Address receiving transaction fees
    pub fee_recipient: Address,
}

impl ExecutionPayloadHeader {
    /// Extract a header from a full `ExecutionPayloadV3`
    ///
    /// This creates the lightweight header that will be included in `ValueMetadata`
    /// for consensus voting. The full payload with transactions is streamed separately.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let (payload, blobs_bundle) = execution_client.generate_block_with_blobs(...).await?;
    ///
    /// // Extract lightweight header for consensus
    /// let header = ExecutionPayloadHeader::from_payload(&payload);
    ///
    /// // Create metadata for consensus voting
    /// let metadata = ValueMetadata::new(header, blobs_bundle.commitments);
    /// ```
    pub fn from_payload(payload: &ExecutionPayloadV3) -> Self {
        let inner = &payload.payload_inner.payload_inner;

        Self {
            block_hash: inner.block_hash,
            parent_hash: inner.parent_hash,
            state_root: inner.state_root,
            receipts_root: inner.receipts_root,
            logs_bloom: inner.logs_bloom,
            block_number: inner.block_number,
            gas_limit: inner.gas_limit,
            gas_used: inner.gas_used,
            timestamp: inner.timestamp,
            base_fee_per_gas: inner.base_fee_per_gas,
            blob_gas_used: payload.blob_gas_used,
            excess_blob_gas: payload.excess_blob_gas,
            prev_randao: inner.prev_randao,
            fee_recipient: inner.fee_recipient.into(),
        }
    }

    /// Calculate approximate size in bytes
    ///
    /// Useful for metrics and ensuring `ValueMetadata` stays under target size.
    pub fn size_bytes(&self) -> usize {
        32 + // block_hash
        32 + // parent_hash
        32 + // state_root
        32 + // receipts_root
        256 + // logs_bloom
        8 + // block_number
        8 + // gas_limit
        8 + // gas_used
        8 + // timestamp
        32 + // base_fee_per_gas
        8 + // blob_gas_used
        8 + // excess_blob_gas
        32 + // prev_randao
        20 // fee_recipient
        // Total: 516 bytes
    }
}

/// Protobuf conversion for ExecutionPayloadHeader (Phase 2)
///
/// This enables serialization/deserialization for consensus messages.
impl Protobuf for ExecutionPayloadHeader {
    type Proto = proto::ExecutionPayloadHeader;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        // Helper to convert bytes to B256
        fn bytes_to_b256(bytes: &[u8]) -> Result<B256, ProtoError> {
            if bytes.len() != 32 {
                return Err(ProtoError::Other(format!("Expected 32 bytes, got {}", bytes.len())));
            }
            Ok(B256::from_slice(bytes))
        }

        // Helper to convert bytes to Bloom
        fn bytes_to_bloom(bytes: &[u8]) -> Result<Bloom, ProtoError> {
            if bytes.len() != 256 {
                return Err(ProtoError::Other(format!(
                    "Expected 256 bytes for bloom, got {}",
                    bytes.len()
                )));
            }
            let mut array = [0u8; 256];
            array.copy_from_slice(bytes);
            Ok(Bloom::from(array))
        }

        // Helper to convert bytes to U256
        fn bytes_to_u256(bytes: &[u8]) -> Result<U256, ProtoError> {
            if bytes.len() > 32 {
                return Err(ProtoError::Other(format!("U256 bytes too long: {}", bytes.len())));
            }
            Ok(U256::try_from_le_slice(bytes).unwrap_or_default())
        }

        Ok(Self {
            block_hash: bytes_to_b256(&proto.block_hash)?,
            parent_hash: bytes_to_b256(&proto.parent_hash)?,
            state_root: bytes_to_b256(&proto.state_root)?,
            receipts_root: bytes_to_b256(&proto.receipts_root)?,
            logs_bloom: bytes_to_bloom(&proto.logs_bloom)?,
            block_number: proto.block_number,
            gas_limit: proto.gas_limit,
            gas_used: proto.gas_used,
            timestamp: proto.timestamp,
            base_fee_per_gas: bytes_to_u256(&proto.base_fee_per_gas)?,
            blob_gas_used: proto.blob_gas_used,
            excess_blob_gas: proto.excess_blob_gas,
            prev_randao: bytes_to_b256(&proto.prev_randao)?,
            fee_recipient: Address::from_proto(proto::Address { value: proto.fee_recipient })?,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::ExecutionPayloadHeader {
            block_hash: self.block_hash.0.to_vec().into(),
            parent_hash: self.parent_hash.0.to_vec().into(),
            state_root: self.state_root.0.to_vec().into(),
            receipts_root: self.receipts_root.0.to_vec().into(),
            logs_bloom: self.logs_bloom.0.to_vec().into(),
            block_number: self.block_number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            base_fee_per_gas: self.base_fee_per_gas.to_le_bytes::<32>().to_vec().into(),
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
            prev_randao: self.prev_randao.0.to_vec().into(),
            fee_recipient: self.fee_recipient.to_proto()?.value,
        })
    }
}
