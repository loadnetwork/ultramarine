//! Pruning behavior integration test.
//!
//! Commits eight consecutive blobbed heights and verifies that the blob store
//! prunes blobs older than the configured retention window while keeping
//! metrics consistent.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_pruning_retains_recent_heights() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis, mocks::MockExecutionNotifier,
        propose_with_optional_blobs, sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    const TOTAL_HEIGHTS: usize = 8; // produce more than retention window
    const TEST_RETENTION_WINDOW: u64 = 5;

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;
    node.state.set_blob_retention_window_for_testing(TEST_RETENTION_WINDOW);

    for height_idx in 0..TOTAL_HEIGHTS {
        let height = Height::new(height_idx as u64);
        let round = Round::new(0);

        let bundle = sample_blob_bundle(1);
        let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
        let (proposed, bytes, maybe_sidecars) =
            propose_with_optional_blobs(&mut node.state, height, round, &payload, Some(&bundle))
                .await?;
        let sidecars = maybe_sidecars.expect("blobbed proposal should yield sidecars");

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
    let retention_window = TEST_RETENTION_WINDOW as usize;
    let expected_pruned = TOTAL_HEIGHTS.saturating_sub(retention_window);
    assert_eq!(metrics.lifecycle_promoted, TOTAL_HEIGHTS as u64);
    assert_eq!(
        metrics.lifecycle_pruned, expected_pruned as u64,
        "pruning metric should reflect heights beyond retention window"
    );

    // Storage should reflect only the retained heights.
    assert_eq!(
        metrics.storage_bytes_decided,
        ((TOTAL_HEIGHTS - expected_pruned) * BYTES_PER_BLOB) as i64
    );

    // Heights older than retention window should have no decided blobs after pruning.
    for height in 0..expected_pruned {
        let decided = node.state.blob_engine().get_for_import(Height::new(height as u64)).await?;
        assert!(decided.is_empty(), "height {} should be pruned", height);
    }

    // Remaining heights should stay accessible.
    for height in expected_pruned..TOTAL_HEIGHTS {
        let decided = node.state.blob_engine().get_for_import(Height::new(height as u64)).await?;
        assert_eq!(decided.len(), 1, "height {} should retain its blob", height);
    }

    // Current height should have advanced beyond the last committed height.
    assert_eq!(node.state.current_height, Height::new(TOTAL_HEIGHTS as u64));

    Ok(())
}
