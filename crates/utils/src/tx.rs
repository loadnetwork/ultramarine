use std::sync::OnceLock;

use alloy_consensus::{
    BlobTransactionSidecar, SignableTransaction, Signed, TxEip1559, TxEip4844, TxEip4844Variant,
    TxEip4844WithSidecar, TxEnvelope,
};
use alloy_eips::eip4844::{Blob as AlloyBlob, Bytes48 as AlloyBytes48, kzg_to_versioned_hash};
use alloy_network::TxSigner;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use c_kzg::{Blob as CKzgBlob, KzgCommitment, KzgProof, KzgSettings};
use color_eyre::eyre::{Result, eyre};

/// Global KZG settings (loaded once on first use)
static KZG_SETTINGS: OnceLock<KzgSettings> = OnceLock::new();

/// Get or initialize the KZG settings with the embedded trusted setup
fn get_kzg_settings() -> &'static KzgSettings {
    KZG_SETTINGS.get_or_init(|| {
        // Load embedded trusted setup (same as used by blob_engine)
        const TRUSTED_SETUP_BYTES: &[u8] =
            include_bytes!("../../blob_engine/src/trusted_setup.json");

        // Parse the JSON
        let setup_json: serde_json::Value = serde_json::from_slice(TRUSTED_SETUP_BYTES)
            .expect("Failed to parse trusted setup JSON");

        // Extract the arrays
        let g1_monomial = setup_json["g1_monomial"].as_array().expect("Missing g1_monomial");
        let g1_lagrange = setup_json["g1_lagrange"].as_array().expect("Missing g1_lagrange");
        let g2_monomial = setup_json["g2_monomial"].as_array().expect("Missing g2_monomial");

        // Convert to bytes
        let g1_monomial_bytes: Vec<u8> = g1_monomial
            .iter()
            .flat_map(|v| {
                let s = v.as_str().unwrap();
                let s = s.strip_prefix("0x").unwrap_or(s);
                hex::decode(s).unwrap()
            })
            .collect();

        let g1_lagrange_bytes: Vec<u8> = g1_lagrange
            .iter()
            .flat_map(|v| {
                let s = v.as_str().unwrap();
                let s = s.strip_prefix("0x").unwrap_or(s);
                hex::decode(s).unwrap()
            })
            .collect();

        let g2_monomial_bytes: Vec<u8> = g2_monomial
            .iter()
            .flat_map(|v| {
                let s = v.as_str().unwrap();
                let s = s.strip_prefix("0x").unwrap_or(s);
                hex::decode(s).unwrap()
            })
            .collect();

        // Initialize KZG settings with no precomputation (0)
        KzgSettings::load_trusted_setup(
            &g1_monomial_bytes,
            &g1_lagrange_bytes,
            &g2_monomial_bytes,
            0, // No precomputation
        )
        .expect("Failed to load KZG trusted setup")
    })
}

/// Generate a test blob filled with deterministic data
///
/// The blob is filled with a pattern based on the blob index to make each blob unique.
/// This ensures different commitments and helps with debugging.
fn generate_test_blob(blob_index: usize) -> Result<CKzgBlob> {
    const BYTES_PER_BLOB: usize = 131_072;
    let mut data = vec![0u8; BYTES_PER_BLOB];

    // Fill each field element (32 bytes, little-endian) with a small canonical value so the blob
    // is valid for KZG commitments. This keeps every chunk < BLS12-381 modulus while still being
    // deterministic per blob index.
    for (chunk_idx, chunk) in data.chunks_mut(32).enumerate() {
        chunk.fill(0);
        chunk[0] = (blob_index as u8).wrapping_mul(17);
        chunk[1] = (chunk_idx as u8).wrapping_mul(31);
        chunk[2] = ((blob_index + chunk_idx) as u8).wrapping_mul(13);
    }

    CKzgBlob::from_bytes(&data).map_err(|e| eyre!("Failed to create blob: {}", e))
}

/// Generate blobs with KZG commitments and proofs
///
/// Returns: (blobs, commitments, proofs, versioned_hashes)
type BlobKzgBundle = (Vec<CKzgBlob>, Vec<KzgCommitment>, Vec<KzgProof>, Vec<B256>);
fn generate_blobs_with_kzg(blob_count: usize) -> Result<BlobKzgBundle> {
    let kzg = get_kzg_settings();

    let mut blobs = Vec::new();
    let mut commitments = Vec::new();
    let mut proofs = Vec::new();

    for i in 0..blob_count {
        // Generate blob
        let blob = generate_test_blob(i)?;

        // Compute KZG commitment
        let commitment = kzg
            .blob_to_kzg_commitment(&blob)
            .map_err(|e| eyre!("Failed to compute KZG commitment: {}", e))?;

        // Compute KZG proof
        // Convert KzgCommitment to c_kzg::Bytes48 for the proof computation
        let commitment_bytes48: &c_kzg::Bytes48 = unsafe {
            // SAFETY: KzgCommitment is a wrapper around c_kzg::Bytes48, same size
            std::mem::transmute(&commitment)
        };
        let proof = kzg
            .compute_blob_kzg_proof(&blob, commitment_bytes48)
            .map_err(|e| eyre!("Failed to compute KZG proof: {}", e))?;

        blobs.push(blob);
        commitments.push(commitment);
        proofs.push(proof);
    }

    // Compute versioned hashes from commitments
    let versioned_hashes: Vec<B256> = commitments
        .iter()
        .map(|commitment| {
            // Convert KzgCommitment bytes to the format expected by kzg_to_versioned_hash
            let mut bytes = [0u8; 48];
            bytes.copy_from_slice(commitment.as_slice());
            kzg_to_versioned_hash(&bytes)
        })
        .collect();

    Ok((blobs, commitments, proofs, versioned_hashes))
}

/// Convert c-kzg Blob to Alloy Blob for network transmission
fn convert_to_alloy_blob(blob: &CKzgBlob) -> Result<AlloyBlob> {
    // AlloyBlob is a FixedBytes<131072>, copy the data directly
    let mut fixed_bytes = [0u8; 131_072];
    fixed_bytes.copy_from_slice(blob.as_slice());
    Ok(AlloyBlob::from(fixed_bytes))
}

/// Convert c-kzg KzgCommitment to Alloy Bytes48
fn convert_to_alloy_commitment(commitment: &KzgCommitment) -> AlloyBytes48 {
    let mut bytes = [0u8; 48];
    bytes.copy_from_slice(commitment.as_slice());
    AlloyBytes48::from(bytes)
}

/// Convert c-kzg KzgProof to Alloy Bytes48
fn convert_to_alloy_proof(proof: &KzgProof) -> AlloyBytes48 {
    let mut bytes = [0u8; 48];
    bytes.copy_from_slice(proof.as_slice());
    AlloyBytes48::from(bytes)
}

pub(crate) fn make_eip1559_tx(nonce: u64, to: Address, chain_id: u64) -> TxEip1559 {
    TxEip1559 {
        chain_id,
        nonce,
        max_priority_fee_per_gas: 1_000_000_000, // 1 gwei
        max_fee_per_gas: 20_000_000_000,         // 20 gwei
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value: U256::from(100_000_000_000_000_u128), // 0.0001 ETH
        input: Bytes::default(),
        access_list: Default::default(),
    }
}

pub(crate) async fn make_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    chain_id: u64,
) -> Result<TxEnvelope> {
    let mut tx = make_eip1559_tx(nonce, to, chain_id);

    let signature = signer.sign_transaction(&mut tx).await?;
    Ok(tx.into_signed(signature).into())
}

/// Create an EIP-4844 transaction with real blobs, KZG commitments, and proofs
///
/// # Arguments
///
/// * `nonce` - Transaction nonce
/// * `to` - Recipient address
/// * `chain_id` - Chain ID
/// * `blob_count` - Number of blobs to include (1-1024)
///
/// # Returns
///
/// Returns the transaction with real versioned hashes computed from KZG commitments.
/// Note: The actual blobs, commitments, and proofs are generated but not included in
/// the transaction envelope (they're submitted via eth_sendRawTransaction sidecar).
pub(crate) fn make_eip4844_tx(
    nonce: u64,
    to: Address,
    chain_id: u64,
    blob_count: usize,
) -> Result<Eip4844TxWithSidecar> {
    if !(1..=1024).contains(&blob_count) {
        return Err(eyre!("blob_count must be between 1 and 1024"));
    }

    // Generate real blobs with KZG commitments and proofs
    let (blobs, commitments, proofs, versioned_hashes) = generate_blobs_with_kzg(blob_count)?;

    let tx = TxEip4844 {
        chain_id,
        nonce,
        max_priority_fee_per_gas: 1_000_000_000, // 1 gwei
        max_fee_per_gas: 20_000_000_000,         // 20 gwei
        gas_limit: 21_000,
        to,
        value: U256::from(100_000_000_000_000_u128), // 0.0001 ETH
        input: Bytes::default(),
        access_list: Default::default(),
        blob_versioned_hashes: versioned_hashes,
        max_fee_per_blob_gas: 20_000_000_000, // 20 gwei
    };

    Ok((tx, blobs, commitments, proofs))
}

type Eip4844TxWithSidecar = (TxEip4844, Vec<CKzgBlob>, Vec<KzgCommitment>, Vec<KzgProof>);

pub(crate) async fn make_signed_eip4844_tx(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    chain_id: u64,
    blob_count: usize,
) -> Result<TxEnvelope> {
    let (mut tx, blobs, commitments, proofs) = make_eip4844_tx(nonce, to, chain_id, blob_count)?;

    let signature = signer.sign_transaction(&mut tx).await?;

    let blob_sidecar = BlobTransactionSidecar {
        blobs: blobs.iter().map(convert_to_alloy_blob).collect::<Result<Vec<_>>>()?,
        commitments: commitments.iter().map(convert_to_alloy_commitment).collect(),
        proofs: proofs.iter().map(convert_to_alloy_proof).collect(),
    };

    let signed_with_sidecar = Signed::new_unhashed(
        TxEip4844WithSidecar::from_tx_and_sidecar(tx, blob_sidecar),
        signature,
    );

    let signed_variant: Signed<TxEip4844Variant> = signed_with_sidecar.into();

    Ok(TxEnvelope::from(signed_variant))
}

#[cfg(test)]
mod tests {
    use alloy_network::eip2718::Encodable2718;
    use alloy_primitives::Signature;

    use super::*;

    #[tokio::test]
    async fn test_encode_decode_signed_eip1559_tx() {
        let tx = make_eip1559_tx(0, Address::ZERO, 1);
        let signature = Signature::test_signature();
        let signed_tx: TxEnvelope = tx.into_signed(signature).into();
        let tx_bytes = signed_tx.encoded_2718();

        // Verify we can encode the transaction
        assert!(!tx_bytes.is_empty());
    }

    #[test]
    fn test_generate_test_blob() {
        let blob = generate_test_blob(0).expect("should create blob");
        assert_eq!(blob.as_slice().len(), 131_072);
    }

    #[tokio::test]
    async fn test_make_signed_eip4844_tx_includes_sidecar() {
        let signer = PrivateKeySigner::from_slice(&[42u8; 32]).expect("create signer from slice");
        let envelope =
            make_signed_eip4844_tx(&signer, 0, Address::ZERO, 1, 2).await.expect("sign 4844 tx");

        match envelope {
            TxEnvelope::Eip4844(signed) => {
                let bytes = signed.encoded_2718();
                assert!(!bytes.is_empty());
                assert_eq!(bytes[0], TxEip4844::tx_type() as u8);
            }
            other => panic!("expected EIP-4844 envelope, got {:?}", other),
        }
    }

    #[test]
    fn test_generate_blobs_with_kzg() {
        let result = generate_blobs_with_kzg(3);
        assert!(result.is_ok(), "should generate blobs with KZG");

        let (blobs, commitments, proofs, versioned_hashes) = result.unwrap();
        assert_eq!(blobs.len(), 3);
        assert_eq!(commitments.len(), 3);
        assert_eq!(proofs.len(), 3);
        assert_eq!(versioned_hashes.len(), 3);

        // Verify versioned hashes start with 0x01 (version byte)
        for hash in versioned_hashes {
            assert_eq!(hash[0], 0x01, "versioned hash should start with 0x01");
        }
    }

    #[test]
    fn test_make_eip4844_tx() {
        let result = make_eip4844_tx(0, Address::ZERO, 1, 3);
        assert!(result.is_ok(), "should create EIP-4844 transaction");

        let (tx, blobs, commitments, proofs) = result.unwrap();
        assert_eq!(tx.blob_versioned_hashes.len(), 3);
        assert_eq!(blobs.len(), 3);
        assert_eq!(commitments.len(), 3);
        assert_eq!(proofs.len(), 3);

        // Verify all versioned hashes are unique (different blobs)
        let mut hashes = tx.blob_versioned_hashes.clone();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), 3, "all versioned hashes should be unique");
    }
}
