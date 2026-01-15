//! Sync parent root integration tests.
//!
//! Ensures sync verification uses store-derived parent roots, even when
//! the in-memory cache is stale (e.g., after restart without hydration).

mod common;

#[tokio::test]
async fn blob_sync_uses_store_parent_root_over_cache() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, build_state, make_genesis, propose_with_optional_blobs,
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ultramarine_types::{height::Height, sync::SyncedValuePackage};

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();
    let round = Round::new(0);

    let (package, expected_parent_root) = {
        let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

        // Commit height 1 (blobless) so parent metadata exists in the store.
        let height_1 = Height::new(1);
        let payload_h1 = sample_execution_payload_v3_for_height(height_1, None);
        let (proposed_h1, payload_bytes_h1, _sidecars_h1) =
            propose_with_optional_blobs(&mut node.state, height_1, round, &payload_h1, None)
                .await?;

        node.state
            .store_undecided_block_data(height_1, round, payload_bytes_h1.clone(), Vec::new())
            .await?;

        let certificate_h1 = CommitCertificate {
            height: height_1,
            round,
            value_id: proposed_h1.value.id(),
            commit_signatures: Vec::new(),
        };
        let mut notifier = common::mocks::MockExecutionNotifier::default();
        node.state
            .process_decided_certificate(&certificate_h1, payload_bytes_h1, &mut notifier)
            .await?;

        let parent_metadata =
            node.state.get_blob_metadata(height_1).await?.expect("parent metadata");
        let expected_parent_root = parent_metadata.to_beacon_header().hash_tree_root();

        // Build a blobbed sync package for height 2.
        let height_2 = Height::new(2);
        let bundle = sample_blob_bundle(1);
        let payload_h2 = sample_execution_payload_v3_for_height(height_2, Some(&bundle));
        let (proposed_h2, payload_bytes_h2, maybe_sidecars_h2) = propose_with_optional_blobs(
            &mut node.state,
            height_2,
            round,
            &payload_h2,
            Some(&bundle),
        )
        .await?;
        let sidecars = maybe_sidecars_h2.expect("sidecars expected");

        let package = SyncedValuePackage::Full {
            value: proposed_h2.value.clone(),
            execution_payload_ssz: payload_bytes_h2,
            blob_sidecars: sidecars,
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
        };

        (package, expected_parent_root)
    };

    // Simulate restart: open the same store without hydrating the cache.
    let mut harness = build_state(&dirs, &genesis, validator, Height::new(0))?;
    let state = &mut harness.state;

    assert_ne!(state.blob_parent_root(), expected_parent_root, "cache should be stale before sync");

    let encoded = package.encode().map_err(|e| color_eyre::eyre::eyre!(e))?;
    let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| color_eyre::eyre::eyre!(e))?;

    let height_2 = Height::new(2);
    let result =
        state.process_synced_package(height_2, round, validator.address(), decoded).await?;
    assert!(result.is_some(), "sync should succeed");

    let synced_metadata = state.get_blob_metadata(height_2).await?.expect("synced metadata");
    assert_eq!(
        synced_metadata.parent_blob_root(),
        expected_parent_root,
        "sync should use store-derived parent root"
    );

    Ok(())
}
