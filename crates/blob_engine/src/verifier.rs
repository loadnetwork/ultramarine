//! KZG proof verification for EIP-4844 blob sidecars
//!
//! This module provides cryptographic verification of blob data using KZG commitments and proofs.
//! It acts as a security gate: only blobs with valid KZG proofs are accepted into the system.
//!
//! ## Architecture
//!
//! The verifier loads the Ethereum trusted setup once at initialization and uses it to verify:
//! - Individual blob KZG proofs
//! - Batch blob KZG proofs (5-10x faster for multiple blobs)
//!
//! ## Usage
//!
//! The verifier is used internally by the BlobEngine and is not directly exposed.
//! Use `BlobEngine::verify_and_store()` instead, which handles verification automatically.

use std::sync::Arc;

use c_kzg::{Blob as CKzgBlob, Bytes48, KzgSettings};
use serde::Deserialize;
use thiserror::Error;
use ultramarine_types::proposal_part::BlobSidecar;

/// Embedded Ethereum mainnet trusted setup (same as Lighthouse)
const TRUSTED_SETUP_BYTES: &[u8] = include_bytes!("trusted_setup.json");

/// Number of bytes per G1 point
const BYTES_PER_G1_POINT: usize = 48;
/// Number of bytes per G2 point
const BYTES_PER_G2_POINT: usize = 96;

/// Disables fixed-base multi-scalar multiplication optimization
/// (precomputation handled by rust-eth-kzg if needed)
const NO_PRECOMPUTE: u64 = 0;

/// Wrapper over a BLS G1 point's byte representation
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
struct G1Point(#[serde(with = "hex_serde")] [u8; BYTES_PER_G1_POINT]);

/// Wrapper over a BLS G2 point's byte representation
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
struct G2Point(#[serde(with = "hex_serde")] [u8; BYTES_PER_G2_POINT]);

/// Trusted setup parameters for KZG
#[derive(Debug, Clone, Deserialize)]
struct TrustedSetup {
    g1_monomial: Vec<G1Point>,
    g1_lagrange: Vec<G1Point>,
    g2_monomial: Vec<G2Point>,
}

impl TrustedSetup {
    fn g1_monomial_bytes(&self) -> Vec<u8> {
        self.g1_monomial.iter().flat_map(|p| p.0).collect()
    }

    fn g1_lagrange_bytes(&self) -> Vec<u8> {
        self.g1_lagrange.iter().flat_map(|p| p.0).collect()
    }

    fn g2_monomial_bytes(&self) -> Vec<u8> {
        self.g2_monomial.iter().flat_map(|p| p.0).collect()
    }
}

/// Hex serialization/deserialization helper
mod hex_serde {
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != N {
            return Err(serde::de::Error::custom(format!(
                "Expected {} bytes, got {}",
                N,
                bytes.len()
            )));
        }
        let mut array = [0u8; N];
        array.copy_from_slice(&bytes);
        Ok(array)
    }
}

/// Errors that can occur during KZG verification
#[derive(Debug, Error)]
pub enum BlobVerificationError {
    /// Failed to load the trusted setup file
    #[error("Failed to load trusted setup: {0}")]
    TrustedSetupLoad(String),

    /// Invalid blob data format
    #[error("Invalid blob data: {0}")]
    InvalidBlob(String),

    /// Invalid KZG commitment format
    #[error("Invalid KZG commitment: {0}")]
    InvalidCommitment(String),

    /// Invalid KZG proof format
    #[error("Invalid KZG proof: {0}")]
    InvalidProof(String),

    /// KZG proof verification failed
    #[error("KZG proof verification failed: {0}")]
    VerificationFailed(String),

    /// KZG proof is invalid (verification returned false)
    #[error("KZG proof is invalid for blob at index {0}")]
    InvalidProofValue(u16),

    /// Empty blob list provided for batch verification
    #[error("Cannot verify empty blob list")]
    EmptyBlobList,
}

/// Handles KZG proof verification for blob sidecars
///
/// This service loads the Ethereum trusted setup once and provides methods
/// to verify individual or batches of blob sidecars.
#[derive(Debug)]
pub(crate) struct BlobVerifier {
    kzg_settings: Arc<KzgSettings>,
}

impl BlobVerifier {
    /// Create a new BlobVerifier with the mainnet trusted setup
    ///
    /// This loads the Ethereum mainnet trusted setup from the embedded c-kzg library.
    /// The trusted setup is loaded once and reused for all subsequent verifications.
    ///
    /// The `precompute` parameter configures precomputation level:
    /// - 0: No precomputation (slower verification, less memory)
    /// - Higher values: More precomputation (faster verification, more memory)
    /// - Recommended: 0 for most use cases
    ///
    /// # Errors
    ///
    /// Returns `BlobVerificationError::TrustedSetupLoad` if the setup cannot be loaded.
    pub(crate) fn new() -> Result<Self, BlobVerificationError> {
        // Parse embedded trusted setup JSON (same approach as Lighthouse)
        let trusted_setup: TrustedSetup =
            serde_json::from_slice(TRUSTED_SETUP_BYTES).map_err(|e| {
                BlobVerificationError::TrustedSetupLoad(format!(
                    "Failed to parse trusted setup: {:?}",
                    e
                ))
            })?;

        // Load KZG settings from parsed trusted setup
        // Uses NO_PRECOMPUTE (0) like Lighthouse for balanced performance
        let kzg_settings = KzgSettings::load_trusted_setup(
            &trusted_setup.g1_monomial_bytes(),
            &trusted_setup.g1_lagrange_bytes(),
            &trusted_setup.g2_monomial_bytes(),
            NO_PRECOMPUTE,
        )
        .map_err(|e| {
            BlobVerificationError::TrustedSetupLoad(format!("Failed to load KZG settings: {:?}", e))
        })?;

        Ok(Self { kzg_settings: Arc::new(kzg_settings) })
    }

    /// Create a new BlobVerifier from a custom trusted setup file
    ///
    /// # Arguments
    ///
    /// * `trusted_setup_path` - Path to the trusted setup file
    ///
    /// # Errors
    ///
    /// Returns `BlobVerificationError::TrustedSetupLoad` if the file cannot be loaded.
    #[allow(dead_code)]
    pub(crate) fn from_trusted_setup_file(
        trusted_setup_path: &str,
    ) -> Result<Self, BlobVerificationError> {
        // Read and parse custom trusted setup file
        let bytes = std::fs::read(trusted_setup_path).map_err(|e| {
            BlobVerificationError::TrustedSetupLoad(format!("Failed to read file: {:?}", e))
        })?;

        let trusted_setup: TrustedSetup = serde_json::from_slice(&bytes).map_err(|e| {
            BlobVerificationError::TrustedSetupLoad(format!(
                "Failed to parse trusted setup: {:?}",
                e
            ))
        })?;

        let kzg_settings = KzgSettings::load_trusted_setup(
            &trusted_setup.g1_monomial_bytes(),
            &trusted_setup.g1_lagrange_bytes(),
            &trusted_setup.g2_monomial_bytes(),
            NO_PRECOMPUTE,
        )
        .map_err(|e| {
            BlobVerificationError::TrustedSetupLoad(format!("Failed to load KZG settings: {:?}", e))
        })?;

        Ok(Self { kzg_settings: Arc::new(kzg_settings) })
    }

    /// Verify a single blob sidecar
    ///
    /// This method verifies that the KZG proof in the sidecar is valid for the blob data
    /// and commitment. This ensures the blob data hasn't been tampered with.
    ///
    /// # Arguments
    ///
    /// * `sidecar` - The blob sidecar to verify
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The blob data format is invalid
    /// - The commitment or proof format is invalid
    /// - The KZG verification fails
    /// - The proof is mathematically invalid
    #[allow(dead_code)]
    pub(crate) fn verify_blob_sidecar(
        &self,
        sidecar: &BlobSidecar,
    ) -> Result<(), BlobVerificationError> {
        // Convert blob data to c-kzg format
        let blob = CKzgBlob::from_bytes(sidecar.blob.data())
            .map_err(|e| BlobVerificationError::InvalidBlob(format!("{:?}", e)))?;

        // Convert commitment to c-kzg Bytes48 format
        let commitment = Bytes48::from_bytes(sidecar.kzg_commitment.as_bytes())
            .map_err(|e| BlobVerificationError::InvalidCommitment(format!("{:?}", e)))?;

        // Convert proof to c-kzg Bytes48 format
        let proof = Bytes48::from_bytes(sidecar.kzg_proof.as_bytes())
            .map_err(|e| BlobVerificationError::InvalidProof(format!("{:?}", e)))?;

        // Verify KZG proof using KzgSettings method
        let valid = self
            .kzg_settings
            .verify_blob_kzg_proof(&blob, &commitment, &proof)
            .map_err(|e| BlobVerificationError::VerificationFailed(format!("{:?}", e)))?;

        if !valid {
            return Err(BlobVerificationError::InvalidProofValue(sidecar.index));
        }

        Ok(())
    }

    /// Batch verify multiple blob sidecars (5-10x faster than individual verification)
    ///
    /// This method verifies all blobs in a batch, which is significantly faster than
    /// verifying them one by one. Use this when you have multiple blobs to verify.
    ///
    /// # Arguments
    ///
    /// * `sidecars` - The blob sidecars to verify
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The sidecar list is empty
    /// - Any blob data format is invalid
    /// - Any commitment or proof format is invalid
    /// - The batch KZG verification fails
    /// - Any proof is mathematically invalid
    #[allow(dead_code)]
    pub(crate) fn verify_blob_sidecars_batch(
        &self,
        sidecars: &[&BlobSidecar],
    ) -> Result<(), BlobVerificationError> {
        if sidecars.is_empty() {
            return Err(BlobVerificationError::EmptyBlobList);
        }

        // Convert all blobs to c-kzg format
        let blobs: Vec<CKzgBlob> = sidecars
            .iter()
            .map(|s| CKzgBlob::from_bytes(s.blob.data()))
            .collect::<Result<_, _>>()
            .map_err(|e| BlobVerificationError::InvalidBlob(format!("{:?}", e)))?;

        // Convert all commitments to c-kzg Bytes48 format
        let commitments: Vec<Bytes48> = sidecars
            .iter()
            .map(|s| Bytes48::from_bytes(s.kzg_commitment.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|e| BlobVerificationError::InvalidCommitment(format!("{:?}", e)))?;

        // Convert all proofs to c-kzg Bytes48 format
        let proofs: Vec<Bytes48> = sidecars
            .iter()
            .map(|s| Bytes48::from_bytes(s.kzg_proof.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|e| BlobVerificationError::InvalidProof(format!("{:?}", e)))?;

        // Batch verify all proofs (much faster than verifying individually)
        let valid = self
            .kzg_settings
            .verify_blob_kzg_proof_batch(&blobs, &commitments, &proofs)
            .map_err(|e| BlobVerificationError::VerificationFailed(format!("{:?}", e)))?;

        if !valid {
            // If batch verification fails, we don't know which blob is invalid
            // The caller may want to verify individually to find the bad one
            return Err(BlobVerificationError::VerificationFailed(
                "Batch verification failed for one or more blobs".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ultramarine_types::{
        aliases::Bytes,
        blob::{Blob, KzgCommitment, KzgProof},
        proposal_part::BlobSidecar,
    };

    use super::*;

    #[test]
    fn test_verifier_creation() {
        // Test that we can create a verifier with the default trusted setup
        let verifier = BlobVerifier::new();
        assert!(verifier.is_ok(), "Failed to create BlobVerifier");
    }

    #[test]
    #[ignore] // Requires valid test vectors from Ethereum spec
    fn test_verify_valid_blob() {
        // TODO: Add real test vectors from Ethereum consensus specs
        // This test is ignored until we have valid test data
        let verifier = BlobVerifier::new().unwrap();

        // Create a dummy blob sidecar (this will fail verification as it's not real)
        let blob_data = vec![0u8; 131_072];
        let blob = Blob::new(Bytes::from(blob_data)).unwrap();
        let commitment = KzgCommitment([0u8; 48]);
        let proof = KzgProof([0u8; 48]);

        let sidecar = BlobSidecar::from_bundle_item(0, blob, commitment, proof);

        // This should fail with invalid proof since we're using dummy data
        let result = verifier.verify_blob_sidecar(&sidecar);
        assert!(result.is_err(), "Dummy data should fail verification");
    }

    #[test]
    fn test_batch_verify_empty_list() {
        let verifier = BlobVerifier::new().unwrap();
        let empty_list: Vec<&BlobSidecar> = vec![];

        let result = verifier.verify_blob_sidecars_batch(&empty_list);
        assert!(
            matches!(result, Err(BlobVerificationError::EmptyBlobList)),
            "Empty list should return EmptyBlobList error"
        );
    }

    #[test]
    fn test_invalid_blob_size() {
        let _verifier = BlobVerifier::new().unwrap();

        // Create a blob with wrong size (should be 131,072 bytes)
        let blob_data = vec![0u8; 1000]; // Wrong size!
        let blob_result = Blob::new(Bytes::from(blob_data));

        assert!(blob_result.is_err(), "Invalid blob size should be rejected");
    }
}
