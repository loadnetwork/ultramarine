//! BeaconBlockBody types for Ethereum Deneb compatibility (Phase 4)
//!
//! This module provides a minimal BeaconBlockBody structure that matches the Ethereum Deneb
//! specification for computing `body_root` in SignedBeaconBlockHeader.
//!
//! ## Purpose
//!
//! The `body_root` in BeaconBlockHeader is the SSZ tree_hash_root of the full BeaconBlockBody
//! structure. Even though Ultramarine doesn't use most Ethereum beacon chain fields (like
//! attestations, deposits, etc.), we must include all 12 fields to match the Deneb spec.
//!
//! ## CRITICAL SSZ Compliance Fixes
//!
//! ### Issue 1: Field Sizes MUST Match Deneb Spec
//! - Field 0 (randao_reveal): **MUST be 96 bytes** (BLS signature size)
//! - Even though Ultramarine uses Ed25519 (64 bytes), we use a 96-byte zero array
//! - This ensures SSZ merkleization matches what Ethereum tools expect
//!
//! ### Issue 2: Proper SSZ Merkleization
//! - **Each field MUST become a 32-byte leaf** before merkleization
//! - Basic types: serialize + right-pad to 32 bytes
//! - Types > 32 bytes: hash to 32 bytes
//! - Composite types: call tree_hash_root (returns 32 bytes)
//!
//! Without these fixes, the body_root diverges from Ethereum and breaks compatibility.
//!
//! ## References
//! - Ethereum spec: `consensus-specs/specs/deneb/beacon-chain.md#beaconblockbody`
//! - Lighthouse: `lighthouse/consensus/types/src/beacon_block_body.rs`
//! - SSZ spec: `consensus-specs/ssz/simple-serialize.md#merkleization`

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use ethereum_hashing::hash32_concat;
use serde::{Deserialize, Serialize};
use tree_hash::{BYTES_PER_CHUNK, Hash256, MerkleHasher, TreeHash};

use crate::{aliases::Bytes, blob::KzgCommitment, engine_api::ExecutionPayloadHeader};

/// Minimal Eth1Data for SSZ tree hashing
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eth1Data {
    pub deposit_root: B256,
    pub deposit_count: u64,
    pub block_hash: B256,
}

impl TreeHash for Eth1Data {
    fn tree_hash_type() -> tree_hash::TreeHashType {
        tree_hash::TreeHashType::Container
    }

    fn tree_hash_packed_encoding(&self) -> tree_hash::PackedEncoding {
        unreachable!("Eth1Data should never be packed")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("Eth1Data should never be packed")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let mut hasher = tree_hash::MerkleHasher::with_leaves(4);

        // Field 0: deposit_root (32 bytes) - already correct size
        hasher.write(self.deposit_root.as_slice()).expect("hash deposit_root");

        // Field 1: deposit_count (u64 = 8 bytes) - pad to 32 bytes
        // SSZ requires all leaves be 32 bytes
        let mut deposit_count_bytes = [0u8; 32];
        deposit_count_bytes[..8].copy_from_slice(&self.deposit_count.to_le_bytes());
        hasher.write(&deposit_count_bytes).expect("hash deposit_count");

        // Field 2: block_hash (32 bytes) - already correct size
        hasher.write(self.block_hash.as_slice()).expect("hash block_hash");

        hasher.finish().expect("finalize Eth1Data hash")
    }
}

/// Minimal SyncAggregate for SSZ tree hashing
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncAggregate {
    pub sync_committee_bits: [u8; 64],
    pub sync_committee_signature: [u8; 96],
}

impl Default for SyncAggregate {
    fn default() -> Self {
        Self { sync_committee_bits: [0u8; 64], sync_committee_signature: [0u8; 96] }
    }
}

impl Serialize for SyncAggregate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SyncAggregate", 2)?;
        state.serialize_field("sync_committee_bits", &self.sync_committee_bits[..])?;
        state.serialize_field("sync_committee_signature", &self.sync_committee_signature[..])?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SyncAggregate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use std::fmt;

        use serde::de::{self, Visitor};

        struct SyncAggregateVisitor;

        impl<'de> Visitor<'de> for SyncAggregateVisitor {
            type Value = SyncAggregate;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("struct SyncAggregate")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut bits = None;
                let mut sig = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "sync_committee_bits" => {
                            let vec: Vec<u8> = map.next_value()?;
                            if vec.len() != 64 {
                                return Err(de::Error::invalid_length(vec.len(), &"64"));
                            }
                            let mut arr = [0u8; 64];
                            arr.copy_from_slice(&vec);
                            bits = Some(arr);
                        }
                        "sync_committee_signature" => {
                            let vec: Vec<u8> = map.next_value()?;
                            if vec.len() != 96 {
                                return Err(de::Error::invalid_length(vec.len(), &"96"));
                            }
                            let mut arr = [0u8; 96];
                            arr.copy_from_slice(&vec);
                            sig = Some(arr);
                        }
                        _ => {
                            let _: de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(SyncAggregate {
                    sync_committee_bits: bits
                        .ok_or_else(|| de::Error::missing_field("sync_committee_bits"))?,
                    sync_committee_signature: sig
                        .ok_or_else(|| de::Error::missing_field("sync_committee_signature"))?,
                })
            }
        }

        deserializer.deserialize_struct(
            "SyncAggregate",
            &["sync_committee_bits", "sync_committee_signature"],
            SyncAggregateVisitor,
        )
    }
}

impl TreeHash for SyncAggregate {
    fn tree_hash_type() -> tree_hash::TreeHashType {
        tree_hash::TreeHashType::Container
    }

    fn tree_hash_packed_encoding(&self) -> tree_hash::PackedEncoding {
        unreachable!("SyncAggregate should never be packed")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("SyncAggregate should never be packed")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let mut hasher = tree_hash::MerkleHasher::with_leaves(2);

        // Field 0: sync_committee_bits (64 bytes) - chunk into 2x32 bytes and merkleize
        // SSZ requires byte arrays be split into 32-byte chunks (not hashed with Keccak)
        let bits_root = {
            let mut sub_hasher = tree_hash::MerkleHasher::with_leaves(2);
            // Chunk 0: bytes[0..32]
            sub_hasher.write(&self.sync_committee_bits[0..32]).expect("chunk 0");
            // Chunk 1: bytes[32..64]
            sub_hasher.write(&self.sync_committee_bits[32..64]).expect("chunk 1");
            sub_hasher.finish().expect("merkleize sync_committee_bits")
        };
        hasher.write(&bits_root.0).expect("hash sync_committee_bits");

        // Field 1: sync_committee_signature (96 bytes) - chunk into 3x32 bytes and merkleize
        let signature_root = {
            let mut sub_hasher = tree_hash::MerkleHasher::with_leaves(4); // 4 = next power of 2 after 3
            // Chunk 0: bytes[0..32]
            sub_hasher.write(&self.sync_committee_signature[0..32]).expect("chunk 0");
            // Chunk 1: bytes[32..64]
            sub_hasher.write(&self.sync_committee_signature[32..64]).expect("chunk 1");
            // Chunk 2: bytes[64..96]
            sub_hasher.write(&self.sync_committee_signature[64..96]).expect("chunk 2");
            sub_hasher.finish().expect("merkleize sync_committee_signature")
        };
        hasher.write(&signature_root.0).expect("hash sync_committee_signature");

        hasher.finish().expect("finalize SyncAggregate hash")
    }
}

/// Minimal BeaconBlockBody matching Ethereum Deneb specification
///
/// ## CRITICAL: Field 0 Size Fix
/// randao_reveal MUST be 96 bytes to match BLS signature in Deneb spec.
/// Ultramarine uses Ed25519 (64 bytes), but we use 96-byte placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeaconBlockBodyMinimal {
    /// Field 0: BLS signature (96 bytes) - CRITICAL: Must be 96 bytes
    pub randao_reveal: [u8; 96],
    pub eth1_data: Eth1Data,
    pub graffiti: [u8; 32],
    pub proposer_slashings: Vec<u8>,
    pub attester_slashings: Vec<u8>,
    pub attestations: Vec<u8>,
    pub deposits: Vec<u8>,
    pub voluntary_exits: Vec<u8>,
    pub sync_aggregate: SyncAggregate,
    /// Field 9: ExecutionPayload root (tree_hash_root of full header)
    pub execution_payload_root: B256,
    pub bls_to_execution_changes: Vec<u8>,
    /// Field 11: THE REAL DATA
    pub blob_kzg_commitments: Vec<KzgCommitment>,
}

impl BeaconBlockBodyMinimal {
    pub fn from_ultramarine_data(
        blob_commitments: Vec<KzgCommitment>,
        execution_payload_header: &ExecutionPayloadHeader,
    ) -> Self {
        let execution_payload_root =
            compute_execution_payload_header_root(execution_payload_header);

        Self {
            randao_reveal: [0u8; 96], // ✅ FIX: 96 bytes, not 64
            eth1_data: Eth1Data::default(),
            graffiti: [0u8; 32],
            proposer_slashings: vec![],
            attester_slashings: vec![],
            attestations: vec![],
            deposits: vec![],
            voluntary_exits: vec![],
            sync_aggregate: SyncAggregate::default(),
            execution_payload_root,
            bls_to_execution_changes: vec![],
            blob_kzg_commitments: blob_commitments,
        }
    }

    pub fn compute_body_root(&self) -> B256 {
        let root = self.tree_hash_root();
        B256::from_slice(root.as_ref())
    }
}

/// ✅ FIX: Proper SSZ Merkleization
/// Each field → 32-byte leaf BEFORE writing to hasher
impl TreeHash for BeaconBlockBodyMinimal {
    fn tree_hash_type() -> tree_hash::TreeHashType {
        tree_hash::TreeHashType::Container
    }

    fn tree_hash_packed_encoding(&self) -> tree_hash::PackedEncoding {
        unreachable!("BeaconBlockBodyMinimal should never be packed")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("BeaconBlockBodyMinimal should never be packed")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let mut hasher = tree_hash::MerkleHasher::with_leaves(16);

        // ⭐ CRITICAL SSZ FIX: Each field becomes a 32-byte leaf
        // > 32 bytes → chunk into 32-byte pieces and merkleize
        // < 32 bytes → pad to 32 bytes
        // = 32 bytes → use directly

        // Field 0: randao_reveal (96 bytes) → chunk into 3x32 bytes and merkleize
        // SSZ requires byte arrays be split into 32-byte chunks (not hashed with Keccak)
        let randao_root = {
            let mut sub_hasher = tree_hash::MerkleHasher::with_leaves(4); // 4 = next power of 2 after 3
            // Chunk 0: bytes[0..32]
            sub_hasher.write(&self.randao_reveal[0..32]).expect("chunk 0");
            // Chunk 1: bytes[32..64]
            sub_hasher.write(&self.randao_reveal[32..64]).expect("chunk 1");
            // Chunk 2: bytes[64..96]
            sub_hasher.write(&self.randao_reveal[64..96]).expect("chunk 2");
            sub_hasher.finish().expect("merkleize randao_reveal")
        };
        hasher.write(&randao_root.0).expect("hash randao_reveal");

        // Field 1: eth1_data (composite) → tree_hash_root returns 32 bytes
        hasher.write(&self.eth1_data.tree_hash_root().0).expect("hash eth1_data");

        // Field 2: graffiti (32 bytes) → already 32 bytes
        hasher.write(&self.graffiti).expect("hash graffiti");

        // Fields 3-7, 10: empty lists → list root (32 bytes)
        let empty_list_root = tree_hash_empty_list();
        hasher.write(&empty_list_root.0).expect("hash proposer_slashings");
        hasher.write(&empty_list_root.0).expect("hash attester_slashings");
        hasher.write(&empty_list_root.0).expect("hash attestations");
        hasher.write(&empty_list_root.0).expect("hash deposits");
        hasher.write(&empty_list_root.0).expect("hash voluntary_exits");

        // Field 8: sync_aggregate (composite) → tree_hash_root returns 32 bytes
        hasher.write(&self.sync_aggregate.tree_hash_root().0).expect("hash sync_aggregate");

        // Field 9: execution_payload_root (32 bytes) → already 32 bytes
        hasher.write(self.execution_payload_root.as_slice()).expect("hash execution_payload_root");

        // Field 10: bls_to_execution_changes (empty list)
        hasher.write(&empty_list_root.0).expect("hash bls_to_execution_changes");

        // Field 11: blob_kzg_commitments (list) → list root (32 bytes)
        let commitments_root = tree_hash_kzg_commitments(&self.blob_kzg_commitments);
        hasher.write(&commitments_root.0).expect("hash blob_kzg_commitments");

        hasher.finish().expect("finalize BeaconBlockBody hash")
    }
}

/// ✅ FIX: Proper SSZ merkleization for ExecutionPayloadHeader
fn compute_execution_payload_header_root(header: &ExecutionPayloadHeader) -> B256 {
    let mut hasher = tree_hash::MerkleHasher::with_leaves(16);

    // ⭐ CRITICAL: Each field → 32-byte leaf

    // parent_hash (32 bytes) → use directly
    hasher.write(header.parent_hash.as_slice()).expect("hash parent_hash");

    // fee_recipient (20 bytes) → pad to 32
    let mut fee_recipient_bytes = [0u8; 32];
    fee_recipient_bytes[..20].copy_from_slice(header.fee_recipient.to_alloy_address().as_slice());
    hasher.write(&fee_recipient_bytes).expect("hash fee_recipient");

    // state_root (32 bytes) → use directly
    hasher.write(header.state_root.as_slice()).expect("hash state_root");

    // receipts_root (32 bytes) → use directly
    hasher.write(header.receipts_root.as_slice()).expect("hash receipts_root");

    // logs_bloom (256 bytes) → hash to 32
    let bloom_root = tree_hash_fixed_vector(header.logs_bloom.as_slice()).expect("hash logs_bloom");
    hasher.write(bloom_root.as_slice()).expect("hash logs_bloom");

    // prev_randao (32 bytes) → use directly
    hasher.write(header.prev_randao.as_slice()).expect("hash prev_randao");

    // u64 fields → pad to 32 bytes (little-endian + zero-pad)
    let mut block_number_bytes = [0u8; 32];
    block_number_bytes[..8].copy_from_slice(&header.block_number.to_le_bytes());
    hasher.write(&block_number_bytes).expect("hash block_number");

    let mut gas_limit_bytes = [0u8; 32];
    gas_limit_bytes[..8].copy_from_slice(&header.gas_limit.to_le_bytes());
    hasher.write(&gas_limit_bytes).expect("hash gas_limit");

    let mut gas_used_bytes = [0u8; 32];
    gas_used_bytes[..8].copy_from_slice(&header.gas_used.to_le_bytes());
    hasher.write(&gas_used_bytes).expect("hash gas_used");

    let mut timestamp_bytes = [0u8; 32];
    timestamp_bytes[..8].copy_from_slice(&header.timestamp.to_le_bytes());
    hasher.write(&timestamp_bytes).expect("hash timestamp");

    // extra_data (ByteList[32]) → SSZ root
    let extra_data_root = tree_hash_variable_bytes_ssz(header.extra_data.as_ref())
        .expect("hash execution extra_data");
    hasher.write(extra_data_root.as_slice()).expect("hash extra_data");

    // base_fee_per_gas (U256, 32 bytes) → already 32 bytes
    let base_fee_bytes = header.base_fee_per_gas.to_le_bytes::<32>();
    hasher.write(&base_fee_bytes).expect("hash base_fee_per_gas");

    // block_hash (32 bytes) → use directly
    hasher.write(header.block_hash.as_slice()).expect("hash block_hash");

    // transactions_root (32 bytes)
    hasher.write(header.transactions_root.as_slice()).expect("hash transactions_root");

    // withdrawals_root (32 bytes)
    hasher.write(header.withdrawals_root.as_slice()).expect("hash withdrawals_root");

    // More u64 fields → pad to 32
    let mut blob_gas_used_bytes = [0u8; 32];
    blob_gas_used_bytes[..8].copy_from_slice(&header.blob_gas_used.to_le_bytes());
    hasher.write(&blob_gas_used_bytes).expect("hash blob_gas_used");

    let mut excess_blob_gas_bytes = [0u8; 32];
    excess_blob_gas_bytes[..8].copy_from_slice(&header.excess_blob_gas.to_le_bytes());
    hasher.write(&excess_blob_gas_bytes).expect("hash excess_blob_gas");

    let root = hasher.finish().expect("finalize ExecutionPayloadHeader hash");
    B256::from_slice(root.as_ref())
}

const MAX_BLOB_COMMITMENTS_PER_BLOCK: usize = 1024;

fn tree_hash_kzg_commitments(commitments: &[KzgCommitment]) -> Hash256 {
    if commitments.is_empty() {
        return tree_hash_empty_list();
    }

    let depth = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two().ilog2() as usize;
    let mut leaves: Vec<Hash256> = commitments.iter().map(|c| c.tree_hash_root()).collect();
    let capacity = 1 << depth;
    leaves.resize(capacity, Hash256::from_slice(&[0u8; 32]));

    let merkle_leaves: Vec<fixed_bytes::Hash256> =
        leaves.iter().map(|h| fixed_bytes::Hash256::from_slice(&h.0)).collect();

    let tree = merkle_proof::MerkleTree::create(&merkle_leaves, depth);
    let list_root = Hash256::from_slice(tree.hash().as_slice());
    mix_in_length(&list_root, commitments.len() as u64)
}

pub(crate) fn tree_hash_variable_bytes_ssz(data: &[u8]) -> Result<Hash256, String> {
    if data.is_empty() {
        return Ok(tree_hash_empty_list());
    }

    let chunk_count = (data.len() + BYTES_PER_CHUNK - 1) / BYTES_PER_CHUNK;
    let mut hasher = MerkleHasher::with_leaves(chunk_count);
    for chunk in data.chunks(BYTES_PER_CHUNK) {
        let mut padded = [0u8; BYTES_PER_CHUNK];
        padded[..chunk.len()].copy_from_slice(chunk);
        hasher.write(&padded).map_err(|e| format!("Failed to hash SSZ byte chunk: {:?}", e))?;
    }

    let root = hasher.finish().map_err(|e| format!("Failed to finalize SSZ byte hash: {:?}", e))?;
    Ok(mix_in_length(&root, data.len() as u64))
}

pub(crate) fn tree_hash_transactions_ssz(transactions: &[Bytes]) -> Result<Hash256, String> {
    if transactions.is_empty() {
        return Ok(tree_hash_empty_list());
    }

    let mut hasher = MerkleHasher::with_leaves(transactions.len());
    for tx in transactions {
        let tx_root = tree_hash_variable_bytes_ssz(tx.as_ref())
            .map_err(|e| format!("Failed to hash transaction bytes: {}", e))?;
        hasher
            .write(tx_root.as_slice())
            .map_err(|e| format!("Failed to hash transaction root: {:?}", e))?;
    }

    let root =
        hasher.finish().map_err(|e| format!("Failed to finalize transactions hash: {:?}", e))?;
    Ok(mix_in_length(&root, transactions.len() as u64))
}

pub(crate) fn tree_hash_withdrawals_ssz(withdrawals: &[Withdrawal]) -> Result<Hash256, String> {
    if withdrawals.is_empty() {
        return Ok(tree_hash_empty_list());
    }

    let mut hasher = MerkleHasher::with_leaves(withdrawals.len());
    for withdrawal in withdrawals {
        let leaf = tree_hash_withdrawal_ssz(withdrawal)?;
        hasher
            .write(leaf.as_slice())
            .map_err(|e| format!("Failed to hash withdrawal leaf: {:?}", e))?;
    }

    let root =
        hasher.finish().map_err(|e| format!("Failed to finalize withdrawals hash: {:?}", e))?;
    Ok(mix_in_length(&root, withdrawals.len() as u64))
}

fn tree_hash_withdrawal_ssz(withdrawal: &Withdrawal) -> Result<Hash256, String> {
    let mut hasher = MerkleHasher::with_leaves(4);
    hasher
        .write(&u64_to_bytes32(withdrawal.index))
        .map_err(|e| format!("Failed to hash withdrawal index: {:?}", e))?;
    hasher
        .write(&u64_to_bytes32(withdrawal.validator_index))
        .map_err(|e| format!("Failed to hash withdrawal validator index: {:?}", e))?;

    let mut address_bytes = [0u8; 32];
    address_bytes[..20].copy_from_slice(withdrawal.address.as_slice());
    hasher
        .write(&address_bytes)
        .map_err(|e| format!("Failed to hash withdrawal address: {:?}", e))?;

    hasher
        .write(&u64_to_bytes32(withdrawal.amount))
        .map_err(|e| format!("Failed to hash withdrawal amount: {:?}", e))?;

    hasher.finish().map_err(|e| format!("Failed to finalize withdrawal hash: {:?}", e))
}

fn tree_hash_fixed_vector(data: &[u8]) -> Result<Hash256, String> {
    if data.is_empty() {
        return Ok(Hash256::from_slice(&[0u8; 32]));
    }

    if data.len() % BYTES_PER_CHUNK != 0 {
        return Err(format!("Expected fixed vector length multiple of 32, got {}", data.len()));
    }

    let chunk_count = data.len() / BYTES_PER_CHUNK;
    let mut hasher = MerkleHasher::with_leaves(chunk_count);
    for chunk in data.chunks(BYTES_PER_CHUNK) {
        hasher.write(chunk).map_err(|e| format!("Failed to hash fixed vector chunk: {:?}", e))?;
    }
    hasher.finish().map_err(|e| format!("Failed to finalize fixed vector hash: {:?}", e))
}

fn u64_to_bytes32(value: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    bytes
}

fn tree_hash_empty_list() -> Hash256 {
    mix_in_length(&Hash256::from_slice(&[0u8; 32]), 0)
}

fn mix_in_length(root: &Hash256, length: u64) -> Hash256 {
    let mut length_bytes = [0u8; BYTES_PER_CHUNK];
    length_bytes[..8].copy_from_slice(&length.to_le_bytes());
    Hash256::from_slice(&hash32_concat(root.as_slice(), &length_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_randao_reveal_is_96_bytes() {
        let body = BeaconBlockBodyMinimal::from_ultramarine_data(
            vec![],
            &ExecutionPayloadHeader {
                block_hash: B256::ZERO,
                parent_hash: B256::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                block_number: 0,
                gas_limit: 0,
                gas_used: 0,
                timestamp: 0,
                base_fee_per_gas: Default::default(),
                blob_gas_used: 0,
                excess_blob_gas: 0,
                prev_randao: B256::ZERO,
                fee_recipient: crate::address::Address::repeat_byte(0),
                extra_data: Bytes::from(vec![]),
                transactions_root: B256::ZERO,
                withdrawals_root: B256::ZERO,
            },
        );

        // ✅ CRITICAL: Must be 96 bytes, not 64
        assert_eq!(body.randao_reveal.len(), 96);
        assert_eq!(body.randao_reveal, [0u8; 96]);
    }

    #[test]
    fn test_body_root_computes() {
        let body = BeaconBlockBodyMinimal {
            randao_reveal: [0u8; 96],
            eth1_data: Eth1Data::default(),
            graffiti: [0u8; 32],
            proposer_slashings: vec![],
            attester_slashings: vec![],
            attestations: vec![],
            deposits: vec![],
            voluntary_exits: vec![],
            sync_aggregate: SyncAggregate::default(),
            execution_payload_root: B256::ZERO,
            bls_to_execution_changes: vec![],
            blob_kzg_commitments: vec![],
        };

        let root = body.compute_body_root();
        assert_eq!(root.len(), 32);
    }
}
