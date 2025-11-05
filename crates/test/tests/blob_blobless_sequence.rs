//! Mixed blob/blueless sequence integration test.
//!
//! Commits three consecutive heights (blobbed → blobless → blobbed) and
//! verifies that blob gauges, lifecycle counters, and availability match the
//! expected state at each step.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_blobless_sequence_behaves() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis, mocks::MockExecutionNotifier, sample_blob_bundle,
        sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_state(&dirs, &genesis, validator, Height::new(0))?;
    node.state.seed_genesis_blob_metadata().await?;
    node.state.hydrate_blob_parent_root().await?;

    // Height 0: one blob.
    let height0 = Height::new(0);
    let payload_h0 = sample_execution_payload_v3_for_height(height0);
    let bundle_h0 = sample_blob_bundle(1);
    let bytes_h0 = Bytes::from(payload_h0.as_ssz_bytes());
    let round0 = Round::new(0);

    let proposed_h0 = node
        .state
        .propose_value_with_blobs(height0, round0, bytes_h0.clone(), &payload_h0, Some(&bundle_h0))
        .await?;
    let (_header_h0, sidecars_h0) =
        node.state.prepare_blob_sidecar_parts(&proposed_h0, Some(&bundle_h0))?;
    node.state.blob_engine().verify_and_store(height0, round0.as_i64(), &sidecars_h0).await?;
    node.state.store_undecided_block_data(height0, round0, bytes_h0.clone()).await?;
    let cert_h0 = CommitCertificate {
        height: height0,
        round: round0,
        value_id: proposed_h0.value.id(),
        commit_signatures: Vec::new(),
    };
    let mut notifier_h0 = MockExecutionNotifier::default();
    node.state.process_decided_certificate(&cert_h0, bytes_h0.clone(), &mut notifier_h0).await?;

    let metrics_after_h0 = node.blob_metrics.snapshot();
    assert_eq!(metrics_after_h0.blobs_per_block, 1);
    assert_eq!(metrics_after_h0.lifecycle_promoted, 1);
    assert_eq!(metrics_after_h0.storage_bytes_decided, (1 * BYTES_PER_BLOB) as i64);

    // Height 1: blobless block.
    let height1 = Height::new(1);
    let payload_h1 = sample_execution_payload_v3_for_height(height1);
    let bytes_h1 = Bytes::from(payload_h1.as_ssz_bytes());
    let round1 = Round::new(0);

    let proposed_h1 = node
        .state
        .propose_value_with_blobs(height1, round1, bytes_h1.clone(), &payload_h1, None)
        .await?;
    node.state.store_undecided_block_data(height1, round1, bytes_h1.clone()).await?;
    let cert_h1 = CommitCertificate {
        height: height1,
        round: round1,
        value_id: proposed_h1.value.id(),
        commit_signatures: Vec::new(),
    };
    let mut notifier_h1 = MockExecutionNotifier::default();
    node.state.process_decided_certificate(&cert_h1, bytes_h1.clone(), &mut notifier_h1).await?;

    let metrics_after_h1 = node.blob_metrics.snapshot();
    assert_eq!(metrics_after_h1.blobs_per_block, 0, "gauge should reflect blobless commit");
    assert_eq!(
        metrics_after_h1.lifecycle_promoted, metrics_after_h0.lifecycle_promoted,
        "no new blobs promoted for blobless block"
    );

    // Height 2: two blobs.
    let height2 = Height::new(2);
    let payload_h2 = sample_execution_payload_v3_for_height(height2);
    let bundle_h2 = sample_blob_bundle(2);
    let bytes_h2 = Bytes::from(payload_h2.as_ssz_bytes());
    let round2 = Round::new(0);

    let proposed_h2 = node
        .state
        .propose_value_with_blobs(height2, round2, bytes_h2.clone(), &payload_h2, Some(&bundle_h2))
        .await?;
    let (_header_h2, sidecars_h2) =
        node.state.prepare_blob_sidecar_parts(&proposed_h2, Some(&bundle_h2))?;
    node.state.blob_engine().verify_and_store(height2, round2.as_i64(), &sidecars_h2).await?;
    node.state.store_undecided_block_data(height2, round2, bytes_h2.clone()).await?;
    let cert_h2 = CommitCertificate {
        height: height2,
        round: round2,
        value_id: proposed_h2.value.id(),
        commit_signatures: Vec::new(),
    };
    let mut notifier_h2 = MockExecutionNotifier::default();
    node.state.process_decided_certificate(&cert_h2, bytes_h2.clone(), &mut notifier_h2).await?;

    let metrics_after_h2 = node.blob_metrics.snapshot();
    assert_eq!(metrics_after_h2.blobs_per_block, 2);
    assert_eq!(metrics_after_h2.lifecycle_promoted, metrics_after_h1.lifecycle_promoted + 2);
    assert_eq!(
        metrics_after_h2.storage_bytes_decided,
        (3 * BYTES_PER_BLOB) as i64,
        "total decided storage should reflect 3 blobs (1 from height 0, 2 from height 2)"
    );

    // Availability checks.
    let blobs_h0 = node.state.blob_engine().get_for_import(Height::new(0)).await?;
    assert_eq!(blobs_h0.len(), 1);
    let blobs_h1 = node.state.blob_engine().get_for_import(Height::new(1)).await?;
    assert!(blobs_h1.is_empty(), "blobless height should have no decided blobs");
    let blobs_h2 = node.state.blob_engine().get_for_import(Height::new(2)).await?;
    assert_eq!(blobs_h2.len(), 2);

    assert_eq!(node.state.current_height, Height::new(3));

    Ok(())
}
