//! Negative test for commitment validation in sync path.
//!
//! Validates that ProcessSyncedValue rejects packages where the Value metadata
//! contains commitments that don't match the actual blob sidecars, preventing
//! malicious peers from injecting fake metadata alongside valid blobs.

mod common;

use serial_test::serial;
use ultramarine_blob_engine::BlobEngine;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_sync_commitment_mismatch_rejected() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis, sample_blob_bundle,
        sample_execution_payload_v3_for_height,
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

    let mut node = build_state(&dirs, &genesis, validator, Height::new(0))?;
    node.state.seed_genesis_blob_metadata().await?;
    node.state.hydrate_blob_parent_root().await?;

    let height = Height::new(0);
    let payload = sample_execution_payload_v3_for_height(height);
    let bundle = sample_blob_bundle(1);
    let payload_bytes = Bytes::from(payload.as_ssz_bytes());
    let round = Round::new(0);

    // Build valid proposal with blobs to get sidecars
    let proposed = node
        .state
        .propose_value_with_blobs(height, round, payload_bytes.clone(), &payload, Some(&bundle))
        .await?;
    let (_signed_header, sidecars) =
        node.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

    // Create fake commitment that differs from the real one
    let mut fake_commitment_bytes = sidecars[0].kzg_commitment.0;
    fake_commitment_bytes[0] ^= 0xFF; // Flip bits in first byte
    let fake_commitment = KzgCommitment(fake_commitment_bytes);

    // Build a Value with the FAKE commitment in metadata
    let header = ExecutionPayloadHeader::from_payload(&payload);
    let fake_metadata = ValueMetadata::new(header, vec![fake_commitment]);
    let fake_value = Value::new(fake_metadata);

    // Create sync package with mismatched metadata (fake commitment) and valid sidecars
    let package = SyncedValuePackage::Full {
        value: fake_value,
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: sidecars.clone(),
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
