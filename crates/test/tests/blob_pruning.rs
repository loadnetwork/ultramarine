//! Pruning behavior integration test.
//!
//! Commits eight consecutive blobbed heights and verifies that the blob store
//! prunes blobs older than the five-height retention window while keeping
//! metrics consistent.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_pruning_retains_recent_heights() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis, mocks::MockExecutionNotifier, sample_blob_bundle,
        sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    const TOTAL_HEIGHTS: usize = 8; // produce more than retention window (5)

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_state(&dirs, &genesis, validator, Height::new(0))?;
    node.state.seed_genesis_blob_metadata().await?;
    node.state.hydrate_blob_parent_root().await?;

    for height_idx in 0..TOTAL_HEIGHTS {
        let height = Height::new(height_idx as u64);
        node.state.current_height = height;
        let round = Round::new(0);
        node.state.current_round = round;

        let payload = sample_execution_payload_v3_for_height(height);
        let bundle = sample_blob_bundle(1);
        let bytes = Bytes::from(payload.as_ssz_bytes());

        let proposed = node
            .state
            .propose_value_with_blobs(height, round, bytes.clone(), &payload, Some(&bundle))
            .await?;
        let (_header, sidecars) =
            node.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

        node.state.blob_engine().verify_and_store(height, round.as_i64(), &sidecars).await?;
        node.state.store_undecided_block_data(height, round, bytes.clone()).await?;

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposed.value.id(),
            commit_signatures: Vec::new(),
        };
        let mut notifier = MockExecutionNotifier::default();
        node.state.process_decided_certificate(&certificate, bytes.clone(), &mut notifier).await?;
    }

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.lifecycle_promoted, TOTAL_HEIGHTS as u64);
    assert_eq!(metrics.lifecycle_pruned, 2, "heights 0 and 1 should be pruned");

    // Storage should reflect only the retained heights (TOTAL_HEIGHTS - 2).
    assert_eq!(metrics.storage_bytes_decided, ((TOTAL_HEIGHTS - 2) * BYTES_PER_BLOB) as i64);

    // Heights 0 and 1 should have no decided blobs after pruning.
    for height in 0..2 {
        let decided = node.state.blob_engine().get_for_import(Height::new(height)).await?;
        assert!(decided.is_empty(), "height {} should be pruned", height);
    }

    // Heights 2..7 should remain accessible.
    for height in 2..TOTAL_HEIGHTS {
        let decided = node.state.blob_engine().get_for_import(Height::new(height as u64)).await?;
        assert_eq!(decided.len(), 1, "height {} should retain its blob", height);
    }

    // Current height should have advanced beyond the last committed height.
    assert_eq!(node.state.current_height, Height::new(TOTAL_HEIGHTS as u64));

    Ok(())
}
