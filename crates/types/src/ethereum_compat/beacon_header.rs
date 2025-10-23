//! Beacon block header types for Ethereum Deneb compatibility
#![allow(dead_code)]
//! These types exist ONLY to enable BlobSidecar to match the Ethereum spec.
//! They should never be used outside of blob gossip/verification paths.
//!
//! ## References
//! - Ethereum spec: `consensus-specs/specs/phase0/beacon-chain.md#beaconblockheader`
//! - Lighthouse: `lighthouse/consensus/types/src/beacon_block_header.rs`

use alloy_primitives::B256;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};
use tree_hash_derive::TreeHash;

use crate::{
    codec::proto::{decode_signature, encode_signature},
    proto,
    signing::{PublicKey, Signature},
};

/// Beacon block header (Ethereum consensus spec)
///
/// This header provides the minimal context needed to verify blob inclusion proofs
/// according to the Deneb specification. It maps to Ultramarine concepts as follows:
///
/// | Ethereum Field    | Ultramarine Equivalent               |
/// |-------------------|--------------------------------------|
/// | `slot`            | `Height` (block number)              |
/// | `proposer_index`  | Validator index in validator set     |
/// | `parent_root`     | Previous block hash                  |
/// | `state_root`      | ExecutionPayloadHeader.state_root    |
/// | `body_root`       | SSZ root of block body (for proofs)  |
///
/// ## Why `body_root` is Critical
///
/// The `body_root` is a Merkle root that commits to all fields in the beacon block body,
/// including the list of blob KZG commitments. This enables:
/// 1. Proving a specific commitment is in the list (via Merkle inclusion proof)
/// 2. Verifying the commitment without downloading the full block body
/// 3. Linking blob data → commitment → body_root → signed header
///
/// ## Internal Use Only
///
/// This type is part of the Ethereum compatibility layer and should NOT be used
/// in core Ultramarine consensus logic (Value, ValueMetadata, etc.).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TreeHash)]
pub struct BeaconBlockHeader {
    /// Slot number (≈ Ultramarine Height)
    pub slot: u64,

    /// Proposer validator index
    pub proposer_index: u64,

    /// Parent block root (hash of previous BeaconBlockHeader)
    pub parent_root: B256,

    /// State root (from ExecutionPayloadHeader)
    pub state_root: B256,

    /// Body root (SSZ Merkle root of BeaconBlockBody)
    ///
    /// This commits to all block body fields including blob_kzg_commitments.
    pub body_root: B256,
}

impl BeaconBlockHeader {
    /// Create a new beacon block header
    pub fn new(
        slot: u64,
        proposer_index: u64,
        parent_root: B256,
        state_root: B256,
        body_root: B256,
    ) -> Self {
        Self { slot, proposer_index, parent_root, state_root, body_root }
    }

    /// Calculate the SSZ hash tree root of this header
    ///
    /// Uses proper SSZ merkleization as defined in the Ethereum consensus spec.
    /// This is the value that gets signed by the proposer.
    ///
    /// https://github.com/ethereum/consensus-specs/blob/dev/ssz/simple-serialize.md#merkleization
    pub fn hash_tree_root(&self) -> B256 {
        use tree_hash::TreeHash;

        // Use the TreeHash trait (derived above) to compute the SSZ tree_hash_root
        // tree_hash::Hash256 is ethereum_hashing::FixedBytes<32>
        let root = TreeHash::tree_hash_root(self);

        // Convert from tree_hash::Hash256 (FixedBytes<32>) to alloy_primitives::B256
        // Both are 32-byte fixed arrays, so we can copy the bytes
        B256::from_slice(root.as_ref())
    }
}

impl Protobuf for BeaconBlockHeader {
    type Proto = proto::BeaconBlockHeader;

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::BeaconBlockHeader {
            slot: self.slot,
            proposer_index: self.proposer_index,
            parent_root: self.parent_root.to_vec().into(),
            state_root: self.state_root.to_vec().into(),
            body_root: self.body_root.to_vec().into(),
        })
    }

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            slot: proto.slot,
            proposer_index: proto.proposer_index,
            parent_root: B256::from_slice(&proto.parent_root),
            state_root: B256::from_slice(&proto.state_root),
            body_root: B256::from_slice(&proto.body_root),
        })
    }
}

/// Signed beacon block header (Ethereum consensus spec)
///
/// Contains a BeaconBlockHeader and the proposer's signature over its hash tree root.
/// This creates a complete chain of trust:
///
/// ```text
/// Blob data
///   ↓ (KZG proof verifies)
/// KZG commitment
///   ↓ (Merkle inclusion proof verifies)
/// body_root (in BeaconBlockHeader)
///   ↓ (Signature verifies)
/// Proposer's public key
/// ```
///
/// ## Security Properties
///
/// - **Authenticity**: Signature proves the header came from the claimed proposer
/// - **Integrity**: body_root commits to all blob commitments in the block
/// - **Inclusion**: Merkle proof links specific commitment to body_root
/// - **Correctness**: KZG proof links blob data to commitment
///
/// ## Signature Algorithm
///
/// Ultramarine uses Ed25519 signatures (same as ProposalFin) instead of Ethereum's
/// BLS12-381. Both provide equivalent security (~128-bit), but Ed25519 is:
/// - Faster to verify (~50μs vs ~1ms for BLS)
/// - Simpler implementation (no pairings)
/// - Widely supported in Rust ecosystem
///
/// ## Internal Use Only
///
/// This type exists solely for Ethereum spec compatibility within BlobSidecar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBeaconBlockHeader {
    /// The block header message
    pub message: BeaconBlockHeader,

    /// Proposer's signature over hash_tree_root(message)
    ///
    /// Note: Ultramarine uses Ed25519, Ethereum uses BLS12-381.
    /// Both are secure, we chose Ed25519 for performance.
    pub signature: Signature,
}

impl SignedBeaconBlockHeader {
    /// Create a new signed beacon block header
    pub fn new(message: BeaconBlockHeader, signature: Signature) -> Self {
        Self { message, signature }
    }

    /// Get the message hash that should be signed
    ///
    /// This is the SSZ hash_tree_root of the BeaconBlockHeader.
    pub fn signing_root(&self) -> B256 {
        self.message.hash_tree_root()
    }

    /// Verify the signature on this header
    ///
    /// Verifies that the signature was created by signing `hash_tree_root(message)`
    /// with the proposer's private key.
    ///
    /// ## Parameters
    ///
    /// - `public_key`: The proposer's Ed25519 public key (32 bytes)
    ///
    /// ## Returns
    ///
    /// `true` if the signature is valid, `false` otherwise.
    ///
    /// ## Note
    ///
    /// This uses Ed25519 signatures from Malachite's signing module.
    /// Ethereum uses BLS12-381, but Ed25519 provides equivalent security
    /// with better performance.
    pub fn verify_signature(&self, public_key: &PublicKey) -> bool {
        let message_root = self.signing_root();

        // Use Malachite's Ed25519 verification
        // PublicKey::verify returns Result<(), signature::Error>
        public_key.verify(message_root.as_slice(), &self.signature).is_ok()
    }
}

impl Protobuf for SignedBeaconBlockHeader {
    type Proto = proto::SignedBeaconBlockHeader;

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::SignedBeaconBlockHeader {
            message: Some(self.message.to_proto()?),
            signature: Some(encode_signature(&self.signature)),
        })
    }

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let message =
            proto.message.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("message"))?;
        let signature =
            proto.signature.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))?;

        Ok(Self {
            message: BeaconBlockHeader::from_proto(message)?,
            signature: decode_signature(signature)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_beacon_block_header_creation() {
        let header = BeaconBlockHeader::new(
            12345,      // slot
            42,         // proposer_index
            B256::ZERO, // parent_root
            B256::ZERO, // state_root
            B256::ZERO, // body_root
        );

        assert_eq!(header.slot, 12345);
        assert_eq!(header.proposer_index, 42);
    }

    #[test]
    fn test_hash_tree_root_deterministic() {
        let header1 = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);
        let header2 = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);

        // Same inputs should produce same hash
        assert_eq!(header1.hash_tree_root(), header2.hash_tree_root());
    }

    #[test]
    fn test_hash_tree_root_changes_with_data() {
        let header1 = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);
        let header2 = BeaconBlockHeader::new(12346, 42, B256::ZERO, B256::ZERO, B256::ZERO);

        // Different data should produce different hash
        assert_ne!(header1.hash_tree_root(), header2.hash_tree_root());
    }

    #[test]
    fn test_signed_header_creation() {
        let header = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);
        let signature = Signature::from_bytes([0u8; 64]);

        let signed_header = SignedBeaconBlockHeader::new(header.clone(), signature.clone());

        assert_eq!(signed_header.message, header);
        assert_eq!(signed_header.signature, signature);
    }

    #[test]
    fn test_signing_root() {
        let header = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);
        let signature = Signature::from_bytes([0u8; 64]);
        let signed_header = SignedBeaconBlockHeader::new(header.clone(), signature);

        // Signing root should match header's hash tree root
        assert_eq!(signed_header.signing_root(), header.hash_tree_root());
    }

    #[test]
    fn test_signature_verification_with_real_keypair() {
        use crate::signing::{Ed25519Provider, PrivateKey};

        // Create a keypair
        let private_key = PrivateKey::from([1u8; 32]);
        let public_key = private_key.public_key();
        let provider = Ed25519Provider::new(private_key);

        // Create a header and sign it
        let header = BeaconBlockHeader::new(12345, 42, B256::ZERO, B256::ZERO, B256::ZERO);
        let message_root = header.hash_tree_root();
        let signature = provider.sign(message_root.as_slice());

        let signed_header = SignedBeaconBlockHeader::new(header, signature);

        // Verification should pass with correct key
        assert!(signed_header.verify_signature(&public_key));

        // Verification should fail with wrong key
        let wrong_key = PrivateKey::from([2u8; 32]).public_key();
        assert!(!signed_header.verify_signature(&wrong_key));
    }

    #[test]
    fn test_ssz_tree_hash_root_spec_compliance() {
        // Test vector: BeaconBlockHeader with known SSZ tree_hash_root
        //
        // This test ensures our SSZ implementation matches the Ethereum spec.
        // The test vector uses simple values (zeros) to make verification straightforward.
        //
        // SSZ merkleization for BeaconBlockHeader (5 fields, each 32 bytes after padding):
        // 1. slot (u64) -> padded to 32 bytes
        // 2. proposer_index (u64) -> padded to 32 bytes
        // 3. parent_root (B256) -> 32 bytes
        // 4. state_root (B256) -> 32 bytes
        // 5. body_root (B256) -> 32 bytes
        //
        // Then merkleize: hash(hash(slot, proposer_index), hash(parent_root, hash(state_root,
        // body_root)))

        // Test case 1: All zeros (empty header)
        let empty_header = BeaconBlockHeader::new(
            0,          // slot
            0,          // proposer_index
            B256::ZERO, // parent_root
            B256::ZERO, // state_root
            B256::ZERO, // body_root
        );

        let root = empty_header.hash_tree_root();

        // Known SSZ root for all-zero BeaconBlockHeader
        // This is the SSZ tree_hash_root for BeaconBlockHeader{ slot: 0, proposer_index: 0,
        // parent_root: 0x00..00, state_root: 0x00..00, body_root: 0x00..00 }
        //
        // SSZ merkleization process for 5 fields:
        // 1. Convert each field to 32-byte chunks (u64 fields are little-endian padded)
        // 2. Build binary merkle tree: nextPow2(5) = 8 leaves (pad with zeros)
        // 3. Hash pairs bottom-up to get root
        //
        // Since all fields are zero, all chunks are zero, resulting in a well-known hash
        // This root value was computed using the `tree_hash` crate (v0.10.0) and is deterministic
        // Verified with: tree_hash::TreeHash::tree_hash_root(&BeaconBlockHeader::new(0, 0,
        // B256::ZERO, B256::ZERO, B256::ZERO))
        let expected_root_hex = "c78009fdf07fc56a11f122370658a353aaa542ed63e44c4bc15ff4cd105ab33c";
        let expected_root = hex::decode(expected_root_hex).expect("Valid hex");

        assert_eq!(
            root.as_slice(),
            expected_root.as_slice(),
            "SSZ tree_hash_root for empty BeaconBlockHeader should match expected value. Got: {}",
            hex::encode(root)
        );

        // Test case 2: Non-zero values
        let header_with_data = BeaconBlockHeader::new(
            12345,                 // slot
            42,                    // proposer_index
            B256::from([1u8; 32]), // parent_root
            B256::from([2u8; 32]), // state_root
            B256::from([3u8; 32]), // body_root
        );

        let root2 = header_with_data.hash_tree_root();

        // Should be different from empty header
        assert_ne!(root, root2, "Different header data should produce different roots");

        // Should be 32 bytes
        assert_eq!(root2.len(), 32, "SSZ root should be 32 bytes");

        // Should be deterministic (same input -> same output)
        let root3 = header_with_data.hash_tree_root();
        assert_eq!(root2, root3, "SSZ root should be deterministic");
    }
}
