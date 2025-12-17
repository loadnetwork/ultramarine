//! EIP-4844 Blob Types
//!
//! This module implements the core data structures for EIP-4844 blob transactions.
//! Blobs are large (~128KB) data chunks that are committed to via KZG commitments
//! and stored separately from the execution payload.
//!
//! ## Architecture Overview
//!
//! ```text
//! Execution Layer (EL)
//!     │
//!     │ getPayloadV3()
//!     ├──> ExecutionPayload (block data)
//!     └──> BlobsBundle (blobs + commitments + proofs)
//!           │
//!           ├─> Vec<Blob>           (actual data, 131,072 bytes each)
//!           ├─> Vec<KzgCommitment>  (48 bytes each, commit to blob)
//!           └─> Vec<KzgProof>       (48 bytes each, for verification)
//!                   │
//!                   │ Consensus Layer
//!                   ├─> ValueMetadata (lightweight, ~2KB)
//!                   │       └─> Contains commitments only, not blobs
//!                   │
//!                   └─> ProposalPart::BlobSidecar (streams blobs separately)
//! ```
//!
//! ## Key Properties
//!
//! - **Blob size**: Exactly 131,072 bytes (fixed by EIP-4844 spec)
//! - **Commitment size**: 48 bytes (BLS12-381 G1 point)
//! - **Proof size**: 48 bytes (BLS12-381 G1 point)
//! - **Max blobs per block**: 4096 per Deneb preset (practical limit enforced by blob gas)
//!
//! ## References
//!
//! - EIP-4844: <https://eips.ethereum.org/EIPS/eip-4844>
//! - Consensus specs: <https://github.com/ethereum/consensus-specs/blob/dev/specs/deneb/>
//! - Polynomial commitments: <https://github.com/ethereum/consensus-specs/blob/dev/specs/deneb/polynomial-commitments.md>

// Use Alloy's versioned hash calculation (uses SHA-256 per EIP-4844 spec)
// Re-export so other crates can depend on ultramarine_types instead of alloy directly.
pub use alloy_eips::eip4844::kzg_to_versioned_hash;
// Phase 1b: Import Alloy's BlobsBundleV1 for conversion from Engine API responses
use alloy_rpc_types_engine::BlobsBundleV1;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};
use sha3::{Digest as Sha3Digest, Keccak256};
use tree_hash::{PackedEncoding, TreeHash, TreeHashType};

use crate::{
    aliases::{B256, Bytes},
    proto,
};

/// The number of bytes in a single blob.
///
/// This is a fixed constant defined by EIP-4844. Each blob is exactly 131,072 bytes,
/// which corresponds to 4096 field elements of 32 bytes each in the BLS12-381 scalar field.
///
/// **Why this size?**
/// - 4096 field elements is a power of 2 (efficient for FFT operations in KZG)
/// - Each field element is ~32 bytes when encoded
/// - Total: 4096 * 32 = 131,072 bytes
///
/// **Do NOT change this value** - it's part of the Ethereum consensus protocol.
pub const BYTES_PER_BLOB: usize = 131_072;

/// The size of a KZG commitment in bytes.
///
/// KZG commitments are points on the BLS12-381 elliptic curve (G1 group).
/// They are serialized as compressed points, which take 48 bytes.
///
/// **Technical detail**: BLS12-381 is a pairing-friendly curve chosen for its
/// security properties and efficient pairing operations needed for KZG proofs.
pub const BYTES_PER_COMMITMENT: usize = 48;

/// The size of a KZG proof in bytes.
///
/// Like commitments, proofs are also G1 points on BLS12-381, serialized to 48 bytes.
pub const BYTES_PER_PROOF: usize = 48;

/// Maximum SSZ List capacity for blob commitments.
///
/// This is the theoretical limit defined by the consensus spec for
/// `List[KZGCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK]`. The SSZ list capacity
/// determines the Merkle tree depth (log2(4096) = 12 levels).
///
/// **Note**: This is used for SSZ serialization and Merkle proof generation,
/// NOT for protocol validation. See `MAX_BLOBS_PER_BLOCK` for the actual limit.
pub const MAX_BLOB_COMMITMENTS_PER_BLOCK: usize = 4096;

/// Maximum number of blobs allowed per block (protocol limit).
///
/// This chain enforces a practical limit of 1024 blobs per block, which provides:
/// - High DA capacity: 1024 blobs × 131,072 bytes = ~134 MB/block
/// - Room for future increases (can raise to 4096 without protocol changes)
/// - Manageable for validators and network
///
/// The SSZ structure supports up to `MAX_BLOB_COMMITMENTS_PER_BLOCK` (4096),
/// but this protocol limit constrains actual usage.
///
/// **Comparison**:
/// - Ethereum Deneb: 6 blobs (~786 KB)
/// - Ethereum Electra: 9 blobs (~1.18 MB)
/// - This chain: 1024 blobs (~134 MB)
pub const MAX_BLOBS_PER_BLOCK: usize = 1024;

/// Maximum number of blobs per block in Ethereum's Electra fork.
///
/// **Note**: This is Ethereum mainnet's Electra limit (9 blobs). Our chain uses
/// `MAX_BLOBS_PER_BLOCK = 1024` for higher DA throughput.
/// This constant is kept for reference/compatibility only.
pub const MAX_BLOBS_PER_BLOCK_ELECTRA: usize = 9;

/// A single blob containing arbitrary data.
///
/// Blobs are the core data structure for EIP-4844. They are large, fixed-size
/// chunks of data that are committed to via KZG commitments but stored separately
/// from the execution payload to avoid bloating the blockchain.
///
/// ## Storage Strategy
///
/// - **Execution Payload**: Stores only the KZG commitments (48 bytes each)
/// - **Blob Sidecars**: Store the actual blob data (131KB each) separately
/// - **Consensus Layer**: Only votes on the commitments, not the full blobs
///
/// ## Use Cases
///
/// - **Layer 2 rollups**: Posting transaction data or state diffs
/// - **Data availability**: Making data available without storing it on-chain forever
/// - **Cost reduction**: Cheaper than CALLDATA for large data chunks
///
/// ## Example
///
/// ```rust,ignore
/// use ultramarine_types::blob::Blob;
/// use bytes::Bytes;
///
/// // Create a blob from 131,072 bytes of data
/// let data = Bytes::from(vec![0u8; 131_072]);
/// let blob = Blob::new(data)?;
///
/// assert_eq!(blob.size(), 131_072);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    /// The raw blob data.
    ///
    /// **Invariant**: This MUST be exactly `BYTES_PER_BLOB` (131,072) bytes.
    /// The constructor enforces this constraint.
    data: Bytes,
}

impl Blob {
    /// Creates a new blob from the given data.
    ///
    /// ## Validation
    ///
    /// This function validates that the data is exactly `BYTES_PER_BLOB` bytes.
    /// If the size is incorrect, it returns an error.
    ///
    /// ## Errors
    ///
    /// Returns an error string if:
    /// - `data.len() != BYTES_PER_BLOB`
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let data = Bytes::from(vec![0u8; 131_072]);
    /// let blob = Blob::new(data)?;  // ✅ OK
    ///
    /// let bad_data = Bytes::from(vec![0u8; 1000]);
    /// let result = Blob::new(bad_data);  // ❌ Error: size mismatch
    /// assert!(result.is_err());
    /// ```
    pub fn new(data: Bytes) -> Result<Self, String> {
        // Validate blob size - this is critical for consensus compatibility
        if data.len() != BYTES_PER_BLOB {
            return Err(format!(
                "Invalid blob size: expected {} bytes, got {} bytes",
                BYTES_PER_BLOB,
                data.len()
            ));
        }

        Ok(Self { data })
    }

    /// Returns a reference to the blob data.
    ///
    /// This is a zero-copy operation since `Bytes` uses reference counting internally.
    #[inline]
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Returns the size of the blob in bytes.
    ///
    /// This will always return `BYTES_PER_BLOB` (131,072) due to the constructor invariant.
    #[inline]
    pub const fn size(&self) -> usize {
        BYTES_PER_BLOB
    }

    /// Computes the keccak256 hash of the blob bytes.
    #[inline]
    pub fn keccak_hash(&self) -> B256 {
        let mut hasher = Keccak256::new();
        hasher.update(self.data.as_ref());
        B256::from_slice(hasher.finalize().as_slice())
    }

    /// Consumes the blob and returns the underlying data.
    ///
    /// Use this when you need ownership of the `Bytes` without cloning.
    #[inline]
    pub fn into_data(self) -> Bytes {
        self.data
    }
}

/// A KZG commitment to a blob.
///
/// ## What is a KZG Commitment?
///
/// A KZG (Kate-Zaverucha-Goldberg) commitment is a cryptographic commitment scheme
/// based on polynomial commitments. It allows you to commit to a polynomial (the blob data)
/// and later prove that the polynomial evaluates to a specific value at a specific point.
///
/// ## Properties
///
/// - **Binding**: You cannot find two different blobs with the same commitment
/// - **Hiding**: The commitment reveals nothing about the blob data
/// - **Succinctness**: Only 48 bytes regardless of blob size (131KB)
///
/// ## Technical Details
///
/// The commitment is a point on the BLS12-381 elliptic curve (G1 group), serialized
/// as a compressed point. The 48-byte encoding is defined by the BLS standard.
///
/// ## Why KZG?
///
/// - **Efficient verification**: Can verify in ~1ms per blob
/// - **Batch verification**: Can verify multiple blobs 5-10x faster than individual checks
/// - **Small size**: 48 bytes vs. 131KB blob
///
/// ## Example
///
/// ```rust,ignore
/// use ultramarine_types::blob::KzgCommitment;
///
/// // Commitment from the execution layer
/// let commitment = KzgCommitment([0u8; 48]);
///
/// // Used in ValueMetadata to keep consensus messages small
/// let commitments = vec![commitment];
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KzgCommitment(
    /// The 48-byte commitment data (compressed BLS12-381 G1 point).
    ///
    /// **Format**: This follows the BLS12-381 compressed point serialization:
    /// - First byte contains compression flag and sign information
    /// - Remaining 47 bytes contain the x-coordinate
    ///
    /// **Validation**: The `c-kzg` library will validate this is a valid curve point
    /// when performing KZG operations.
    pub [u8; BYTES_PER_COMMITMENT],
);

impl KzgCommitment {
    /// Creates a new KZG commitment from a 48-byte array.
    ///
    /// **Note**: This does NOT validate that the bytes represent a valid curve point.
    /// Validation happens later during KZG proof verification.
    #[inline]
    pub const fn new(bytes: [u8; BYTES_PER_COMMITMENT]) -> Self {
        Self(bytes)
    }

    /// Returns a reference to the underlying bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; BYTES_PER_COMMITMENT] {
        &self.0
    }

    /// Creates a commitment from a byte slice.
    ///
    /// ## Errors
    ///
    /// Returns an error if the slice is not exactly 48 bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != BYTES_PER_COMMITMENT {
            return Err(format!(
                "Invalid commitment size: expected {} bytes, got {}",
                BYTES_PER_COMMITMENT,
                bytes.len()
            ));
        }

        let mut array = [0u8; BYTES_PER_COMMITMENT];
        array.copy_from_slice(bytes);
        Ok(Self(array))
    }
}

impl TreeHash for KzgCommitment {
    fn tree_hash_type() -> TreeHashType {
        <[u8; BYTES_PER_COMMITMENT] as TreeHash>::tree_hash_type()
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        self.0.tree_hash_packed_encoding()
    }

    fn tree_hash_packing_factor() -> usize {
        <[u8; BYTES_PER_COMMITMENT] as TreeHash>::tree_hash_packing_factor()
    }

    fn tree_hash_root(&self) -> tree_hash::Hash256 {
        TreeHash::tree_hash_root(&self.0)
    }
}

/// Protobuf conversion for KzgCommitment (Phase 2)
impl Protobuf for KzgCommitment {
    type Proto = proto::KzgCommitment;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        KzgCommitment::from_slice(&proto.data).map_err(ProtoError::Other)
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::KzgCommitment { data: self.0.to_vec().into() })
    }
}

/// A KZG proof used to verify blob correctness.
///
/// ## What is a KZG Proof?
///
/// A KZG proof is a cryptographic proof that demonstrates a blob matches its commitment
/// without revealing the blob data itself. The proof allows anyone to verify that:
///
/// ```text
/// verify_blob_kzg_proof(blob, commitment, proof) == true
/// ```
///
/// ## How It Works
///
/// 1. **Prover** (execution layer):
///    - Has the full blob data
///    - Generates commitment C = commit(blob)
///    - Generates proof π = prove(blob, commitment)
///
/// 2. **Verifier** (consensus layer):
///    - Receives blob, commitment C, proof π
///    - Checks: verify(C, blob, π) == true
///    - Does NOT need to trust the prover
///
/// ## Verification Cost
///
/// - **Individual verification**: ~1ms per blob on modern hardware
/// - **Batch verification**: ~5-10x faster for multiple blobs
/// - Uses pairings on BLS12-381 curve (expensive but tractable)
///
/// ## Security
///
/// KZG proofs are secure under the **Algebraic Group Model (AGM)** and the
/// **t-Strong Diffie-Hellman assumption**. Breaking the proof would require
/// solving a hard cryptographic problem.
///
/// ## Example
///
/// ```rust,ignore
/// use ultramarine_types::blob::{Blob, KzgCommitment, KzgProof};
///
/// // Received from execution layer
/// let blob = Blob::new(data)?;
/// let commitment = KzgCommitment([0u8; 48]);
/// let proof = KzgProof([0u8; 48]);
///
/// // Verify in consensus layer (Phase 4)
/// // blob_verifier.verify_blob_sidecar(&sidecar)?;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KzgProof(
    /// The 48-byte proof data (compressed BLS12-381 G1 point).
    ///
    /// Like commitments, proofs are also curve points. The same serialization
    /// format applies.
    pub [u8; BYTES_PER_PROOF],
);

impl KzgProof {
    /// Creates a new KZG proof from a 48-byte array.
    #[inline]
    pub const fn new(bytes: [u8; BYTES_PER_PROOF]) -> Self {
        Self(bytes)
    }

    /// Returns a reference to the underlying bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; BYTES_PER_PROOF] {
        &self.0
    }

    /// Creates a proof from a byte slice.
    ///
    /// ## Errors
    ///
    /// Returns an error if the slice is not exactly 48 bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != BYTES_PER_PROOF {
            return Err(format!(
                "Invalid proof size: expected {} bytes, got {}",
                BYTES_PER_PROOF,
                bytes.len()
            ));
        }

        let mut array = [0u8; BYTES_PER_PROOF];
        array.copy_from_slice(bytes);
        Ok(Self(array))
    }
}

// Custom Serialize/Deserialize implementations for KzgCommitment
// Default serde doesn't support arrays > 32 bytes, so we implement it manually
impl Serialize for KzgCommitment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize as a byte array (compatible with borsh, bincode, etc.)
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for KzgCommitment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Visitor to deserialize the byte array
        struct KzgCommitmentVisitor;

        impl<'de> serde::de::Visitor<'de> for KzgCommitmentVisitor {
            type Value = KzgCommitment;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a 48-byte array")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                KzgCommitment::from_slice(v).map_err(serde::de::Error::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut bytes = [0u8; BYTES_PER_COMMITMENT];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(KzgCommitment(bytes))
            }
        }

        deserializer.deserialize_bytes(KzgCommitmentVisitor)
    }
}

// Custom Serialize/Deserialize implementations for KzgProof
impl Serialize for KzgProof {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for KzgProof {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KzgProofVisitor;

        impl<'de> serde::de::Visitor<'de> for KzgProofVisitor {
            type Value = KzgProof;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a 48-byte array")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                KzgProof::from_slice(v).map_err(serde::de::Error::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut bytes = [0u8; BYTES_PER_PROOF];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(KzgProof(bytes))
            }
        }

        deserializer.deserialize_bytes(KzgProofVisitor)
    }
}

/// A bundle of blobs with their cryptographic commitments and proofs.
///
/// ## What is a BlobsBundle?
///
/// The `BlobsBundle` is returned by the execution layer's `getPayloadV3` RPC call.
/// It contains all the blob-related data needed for a block proposal:
///
/// ```text
/// BlobsBundle {
///     blobs:       [Blob₀, Blob₁, ..., Blobₙ]        (131KB each)
///     commitments: [Commit₀, Commit₁, ..., Commitₙ]  (48 bytes each)
///     proofs:      [Proof₀, Proof₁, ..., Proofₙ]     (48 bytes each)
/// }
/// ```
///
/// ## Invariants
///
/// The bundle MUST maintain these invariants (enforced by `validate()`):
///
/// 1. **Equal lengths**: `blobs.len() == commitments.len() == proofs.len()`
/// 2. **Within limits**: `blobs.len() <= MAX_BLOBS_PER_BLOCK`
/// 3. **Commitment matching**: `commitment[i]` commits to `blobs[i]`
/// 4. **Proof validity**: `proof[i]` proves `commitment[i]` for `blobs[i]`
///
/// ## Usage in Consensus Flow
///
/// ```text
/// 1. Execution Layer generates BlobsBundle
/// 2. Consensus extracts commitments → ValueMetadata (~2KB)
/// 3. Consensus streams full BlobsBundle → ProposalPart::BlobSidecar
/// 4. Validators verify proofs before voting
/// 5. Decided block stores BlobsBundle temporarily (hot storage)
/// 6. After retention period, blobs are pruned (commitments stay on-chain)
/// ```
///
/// ## Example
///
/// ```rust,ignore
/// use ultramarine_types::blob::{Blob, BlobsBundle, KzgCommitment, KzgProof};
///
/// // Received from execution layer via getPayloadV3
/// let bundle = BlobsBundle {
///     blobs: vec![blob1, blob2, blob3],
///     commitments: vec![commit1, commit2, commit3],
///     proofs: vec![proof1, proof2, proof3],
/// };
///
/// // Validate structure
/// bundle.validate()?;
///
/// // Extract commitments for consensus voting
/// let versioned_hashes = bundle.versioned_hashes();
///
/// // Store for later retrieval
/// blob_store.store_blobs(height, &bundle)?;
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsBundle {
    /// The KZG commitments for each blob.
    ///
    /// **Purpose**: These go into the `ValueMetadata` that consensus votes on.
    /// They are also included in the execution payload's `blob_kzg_commitments` field.
    ///
    /// **Size**: 48 bytes per commitment
    pub commitments: Vec<KzgCommitment>,

    /// The KZG proofs for each blob.
    ///
    /// **Purpose**: Used by validators to verify that each blob matches its commitment.
    /// Verification happens in Phase 4 before voting.
    ///
    /// **Size**: 48 bytes per proof
    pub proofs: Vec<KzgProof>,

    /// The actual blob data.
    ///
    /// **Purpose**: Streamed separately via `ProposalPart::BlobSidecar` to keep
    /// consensus messages small. Stored temporarily in hot storage.
    ///
    /// **Size**: 131,072 bytes per blob
    pub blobs: Vec<Blob>,
}

impl BlobsBundle {
    /// Creates a new `BlobsBundle`.
    ///
    /// **Note**: This does NOT validate the bundle. Call `validate()` after construction
    /// to ensure all invariants are satisfied.
    pub fn new(commitments: Vec<KzgCommitment>, proofs: Vec<KzgProof>, blobs: Vec<Blob>) -> Self {
        Self { commitments, proofs, blobs }
    }

    /// Validates the bundle structure.
    ///
    /// ## Checks Performed
    ///
    /// 1. **Length consistency**: All vectors must have the same length
    /// 2. **Count limit**: Number of blobs must not exceed the fork-specific maximum
    ///
    /// **Note**: This does NOT verify KZG proofs. That happens in Phase 4 with the
    /// `BlobVerifier` using the `c-kzg` library.
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - Vector lengths don't match
    /// - Too many blobs for the current fork
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let bundle = BlobsBundle::new(commitments, proofs, blobs);
    ///
    /// // Validate before using
    /// bundle.validate()?;
    /// ```
    pub fn validate(&self) -> Result<(), String> {
        // Check 1: All vectors must have the same length
        if self.blobs.len() != self.commitments.len() || self.blobs.len() != self.proofs.len() {
            return Err(format!(
                "BlobsBundle length mismatch: {} blobs, {} commitments, {} proofs",
                self.blobs.len(),
                self.commitments.len(),
                self.proofs.len()
            ));
        }

        // Check 2: Must not exceed maximum blobs per block
        //
        // This chain enforces a practical limit of 1024 blobs per block.
        // The SSZ structure supports up to MAX_BLOB_COMMITMENTS_PER_BLOCK (4096)
        // for Merkle proofs, but we constrain actual usage to 1024 blobs
        // (~134 MB/block) for manageable validator load.
        let max_blobs = MAX_BLOBS_PER_BLOCK;
        if self.blobs.len() > max_blobs {
            return Err(format!(
                "Too many blobs: got {}, maximum is {} (protocol limit)",
                self.blobs.len(),
                max_blobs
            ));
        }

        Ok(())
    }

    /// Calculates versioned hashes for all commitments.
    ///
    /// ## What are Versioned Hashes?
    ///
    /// A versioned hash is a 32-byte value derived from a KZG commitment:
    ///
    /// ```text
    /// versioned_hash = SHA256(commitment) with first byte = version
    /// ```
    ///
    /// The version byte (currently `0x01`) allows for future commitment schemes
    /// while maintaining backward compatibility.
    ///
    /// ## Purpose
    ///
    /// Versioned hashes are used in two places:
    ///
    /// 1. **Blob transactions**: The `blob_versioned_hashes` field in type-3 transactions
    /// 2. **Block validation**: Passed to `engine_newPayloadV3` to link blobs to transactions
    ///
    /// ## Implementation
    ///
    /// ```text
    /// For each commitment C:
    ///   1. hash = SHA256(C)              # 32 bytes
    ///   2. hash[0] = 0x01                # Set version byte
    ///   3. versioned_hashes.push(hash)
    /// ```
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let bundle = BlobsBundle::new(commitments, proofs, blobs);
    /// let versioned_hashes = bundle.versioned_hashes();
    ///
    /// // Pass to execution layer
    /// execution_client.new_payload_with_blobs(payload, versioned_hashes, ...)?;
    /// ```
    pub fn versioned_hashes(&self) -> Vec<[u8; 32]> {
        // Use Alloy's EIP-4844 implementation for versioned hash calculation
        // This function:
        // 1. Computes SHA-256 hash of the commitment (per EIP-4844 spec)
        // 2. Sets first byte to 0x01 (VERSIONED_HASH_VERSION_KZG)
        // 3. Returns the 32-byte versioned hash
        //
        // Reference: https://eips.ethereum.org/EIPS/eip-4844#helpers
        self.commitments
            .iter()
            .map(|commitment| {
                let hash = kzg_to_versioned_hash(commitment.as_bytes());
                hash.0 // Convert B256 to [u8; 32]
            })
            .collect()
    }

    /// Returns keccak256 hashes for each blob's bytes.
    pub fn blob_keccak_hashes(&self) -> Vec<B256> {
        self.blobs.iter().map(|blob| blob.keccak_hash()).collect()
    }

    /// Returns the number of blobs in the bundle.
    #[inline]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Returns `true` if the bundle contains no blobs.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }

    /// Calculates the total size of all blobs in bytes.
    ///
    /// This is useful for bandwidth estimation and metrics.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let bundle = BlobsBundle::new(commitments, proofs, blobs);
    /// let total_bytes = bundle.total_blob_bytes();
    ///
    /// println!("Bundle contains {} bytes of blob data", total_bytes);
    /// // Output: "Bundle contains 786432 bytes of blob data" (for 6 blobs)
    /// ```
    pub fn total_blob_bytes(&self) -> usize {
        self.blobs.len() * BYTES_PER_BLOB
    }
}

/// Convert from Alloy's `BlobsBundleV1` (Engine API response) to our `BlobsBundle` type.
///
/// ## Purpose
///
/// This conversion is used when parsing the `getPayloadV3` response from the execution layer.
/// The Engine API returns a `BlobsBundleV1` structure with Alloy types, which we convert
/// to our internal `BlobsBundle` type.
///
/// ## Conversion Process
///
/// ```text
/// BlobsBundleV1 (Alloy)                 BlobsBundle (Ultramarine)
/// ├─ commitments: Vec<Bytes48>    →    ├─ commitments: Vec<KzgCommitment>
/// ├─ proofs: Vec<Bytes48>         →    ├─ proofs: Vec<KzgProof>
/// └─ blobs: Vec<alloy_Blob>       →    └─ blobs: Vec<Blob>
/// ```
///
/// ## Error Handling
///
/// This conversion can fail if:
/// - Any commitment is not exactly 48 bytes
/// - Any proof is not exactly 48 bytes
/// - Any blob is not exactly 131,072 bytes
///
/// These errors should be rare since the execution layer should always return valid sizes,
/// but we validate to ensure consensus safety.
///
/// ## Example
///
/// ```rust,ignore
/// use alloy_rpc_types_engine::BlobsBundleV1;
/// use ultramarine_types::blob::BlobsBundle;
///
/// // Received from Engine API getPayloadV3 response
/// let alloy_bundle: BlobsBundleV1 = /* ... */;
///
/// // Convert to our internal type
/// let our_bundle = BlobsBundle::try_from(alloy_bundle)?;
///
/// // Now can use with our consensus logic
/// our_bundle.validate()?;
/// ```
impl TryFrom<BlobsBundleV1> for BlobsBundle {
    type Error = String;

    fn try_from(alloy_bundle: BlobsBundleV1) -> Result<Self, Self::Error> {
        // Convert commitments: Vec<alloy_consensus::Bytes48> → Vec<KzgCommitment>
        //
        // alloy_consensus::Bytes48 is a wrapper around [u8; 48], so we can
        // directly convert by copying the bytes into our KzgCommitment type.
        let commitments: Result<Vec<KzgCommitment>, String> = alloy_bundle
            .commitments
            .iter()
            .map(|bytes48| {
                // Bytes48 wraps [u8; 48], access it via .0
                KzgCommitment::from_slice(&bytes48.0)
            })
            .collect();
        let commitments = commitments?;

        // Convert proofs: Vec<alloy_consensus::Bytes48> → Vec<KzgProof>
        let proofs: Result<Vec<KzgProof>, String> =
            alloy_bundle.proofs.iter().map(|bytes48| KzgProof::from_slice(&bytes48.0)).collect();
        let proofs = proofs?;

        // Convert blobs: Vec<alloy_consensus::Blob> → Vec<Blob>
        //
        // alloy_consensus::Blob is also a wrapper around a fixed-size array.
        // We need to convert it to a Bytes and then to our Blob type.
        let blobs: Result<Vec<Blob>, String> = alloy_bundle
            .blobs
            .into_iter()
            .map(|alloy_blob| {
                // alloy_consensus::Blob wraps [u8; 131072]
                // Convert to Bytes (which is Arc<[u8]> internally)
                let bytes = Bytes::copy_from_slice(&alloy_blob.0);
                Blob::new(bytes)
            })
            .collect();
        let blobs = blobs?;

        Ok(BlobsBundle { commitments, proofs, blobs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that Blob validation enforces the correct size.
    #[test]
    fn test_blob_size_validation() {
        // Valid blob
        let valid_data = Bytes::from(vec![0u8; BYTES_PER_BLOB]);
        assert!(Blob::new(valid_data).is_ok());

        // Too small
        let small_data = Bytes::from(vec![0u8; 1000]);
        assert!(Blob::new(small_data).is_err());

        // Too large
        let large_data = Bytes::from(vec![0u8; BYTES_PER_BLOB + 1]);
        assert!(Blob::new(large_data).is_err());
    }

    /// Test that BlobsBundle validates length consistency.
    #[test]
    fn test_bundle_length_validation() {
        let blob = Blob::new(Bytes::from(vec![0u8; BYTES_PER_BLOB])).unwrap();
        let commitment = KzgCommitment([0u8; 48]);
        let proof = KzgProof([0u8; 48]);

        // Valid bundle (all same length)
        let valid_bundle = BlobsBundle::new(vec![commitment], vec![proof], vec![blob.clone()]);
        assert!(valid_bundle.validate().is_ok());

        // Invalid: mismatched lengths
        let invalid_bundle =
            BlobsBundle::new(vec![commitment, commitment], vec![proof], vec![blob]);
        assert!(invalid_bundle.validate().is_err());
    }

    /// Test that BlobsBundle enforces maximum blob count.
    #[test]
    fn test_bundle_max_blobs() {
        let blob = Blob::new(Bytes::from(vec![0u8; BYTES_PER_BLOB])).unwrap();
        let commitment = KzgCommitment([0u8; 48]);
        let proof = KzgProof([0u8; 48]);

        // At the limit (should pass)
        let at_limit = BlobsBundle::new(
            vec![commitment; MAX_BLOBS_PER_BLOCK],
            vec![proof; MAX_BLOBS_PER_BLOCK],
            vec![blob.clone(); MAX_BLOBS_PER_BLOCK],
        );
        assert!(at_limit.validate().is_ok());

        // Over the limit (should fail)
        let over_limit = BlobsBundle::new(
            vec![commitment; MAX_BLOBS_PER_BLOCK + 1],
            vec![proof; MAX_BLOBS_PER_BLOCK + 1],
            vec![blob; MAX_BLOBS_PER_BLOCK + 1],
        );
        assert!(over_limit.validate().is_err());
    }

    /// Test versioned hash generation.
    #[test]
    fn test_versioned_hashes() {
        let commitment = KzgCommitment([0u8; 48]);
        let proof = KzgProof([0u8; 48]);
        let blob = Blob::new(Bytes::from(vec![0u8; BYTES_PER_BLOB])).unwrap();

        let bundle = BlobsBundle::new(vec![commitment], vec![proof], vec![blob]);
        let hashes = bundle.versioned_hashes();

        assert_eq!(hashes.len(), 1);
        // First byte should be the version (0x01)
        assert_eq!(hashes[0][0], 0x01);
    }

    /// Test conversion from Alloy's BlobsBundleV1 to our BlobsBundle.
    ///
    /// This tests the TryFrom implementation that's used when parsing
    /// Engine API getPayloadV3 responses.
    #[test]
    fn test_blobs_bundle_conversion_from_alloy() {
        use alloy_consensus::{Blob as AlloyBlob, Bytes48};

        // Create Alloy types (simulating Engine API response)
        let alloy_commitment = Bytes48::from([1u8; 48]);
        let alloy_proof = Bytes48::from([2u8; 48]);
        let alloy_blob = AlloyBlob::from([3u8; BYTES_PER_BLOB]);

        let alloy_bundle = BlobsBundleV1 {
            commitments: vec![alloy_commitment],
            proofs: vec![alloy_proof],
            blobs: vec![alloy_blob],
        };

        // Convert to our BlobsBundle type
        let our_bundle = BlobsBundle::try_from(alloy_bundle).unwrap();

        // Verify conversion
        assert_eq!(our_bundle.commitments.len(), 1);
        assert_eq!(our_bundle.proofs.len(), 1);
        assert_eq!(our_bundle.blobs.len(), 1);

        // Verify commitment bytes
        assert_eq!(our_bundle.commitments[0].as_bytes(), &[1u8; 48]);

        // Verify proof bytes
        assert_eq!(our_bundle.proofs[0].as_bytes(), &[2u8; 48]);

        // Verify blob data
        assert_eq!(our_bundle.blobs[0].data().as_ref(), &[3u8; BYTES_PER_BLOB]);

        // Validate the bundle structure
        assert!(our_bundle.validate().is_ok());
    }

    /// Test conversion with empty blob bundle.
    ///
    /// Engine API may return empty blob bundle if no blob transactions
    /// were included in the block.
    #[test]
    fn test_empty_blobs_bundle_conversion() {
        let empty_alloy_bundle =
            BlobsBundleV1 { commitments: vec![], proofs: vec![], blobs: vec![] };

        let empty_bundle = BlobsBundle::try_from(empty_alloy_bundle).unwrap();

        assert_eq!(empty_bundle.len(), 0);
        assert!(empty_bundle.is_empty());
        assert!(empty_bundle.validate().is_ok());
    }

    /// Test conversion with multiple blobs.
    #[test]
    fn test_multiple_blobs_conversion() {
        use alloy_consensus::{Blob as AlloyBlob, Bytes48};

        // Create 3 blobs
        let alloy_bundle = BlobsBundleV1 {
            commitments: vec![
                Bytes48::from([1u8; 48]),
                Bytes48::from([2u8; 48]),
                Bytes48::from([3u8; 48]),
            ],
            proofs: vec![
                Bytes48::from([4u8; 48]),
                Bytes48::from([5u8; 48]),
                Bytes48::from([6u8; 48]),
            ],
            blobs: vec![
                AlloyBlob::from([7u8; BYTES_PER_BLOB]),
                AlloyBlob::from([8u8; BYTES_PER_BLOB]),
                AlloyBlob::from([9u8; BYTES_PER_BLOB]),
            ],
        };

        let our_bundle = BlobsBundle::try_from(alloy_bundle).unwrap();

        assert_eq!(our_bundle.len(), 3);
        assert!(our_bundle.validate().is_ok());

        // Verify each blob was converted correctly
        assert_eq!(our_bundle.commitments[0].as_bytes(), &[1u8; 48]);
        assert_eq!(our_bundle.commitments[1].as_bytes(), &[2u8; 48]);
        assert_eq!(our_bundle.commitments[2].as_bytes(), &[3u8; 48]);
    }
}
