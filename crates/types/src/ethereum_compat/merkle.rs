//! Merkle proof utilities for blob KZG commitment inclusion proofs
#![allow(dead_code)]
//! This module provides functions to generate and verify Merkle inclusion proofs
//! that prove a KZG commitment is included in a beacon block body according to
//! the Ethereum Deneb specification.
//!
//! ## Proof Structure (17 levels)
//!
//! ```text
//! KZG Commitment (leaf)
//!   ↓ [proof branches 0..N]
//! blob_kzg_commitments list root
//!   ↓ [proof branches N..17]
//! BeaconBlockBody root (body_root in header)
//! ```
//!
//! The proof has exactly 17 branches:
//! - First N branches: Prove commitment is in blob_kzg_commitments list
//! - Remaining 17-N branches: Prove list root is in BeaconBlockBody
//!
//! ## References
//! - Deneb spec: `consensus-specs/specs/deneb/p2p-interface.md`
//! - Lighthouse: `lighthouse/consensus/types/src/beacon_block_body.rs`

use alloy_primitives::B256;
use ethereum_hashing::hash32_concat;
use merkle_proof::MerkleTree;
use tree_hash::{BYTES_PER_CHUNK, Hash256, MerkleHasher, TreeHash, mix_in_length};

use crate::{
    blob::{KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK},
    ethereum_compat::BeaconBlockBodyMinimal,
};

type FixedHash = fixed_bytes::Hash256;

fn tree_hash_to_fixed(hash: Hash256) -> FixedHash {
    FixedHash::from_slice(hash.as_ref())
}

fn fixed_to_tree_hash(hash: &FixedHash) -> Hash256 {
    Hash256::from_slice(hash.as_slice())
}

fn fixed_to_b256(hash: &FixedHash) -> B256 {
    B256::from_slice(hash.as_slice())
}

fn b256_to_fixed(value: &B256) -> FixedHash {
    FixedHash::from_slice(value.as_slice())
}

fn merkle_root_from_branch(
    mut value: FixedHash,
    branch: &[FixedHash],
    mut index: usize,
) -> FixedHash {
    for sibling in branch {
        if index & 1 == 1 {
            value = FixedHash::from_slice(&hash32_concat(sibling.as_slice(), value.as_slice()));
        } else {
            value = FixedHash::from_slice(&hash32_concat(value.as_slice(), sibling.as_slice()));
        }
        index >>= 1;
    }
    value
}

fn commitments_subtree_proof(
    commitments: &[KzgCommitment],
    index: usize,
) -> Result<(Vec<FixedHash>, FixedHash), String> {
    let capacity = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two();
    let depth = capacity.ilog2() as usize;

    if commitments.len() > capacity {
        return Err(format!("Too many commitments: got {}, max {}", commitments.len(), capacity));
    }

    let mut leaves = vec![FixedHash::default(); capacity];
    for (position, commitment) in commitments.iter().enumerate() {
        leaves[position] = tree_hash_to_fixed(TreeHash::tree_hash_root(commitment));
    }

    let tree = MerkleTree::create(&leaves, depth);
    let (_, mut proof) = tree
        .generate_proof(index, depth)
        .map_err(|err| format!("Failed to create commitment proof: {err:?}"))?;

    // SSZ lists mix in their length as an additional branch
    let mut length_bytes = [0u8; BYTES_PER_CHUNK];
    length_bytes[..std::mem::size_of::<usize>()].copy_from_slice(&commitments.len().to_le_bytes());
    proof.push(FixedHash::from_slice(&length_bytes));

    debug_assert_eq!(proof.len(), depth + 1);

    let list_root = fixed_to_tree_hash(&tree.hash());
    let mixed_root = mix_in_length(&list_root, commitments.len());

    Ok((proof, tree_hash_to_fixed(mixed_root)))
}

fn body_subtree_proof_with_body(
    list_root: FixedHash,
    body: &BeaconBlockBodyMinimal,
) -> Result<Vec<FixedHash>, String> {
    let leaves = body_merkle_leaves(body, list_root)?;
    let depth = leaves.len().next_power_of_two().ilog2() as usize;
    let tree = MerkleTree::create(&leaves, depth);
    let (_, proof) = tree
        .generate_proof(BLOB_KZG_COMMITMENTS_INDEX, depth)
        .map_err(|err| format!("Failed to create body proof: {err:?}"))?;
    Ok(proof)
}

fn body_merkle_leaves(
    body: &BeaconBlockBodyMinimal,
    commitments_root: FixedHash,
) -> Result<Vec<FixedHash>, String> {
    let mut leaves = Vec::with_capacity(NUM_BEACON_BLOCK_BODY_HASH_TREE_ROOT_LEAVES);

    // Field 0: randao_reveal (96 bytes)
    let randao_root = {
        let mut sub_hasher = MerkleHasher::with_leaves(4);
        sub_hasher
            .write(&body.randao_reveal[0..32])
            .map_err(|e| format!("Failed to hash randao chunk 0: {e:?}"))?;
        sub_hasher
            .write(&body.randao_reveal[32..64])
            .map_err(|e| format!("Failed to hash randao chunk 1: {e:?}"))?;
        sub_hasher
            .write(&body.randao_reveal[64..96])
            .map_err(|e| format!("Failed to hash randao chunk 2: {e:?}"))?;
        sub_hasher.finish().map_err(|e| format!("Failed to finalize randao_reveal hash: {e:?}"))?
    };
    leaves.push(tree_hash_to_fixed(randao_root));

    // Field 1: eth1_data
    leaves.push(tree_hash_to_fixed(body.eth1_data.tree_hash_root()));

    // Field 2: graffiti (32 bytes)
    leaves.push(FixedHash::from_slice(&body.graffiti));

    // Pre-compute empty list root
    let empty_list_root = tree_hash_to_fixed(mix_in_length(&Hash256::default(), 0));

    // Fields 3-7: proposer_slashings, attester_slashings, attestations, deposits, voluntary_exits
    leaves.push(empty_list_root);
    leaves.push(empty_list_root);
    leaves.push(empty_list_root);
    leaves.push(empty_list_root);
    leaves.push(empty_list_root);

    // Field 8: sync_aggregate
    leaves.push(tree_hash_to_fixed(body.sync_aggregate.tree_hash_root()));

    // Field 9: execution_payload_root
    leaves.push(b256_to_fixed(&body.execution_payload_root));

    // Field 10: bls_to_execution_changes
    leaves.push(empty_list_root);

    // Field 11: blob_kzg_commitments list root (with length mix-in already applied)
    leaves.push(commitments_root);

    // Pad remaining leaves with zero as per SSZ container rules
    while leaves.len() < NUM_BEACON_BLOCK_BODY_HASH_TREE_ROOT_LEAVES {
        leaves.push(FixedHash::default());
    }

    Ok(leaves)
}

/// Number of fields in `BeaconBlockBody` used for SSZ merkleization.
const NUM_BEACON_BLOCK_BODY_HASH_TREE_ROOT_LEAVES: usize = 16;

/// Index of `blob_kzg_commitments` within the `BeaconBlockBody` SSZ container.
const BLOB_KZG_COMMITMENTS_INDEX: usize = 11;

/// Depth of the full inclusion proof defined by the Deneb specification.
const KZG_COMMITMENT_INCLUSION_PROOF_DEPTH: usize = 17;

/// Generate a Merkle inclusion proof for a KZG commitment
///
/// This proves that `commitments[index]` is included in the BeaconBlockBody
/// represented by `body_root`.
///
/// ## Algorithm
///
/// 1. Build Merkle tree for the commitments list
/// 2. Generate proof from commitment[index] to list root (variable depth)
/// 3. Generate proof from list root to body_root (fixed depth)
/// 4. Concatenate both proofs → total 17 branches
///
/// ## Parameters
///
/// - `commitments`: Full list of blob KZG commitments from the block
/// - `index`: Index of the commitment to prove (`0 <= index < commitments.len()`)
///
/// ## Returns
///
/// A vector of 17 B256 hashes representing the Merkle proof branches.
///
/// ## Errors
///
/// Returns `Err` if:
/// - Index is out of bounds
/// - Commitments list is empty
/// - Merkle tree generation fails
///
/// ## Example
///
/// ```ignore
/// let commitments = vec![commitment0, commitment1, commitment2];
/// let body = BeaconBlockBodyMinimal::from_ultramarine_data(commitments.clone(), &header);
/// let proof = generate_kzg_commitment_inclusion_proof(&commitments, 1, &body)?;
/// assert_eq!(proof.len(), 17);
/// ```
pub fn generate_kzg_commitment_inclusion_proof(
    commitments: &[KzgCommitment],
    index: usize,
    body: &BeaconBlockBodyMinimal,
) -> Result<Vec<B256>, String> {
    if commitments.is_empty() {
        return Err("Commitments list is empty".to_string());
    }

    if index >= commitments.len() {
        return Err(format!("Index {} out of bounds for {} commitments", index, commitments.len()));
    }

    let (list_proof, list_root) = commitments_subtree_proof(commitments, index)?;
    let body_proof = body_subtree_proof_with_body(list_root, body)?;

    debug_assert_eq!(list_proof.len() + body_proof.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);

    let mut result = Vec::with_capacity(KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);
    result.extend(list_proof.into_iter().map(|hash| fixed_to_b256(&hash)));
    result.extend(body_proof.into_iter().map(|hash| fixed_to_b256(&hash)));

    debug_assert_eq!(result.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);

    Ok(result)
}

/// Verify a KZG commitment inclusion proof
///
/// This verifies that a commitment is included in a BeaconBlockBody with the
/// given `body_root`, using the provided Merkle proof.
///
/// ## Parameters
///
/// - `commitment`: The KZG commitment to verify
/// - `proof`: The 17-branch Merkle inclusion proof
/// - `index`: The claimed index of the commitment (0-based)
/// - `body_root`: The body_root from BeaconBlockHeader
///
/// ## Returns
///
/// `true` if the proof is valid, `false` otherwise.
///
/// ## Example
///
/// ```ignore
/// let valid = verify_kzg_commitment_inclusion_proof(
///     &commitment,
///     &proof,
///     1,
///     body_root
/// );
/// assert!(valid);
/// ```
pub fn verify_kzg_commitment_inclusion_proof(
    commitment: &KzgCommitment,
    proof: &[B256],
    index: usize,
    body_root: B256,
) -> bool {
    let commitments_depth = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two().ilog2() as usize;
    let commitments_branch_length = commitments_depth + 1; // includes length mix-in

    if proof.len() != KZG_COMMITMENT_INCLUSION_PROOF_DEPTH ||
        commitments_branch_length > KZG_COMMITMENT_INCLUSION_PROOF_DEPTH ||
        index >= MAX_BLOB_COMMITMENTS_PER_BLOCK
    {
        return false;
    }

    let (commitment_branch, body_branch) = proof.split_at(commitments_branch_length);
    let commitment_branch_fixed: Vec<_> = commitment_branch.iter().map(b256_to_fixed).collect();
    let body_branch_fixed: Vec<_> = body_branch.iter().map(b256_to_fixed).collect();

    if commitment_branch_fixed.len() != commitments_branch_length ||
        commitment_branch_fixed.len() + body_branch_fixed.len() !=
            KZG_COMMITMENT_INCLUSION_PROOF_DEPTH
    {
        return false;
    }

    let leaf = tree_hash_to_fixed(TreeHash::tree_hash_root(commitment));
    let commitments_root = merkle_root_from_branch(leaf, &commitment_branch_fixed, index);

    let reconstructed_body_root =
        merkle_root_from_branch(commitments_root, &body_branch_fixed, BLOB_KZG_COMMITMENTS_INDEX);

    reconstructed_body_root == b256_to_fixed(&body_root)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bloom, U256};

    use super::*;
    use crate::{address::Address, aliases::Bytes, engine_api::ExecutionPayloadHeader};

    fn create_test_commitment(value: u8) -> KzgCommitment {
        let mut bytes = [0u8; 48];
        bytes[0] = value;
        KzgCommitment::from_slice(&bytes).unwrap()
    }

    fn test_execution_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::ZERO,
            parent_hash: B256::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            block_number: 0,
            gas_limit: 0,
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
        }
    }

    fn make_test_body(commitments: &[KzgCommitment]) -> BeaconBlockBodyMinimal {
        BeaconBlockBodyMinimal::from_ultramarine_data(
            commitments.to_vec(),
            &test_execution_header(),
        )
    }

    #[test]
    fn test_generate_proof_single_commitment() {
        let commitments = vec![create_test_commitment(1)];
        let body = make_test_body(&commitments);
        let proof = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body).unwrap();
        assert_eq!(proof.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);

        let body_root = body.compute_body_root();
        assert!(verify_kzg_commitment_inclusion_proof(&commitments[0], &proof, 0, body_root));
    }

    #[test]
    fn test_generate_proof_multiple_commitments() {
        let commitments =
            vec![create_test_commitment(1), create_test_commitment(2), create_test_commitment(3)];
        let body = make_test_body(&commitments);
        let body_root = body.compute_body_root();
        for index in 0..3 {
            let proof =
                generate_kzg_commitment_inclusion_proof(&commitments, index, &body).unwrap();
            assert_eq!(proof.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);
            assert!(verify_kzg_commitment_inclusion_proof(
                &commitments[index],
                &proof,
                index,
                body_root
            ));
        }
    }

    #[test]
    fn test_generate_proof_empty_commitments() {
        let commitments = vec![];
        let body = make_test_body(&commitments);
        let proof = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body);

        assert!(proof.is_err());
        assert!(proof.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_generate_proof_index_out_of_bounds() {
        let commitments = vec![create_test_commitment(1)];
        let body = make_test_body(&commitments);
        let proof = generate_kzg_commitment_inclusion_proof(&commitments, 1, &body);

        assert!(proof.is_err());
        assert!(proof.unwrap_err().contains("out of bounds"));
    }

    #[test]
    fn test_verify_proof_rejects_wrong_length() {
        let commitment = create_test_commitment(1);
        let commitments = vec![commitment];
        let body = make_test_body(&commitments);
        let body_root = body.compute_body_root();
        let short_proof = vec![B256::ZERO; 10]; // Wrong length

        let valid =
            verify_kzg_commitment_inclusion_proof(&commitments[0], &short_proof, 0, body_root);
        assert!(!valid, "Should reject proof with wrong length");
    }

    #[test]
    fn test_proof_deterministic() {
        let commitments = vec![create_test_commitment(1), create_test_commitment(2)];
        let body = make_test_body(&commitments);

        let proof1 = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body).unwrap();
        let proof2 = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body).unwrap();

        assert_eq!(proof1, proof2, "Same inputs should produce same proof");

        let body_root = body.compute_body_root();
        assert!(verify_kzg_commitment_inclusion_proof(&commitments[0], &proof1, 0, body_root));
    }

    #[test]
    fn test_proof_different_for_different_indices() {
        let commitments = vec![create_test_commitment(1), create_test_commitment(2)];

        let body = make_test_body(&commitments);
        let body_root = body.compute_body_root();
        let proof0 = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body).unwrap();
        let proof1 = generate_kzg_commitment_inclusion_proof(&commitments, 1, &body).unwrap();

        assert!(verify_kzg_commitment_inclusion_proof(&commitments[0], &proof0, 0, body_root));
        assert!(verify_kzg_commitment_inclusion_proof(&commitments[1], &proof1, 1, body_root));

        assert_ne!(proof0, proof1, "Different indices should produce different proofs");
    }

    #[test]
    fn test_proof_at_max_capacity() {
        // Test with maximum blob count to ensure tree capacity works correctly
        // Create unique commitments so proofs will differ
        let max_commitments: Vec<KzgCommitment> =
            (0..1024).map(|i| create_test_commitment((i % 256) as u8)).collect();
        let body = make_test_body(&max_commitments);
        let body_root = body.compute_body_root();

        // Test first blob
        let proof_0 = generate_kzg_commitment_inclusion_proof(&max_commitments, 0, &body).unwrap();
        assert_eq!(proof_0.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);
        assert!(verify_kzg_commitment_inclusion_proof(&max_commitments[0], &proof_0, 0, body_root));

        // Test last blob
        let proof_last =
            generate_kzg_commitment_inclusion_proof(&max_commitments, 1023, &body).unwrap();
        assert_eq!(proof_last.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);
        assert!(verify_kzg_commitment_inclusion_proof(
            &max_commitments[1023],
            &proof_last,
            1023,
            body_root
        ));

        // Proofs should be different for different indices
        assert_ne!(proof_0, proof_last);
    }

    #[test]
    fn test_with_known_test_vector() {
        // Test vector generated from Ultramarine implementation
        // This ensures our proof generation/verification is deterministic and correct
        let commitment = KzgCommitment::from_slice(&hex::decode("020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").unwrap()).unwrap();
        let index = 1;
        let proof_hex = vec![
            "16abab341fb7f370e27e4dadcf81766dd0dfd0ae64469477bb2cf6614938b2af",
            "e6acdcb1d2161de9b249e4ef9aeab58d5646cab44ce7db19b904d972eb471860",
            "db56114e00fdd4c1f85c892bf35ac9a89289aaecb1ebd0a96cde606a748b5d71",
            "c78009fdf07fc56a11f122370658a353aaa542ed63e44c4bc15ff4cd105ab33c",
            "536d98837f2dd165a55d5eeae91485954472d56f246df256bf3cae19352a123c",
            "9efde052aa15429fae05bad4d0b1d7c64da64d03d7a1854a588c2cb8430c0d30",
            "d88ddfeed400a8755596b21942c1497e114c302e6118290f91e6772976041fa1",
            "87eb0ddba57e35f6d286673802a4af5975e22506c7cf4c64bb6be5ee11527f2c",
            "26846476fd5fc54a5d43385167c95144f2643f533cc85bb9d16b782f8d7db193",
            "506d86582d252405b840018792cad2bf1259f1ef5aa5f887e13cb2f0094f51e1",
            "ffff0ad7e659772f9534c195c815efc4014ef1e1daed4404c06385d11192e92b",
            "6cf04127db05441cd833107a52be852868890e4317e6a02ab47683aa75964220",
            "0300000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b",
            "db56114e00fdd4c1f85c892bf35ac9a89289aaecb1ebd0a96cde606a748b5d71",
            "c78009fdf07fc56a11f122370658a353aaa542ed63e44c4bc15ff4cd105ab33c",
        ];
        let proof: Vec<B256> =
            proof_hex.into_iter().map(|h| B256::from_slice(&hex::decode(h).unwrap())).collect();
        let body_root = B256::from_slice(
            &hex::decode("03b402fc7a71579536d03a7c01498d3e35d9487ce9be52e07f9e2db7f48ddded")
                .unwrap(),
        );

        // Verify the known-good proof
        assert!(
            verify_kzg_commitment_inclusion_proof(&commitment, &proof, index, body_root),
            "Failed to validate known test vector"
        );

        // Also verify proof length
        assert_eq!(proof.len(), KZG_COMMITMENT_INCLUSION_PROOF_DEPTH);
    }

    /// Regression test: Proof generation must use actual body structure
    ///
    /// This test catches a critical bug where proof generation used only the commitments
    /// list without incorporating other BeaconBlockBody fields (randao_reveal, eth1_data,
    /// execution_payload_root, etc.) into the Merkle tree.
    ///
    /// ## The Bug
    ///
    /// Before the fix:
    /// - Proof generation: Built tree from commitments only → Wrong body_root
    /// - Verification: Expected body_root that includes all body fields
    /// - Result: body_root mismatch, proof verification fails
    ///
    /// ## The Fix
    ///
    /// After the fix (merkle.rs:219):
    /// - Proof generation: Takes full BeaconBlockBody, builds 16-leaf SSZ tree
    /// - All body fields (randao, eth1_data, execution_payload, etc.) are included
    /// - Proof is tied to the specific body structure
    /// - Verification correctly rejects if body fields differ
    ///
    /// ## This Test
    ///
    /// Creates two bodies with:
    /// - **Same commitments** (so old broken code would generate "same" proof)
    /// - **Different execution headers** (different gas_limit, timestamp, etc.)
    /// - Different execution headers → different execution_payload_root → different body_root
    ///
    /// Generates proof from body_a, then verifies:
    /// - ❌ Verification FAILS against body_b's root (different structure)
    /// - ✅ Verification SUCCEEDS against body_a's root (matching structure)
    ///
    /// Old broken code would PASS both verifications (bug).
    /// Fixed code FAILS body_b verification (correct behavior).
    #[test]
    fn test_proof_rejects_mismatched_body_structure() {
        let commitments = vec![create_test_commitment(1), create_test_commitment(2)];

        // Body A: Uses default execution header (gas_limit=0, timestamp=0, etc.)
        let header_a = test_execution_header();
        let body_a = BeaconBlockBodyMinimal::from_ultramarine_data(commitments.clone(), &header_a);

        // Body B: Uses DIFFERENT execution header (different gas_limit, timestamp, etc.)
        let mut header_b = test_execution_header();
        header_b.gas_limit = 30_000_000; // Different from body_a
        header_b.timestamp = 1234567890; // Different from body_a
        header_b.block_number = 100; // Different from body_a
        let body_b = BeaconBlockBodyMinimal::from_ultramarine_data(commitments.clone(), &header_b);

        // Verify bodies have different roots (execution_payload_root differs)
        let body_root_a = body_a.compute_body_root();
        let body_root_b = body_b.compute_body_root();
        assert_ne!(
            body_root_a, body_root_b,
            "Bodies with different execution headers must have different body_roots"
        );

        // Generate proof using body_a
        let proof = generate_kzg_commitment_inclusion_proof(&commitments, 0, &body_a)
            .expect("proof generation failed");

        // ✅ Verification SUCCEEDS against body_a's root (matching body structure)
        assert!(
            verify_kzg_commitment_inclusion_proof(&commitments[0], &proof, 0, body_root_a),
            "Proof generated from body_a must verify against body_a's root"
        );

        // ❌ Verification FAILS against body_b's root (different body structure)
        // This is the critical check: proof tied to body_a should NOT verify against body_b
        assert!(
            !verify_kzg_commitment_inclusion_proof(&commitments[0], &proof, 0, body_root_b),
            "Proof generated from body_a must REJECT body_b's root (different execution_payload_root)"
        );
    }

    #[test]
    #[ignore] // Run manually with: cargo test generate_test_vector -- --ignored --nocapture
    fn generate_test_vector() {
        // Create test commitments
        let commitments =
            vec![create_test_commitment(1), create_test_commitment(2), create_test_commitment(3)];
        let body = make_test_body(&commitments);

        // Generate proof for index 1
        let index = 1;
        let proof = generate_kzg_commitment_inclusion_proof(&commitments, index, &body).unwrap();
        let body_root = body.compute_body_root();

        // Print test vector in Rust format
        println!("\n// Test vector generated from Ultramarine implementation");
        println!(
            "let commitment = KzgCommitment::from_slice(&hex::decode(\"{}\").unwrap()).unwrap();",
            hex::encode(commitments[index].as_bytes())
        );
        println!("let index = {};", index);
        println!("let proof_hex = vec![");
        for branch in &proof {
            println!("    \"{}\",", hex::encode(branch));
        }
        println!("];");
        println!("let body_root = hex::decode(\"{}\").unwrap();", hex::encode(body_root));
        println!();

        // Verify it works
        assert!(verify_kzg_commitment_inclusion_proof(
            &commitments[index],
            &proof,
            index,
            body_root
        ));
    }
}
