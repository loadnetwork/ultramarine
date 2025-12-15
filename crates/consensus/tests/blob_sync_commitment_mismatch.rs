//! Negative test for commitment validation in sync path.
//!
//! **Tier 0 smoke test:** Negative path validation
//! Kept for fast feedback (~3s) during blob engine development.
//!
//! Validates that ProcessSyncedValue rejects packages where the Value metadata
//! contains commitments that don't match the actual blob sidecars, preventing
//! malicious peers from injecting fake metadata alongside valid blobs.

mod common;

use ultramarine_blob_engine::BlobEngine;

#[tokio::test]
async fn blob_sync_commitment_mismatch_rejected() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis, propose_with_optional_blobs,
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::Round;
    use ssz::Encode;
    use ultramarine_types::{
        blob::KzgCommitment, engine_api::ExecutionPayloadHeader, height::Height,
        sync::SyncedValuePackage, value::Value, value_metadata::ValueMetadata,
    };

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

    let height = Height::new(0);
    let bundle = sample_blob_bundle(1);
    let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
    let round = Round::new(0);

    // Build valid proposal with blobs to get sidecars
    let (proposed, payload_bytes, maybe_sidecars) =
        propose_with_optional_blobs(&mut node.state, height, round, &payload, Some(&bundle))
            .await?;
    let sidecars = maybe_sidecars.expect("sidecars expected");

    // Create fake commitment that differs from the real one
    let mut fake_commitment_bytes = sidecars[0].kzg_commitment.0;
    fake_commitment_bytes[0] ^= 0xFF; // Flip bits in first byte
    let fake_commitment = KzgCommitment(fake_commitment_bytes);

    // Build a Value with the FAKE commitment in metadata
    let header = ExecutionPayloadHeader::from_payload(&payload, None);
    let fake_metadata = ValueMetadata::new(header, vec![fake_commitment]);
    let fake_value = Value::new(fake_metadata);

    // Create sync package with mismatched metadata (fake commitment) and valid sidecars
    let package = SyncedValuePackage::Full {
        value: fake_value,
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: sidecars.clone(),
        execution_requests: Vec::new(),
        archive_notices: Vec::new(),
    };

    // Encode and decode to simulate receiving from network
    let encoded = package.encode().map_err(|e| color_eyre::eyre::eyre!(e))?;
    let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| color_eyre::eyre::eyre!(e))?;

    let result =
        node.state.process_synced_package(height, round, validator.address(), decoded).await?;

    assert!(result.is_none(), "Commitment mismatch should cause package rejection");

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.sync_failures, 1, "sync failure should be recorded");
    assert_eq!(metrics.verifications_success, 1, "blobs were verified");

    // Ensure no blobs or metadata remain after rejection.
    let round_i64 = round.as_i64();
    let undecided = node.state.blob_engine().get_undecided_blobs(height, round_i64).await?;
    assert!(undecided.is_empty(), "invalid blobs should be dropped after validation failure");

    let metadata = node.state.load_blob_metadata_for_round(height, round).await?;
    assert!(metadata.map_or(true, |m| m.blob_count() == 0), "fake metadata should not be stored");

    Ok(())
}

#[tokio::test]
async fn blob_sync_inclusion_proof_failure_rejected() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis, propose_with_optional_blobs,
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::Round;
    use ssz::Encode;
    use ultramarine_types::{
        blob::KzgCommitment, engine_api::ExecutionPayloadHeader, height::Height,
        sync::SyncedValuePackage, value::Value, value_metadata::ValueMetadata,
    };

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

    let height = Height::new(0);
    let bundle = sample_blob_bundle(1);
    let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
    let round = Round::new(0);

    let (proposed, payload_bytes, maybe_sidecars) =
        propose_with_optional_blobs(&mut node.state, height, round, &payload, Some(&bundle))
            .await?;
    let sidecars = maybe_sidecars.expect("sidecars expected");

    let mut tampered_sidecars = sidecars.clone();
    tampered_sidecars[0].kzg_commitment_inclusion_proof.clear();

    let header = ExecutionPayloadHeader::from_payload(&payload, None);
    let metadata = ValueMetadata::new(header, bundle.commitments.clone());
    let value = Value::new(metadata);

    let package = SyncedValuePackage::Full {
        value,
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: tampered_sidecars.clone(),
        execution_requests: Vec::new(),
        archive_notices: Vec::new(),
    };

    let encoded = package.encode().map_err(|e| color_eyre::eyre::eyre!(e))?;
    let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| color_eyre::eyre::eyre!(e))?;

    let result =
        node.state.process_synced_package(height, round, validator.address(), decoded).await?;

    assert!(result.is_none(), "Invalid inclusion proof should cause package rejection");

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.sync_failures, 1, "sync failure should be recorded");
    assert_eq!(metrics.verifications_success, 1, "blobs were verified before rejection");

    let round_i64 = round.as_i64();
    let undecided = node.state.blob_engine().get_undecided_blobs(height, round_i64).await?;
    assert!(undecided.is_empty(), "invalid blobs should be dropped after inclusion proof failure");

    let decided = node.state.blob_engine().get_for_import(height).await?;
    assert!(
        decided.is_empty(),
        "no blobs should be promoted when inclusion proof verification fails"
    );

    Ok(())
}
