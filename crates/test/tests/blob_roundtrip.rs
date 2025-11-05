//! Blob round-trip integration test harness.
//!
//! This test exercises the happy-path proposal flow:
//! 1. Proposer streams execution payload + blob sidecars.
//! 2. Validators verify, store, and commit the block.
//! 3. Blob metadata is promoted and blobs become available for import.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_roundtrip() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis, mocks::MockExecutionNotifier, sample_blob_bundle,
        sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    // Arrange: single validator environment with deterministic stores.
    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();
    let mut node = build_state(&dirs, &genesis, validator, Height::new(0))?;
    node.state.seed_genesis_blob_metadata().await?;
    node.state.hydrate_blob_parent_root().await?;

    use common::mocks::MockEngineApi;
    use ultramarine_execution::EngineApi;

    // Prepare execution payload + blobs via mock Execution API to simulate proposer flow.
    let height = Height::new(0);
    let raw_payload = sample_execution_payload_v3_for_height(height);
    let raw_bundle = sample_blob_bundle(2);
    let payload_id = common::payload_id(0);
    let mock_engine = MockEngineApi::default().with_payload(
        payload_id,
        raw_payload.clone(),
        Some(raw_bundle.clone()),
    );
    let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
    let bundle = maybe_bundle.expect("bundle");
    let payload_bytes = Bytes::from(payload.as_ssz_bytes());

    let round = Round::new(0);

    let proposed = node
        .state
        .propose_value_with_blobs(height, round, payload_bytes.clone(), &payload, Some(&bundle))
        .await?;

    // Extract blob sidecars for downstream validators.
    let (_signed_header, sidecars) =
        node.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

    assert_eq!(sidecars.len(), bundle.blobs.len(), "sidecar count matches blob bundle");

    // Simulate verification/storage performed by validators prior to commit.
    let round_i64 = round.as_i64();
    node.state.blob_engine().verify_and_store(height, round_i64, &sidecars).await?;

    let stored = node.state.blob_engine().get_undecided_blobs(height, round_i64).await?;
    assert_eq!(stored.len(), sidecars.len(), "undecided blob count");

    // Store block bytes for commit flow.
    node.state.store_undecided_block_data(height, round, payload_bytes.clone()).await?;

    // Commit the block and promote blobs.
    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposed.value.id(),
        commit_signatures: Vec::new(),
    };
    let mut notifier = MockExecutionNotifier::default();
    node.state
        .process_decided_certificate(&certificate, payload_bytes.clone(), &mut notifier)
        .await?;

    let imported = node.state.blob_engine().get_for_import(height).await?;
    assert_eq!(imported.len(), 2, "decided blob count");
    assert_eq!(imported[0].kzg_commitment, bundle.commitments[0]);
    assert_eq!(imported[1].kzg_commitment, bundle.commitments[1]);

    assert_eq!(node.state.current_height, Height::new(1));

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.verifications_success, sidecars.len() as u64);
    assert_eq!(metrics.verifications_failure, 0);
    assert_eq!(metrics.storage_bytes_undecided, 0);
    assert_eq!(metrics.storage_bytes_decided, (sidecars.len() * BYTES_PER_BLOB) as i64);
    assert_eq!(metrics.lifecycle_promoted, sidecars.len() as u64);
    assert_eq!(metrics.lifecycle_dropped, 0);
    assert_eq!(metrics.lifecycle_pruned, 0);
    assert_eq!(metrics.sync_failures, 0);

    Ok(())
}
