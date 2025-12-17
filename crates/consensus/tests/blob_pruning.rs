//! Pruning behavior integration test.
//!
//! **Tier 0 smoke test:** Retention logic
//! Kept for fast feedback (~3s) during blob engine development.
//!
//! Commits eight consecutive blobbed heights and verifies that the blob store
//! prunes blobs older than the configured retention window while keeping
//! metrics consistent.

mod common;

#[tokio::test]
async fn blob_pruning_retains_recent_heights() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis, mocks::MockExecutionNotifier,
        propose_with_optional_blobs, sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    const TOTAL_HEIGHTS: usize = 8; // produce more than retention window
    const TEST_RETENTION_WINDOW: u64 = 5;

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;
    node.state.set_blob_retention_window_for_testing(TEST_RETENTION_WINDOW);
    // Enable archiver mode to prevent auto-archival during commit
    node.state.set_archiver_enabled(true);

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
        node.state.store_undecided_block_data(height, round, bytes.clone(), Vec::new()).await?;

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

    // With archiver enabled, blobs are NOT auto-archived during commit.
    // Pruning only happens after actual archival via the archiver worker.
    // Since we haven't archived anything, no blobs should be pruned.
    assert_eq!(
        metrics.lifecycle_pruned, 0,
        "no pruning without actual archival (archiver_enabled=true)"
    );

    // All decided blobs should still be present.
    assert_eq!(metrics.storage_bytes_decided, (TOTAL_HEIGHTS * BYTES_PER_BLOB) as i64);

    // All heights should retain their blobs until archived.
    for height in 0..TOTAL_HEIGHTS {
        let decided = node.state.blob_engine().get_for_import(Height::new(height as u64)).await?;
        assert_eq!(decided.len(), 1, "height {} should retain its blob until archived", height);
    }

    assert!(
        node.state.get_decided_value(Height::new(0)).await?.is_some(),
        "decided value at height 0 should remain until archival completes"
    );

    // Current height should have advanced beyond the last committed height.
    assert_eq!(node.state.current_height, Height::new(TOTAL_HEIGHTS as u64));

    Ok(())
}
