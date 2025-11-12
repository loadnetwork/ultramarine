//! Multi-height state sync & restart test.
//!
//! Produces three consecutive heights with mixed blob presence, restarts the
//! follower, and ensures decided blobs and metadata survive restarts when
//! ingested via the sync handler.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_sync_across_restart_multiple_heights() -> color_eyre::Result<()> {
    use alloy_primitives::B256;
    use common::{
        TestDirs, build_seeded_state, build_state, make_genesis, mocks::MockExecutionNotifier,
        propose_with_optional_blobs, sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{
        blob::BYTES_PER_BLOB, blob_metadata::BlobMetadata, engine_api::ExecutionPayloadHeader,
        height::Height, sync::SyncedValuePackage,
    };

    let (genesis, validators) = make_genesis(2);
    let proposer_key = &validators[0];
    let follower_key = &validators[1];
    let follower_address = follower_key.address();
    let proposer_dirs = TestDirs::new();
    let follower_dirs = TestDirs::new();

    {
        let mut proposer =
            build_seeded_state(&proposer_dirs, &genesis, proposer_key, Height::new(0)).await?;

        let mut follower =
            build_seeded_state(&follower_dirs, &genesis, follower_key, Height::new(0)).await?;

        let configurations =
            [(Height::new(0), 1usize), (Height::new(1), 0usize), (Height::new(2), 2usize)];

        for (height, blob_count) in configurations {
            let round = Round::new(0);

            let mut bundle =
                if blob_count == 0 { None } else { Some(sample_blob_bundle(blob_count)) };
            let payload = sample_execution_payload_v3_for_height(height, bundle.as_ref());
            let (proposed, bytes, maybe_sidecars) = propose_with_optional_blobs(
                &mut proposer.state,
                height,
                round,
                &payload,
                bundle.as_ref(),
            )
            .await?;

            let sidecars = if let (Some(ref bundle), Some(sidecars)) =
                (bundle.as_ref(), maybe_sidecars.as_ref())
            {
                let parent_root = if height.as_u64() == 0 {
                    B256::ZERO
                } else {
                    follower.state.blob_parent_root()
                };
                let header = ExecutionPayloadHeader::from_payload(&payload);
                let proposer_index = Some(0); // deterministic genesis ordering
                let metadata = BlobMetadata::new(
                    height,
                    parent_root,
                    bundle.commitments.clone(),
                    header,
                    proposer_index,
                );

                Some(
                    proposer
                        .state
                        .rebuild_blob_sidecars_for_restream(
                            &metadata,
                            &proposer_key.address(),
                            sidecars,
                        )
                        .expect("rebuild sidecars with correct parent root"),
                )
            } else {
                None
            };

            let package = SyncedValuePackage::Full {
                value: proposed.value.clone(),
                execution_payload_ssz: bytes.clone(),
                blob_sidecars: sidecars.clone().unwrap_or_default(),
            };

            let reconstructed_opt = follower
                .state
                .process_synced_package(height, round, proposer_key.address(), package)
                .await?;
            let reconstructed = reconstructed_opt.unwrap_or_else(|| {
                let metrics = follower.blob_metrics.snapshot();
                panic!(
                    "sync package should yield proposal at height {} with {} blobs (sync_failures={}, verifications_failure={}, verifications_success={})",
                    height.as_u64(),
                    blob_count,
                    metrics.sync_failures,
                    metrics.verifications_failure,
                    metrics.verifications_success
                )
            });

            let certificate = CommitCertificate {
                height,
                round,
                value_id: reconstructed.value.id(),
                commit_signatures: Vec::new(),
            };
            let follower_payload =
                follower.state.get_block_data(height, round).await.unwrap_or_else(|| bytes.clone());
            let mut notifier = MockExecutionNotifier::default();
            follower
                .state
                .process_decided_certificate(&certificate, follower_payload, &mut notifier)
                .await?;
        }

        let metrics = follower.blob_metrics.snapshot();
        assert_eq!(metrics.lifecycle_promoted, 3);
        assert_eq!(metrics.lifecycle_dropped, 0);
        assert_eq!(metrics.storage_bytes_decided, (3 * BYTES_PER_BLOB) as i64);
        assert_eq!(follower.state.current_height, Height::new(3));
    }

    // Restart follower from the persisted directories.
    let mut restarted = build_state(&follower_dirs, &genesis, follower_key, Height::new(0))?;
    restarted.state.hydrate_blob_parent_root().await?;

    // Height 0 (blobbed)
    let metadata_h0 = restarted
        .state
        .get_blob_metadata(Height::new(0))
        .await?
        .expect("metadata should exist for height 0");
    assert_eq!(metadata_h0.blob_count(), 1);
    let stored_h0 = restarted.state.blob_engine().get_for_import(Height::new(0)).await?;
    assert_eq!(stored_h0.len(), 1);
    let rebuilt_h0 = restarted.state.rebuild_blob_sidecars_for_restream(
        &metadata_h0,
        &follower_address,
        &stored_h0,
    )?;
    assert_eq!(rebuilt_h0.len(), 1);

    // Height 1 (blobless)
    let metadata_h1 = restarted
        .state
        .get_blob_metadata(Height::new(1))
        .await?
        .expect("metadata should exist for blobless height");
    assert_eq!(metadata_h1.blob_count(), 0);
    let stored_h1 = restarted.state.blob_engine().get_for_import(Height::new(1)).await?;
    assert!(stored_h1.is_empty());

    // Height 2 (two blobs)
    let metadata_h2 = restarted
        .state
        .get_blob_metadata(Height::new(2))
        .await?
        .expect("metadata should exist for height 2");
    assert_eq!(metadata_h2.blob_count(), 2);
    let stored_h2 = restarted.state.blob_engine().get_for_import(Height::new(2)).await?;
    assert_eq!(stored_h2.len(), 2);
    let rebuilt_h2 = restarted.state.rebuild_blob_sidecars_for_restream(
        &metadata_h2,
        &follower_address,
        &stored_h2,
    )?;
    assert_eq!(rebuilt_h2.len(), 2);

    // Parent root should match last decided metadata.
    let parent_root = restarted.state.blob_parent_root();
    assert_eq!(
        parent_root,
        metadata_h2.to_beacon_header().hash_tree_root(),
        "parent root should reflect latest decided blob metadata"
    );

    Ok(())
}
