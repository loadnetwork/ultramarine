//! Multi-height restart hydration integration test.
//!
//! Commits three consecutive heights (blobbed, blobless, blobbed) and verifies
//! that after a restart the node can rebuild blob sidecars for every height.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_restart_hydrates_multiple_heights() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, build_state, make_genesis, mocks::MockExecutionNotifier,
        propose_with_optional_blobs, sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::height::Height;

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let validator_address = validator.address();
    let dirs = TestDirs::new();

    {
        let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

        let scenarios = [
            (Height::new(0), Some(sample_blob_bundle(1))),
            (Height::new(1), None),
            (Height::new(2), Some(sample_blob_bundle(2))),
        ];

        for (height, maybe_bundle) in scenarios.into_iter() {
            let payload = sample_execution_payload_v3_for_height(height, maybe_bundle.as_ref());
            let (proposed, bytes, sidecars) = propose_with_optional_blobs(
                &mut node.state,
                height,
                Round::new(0),
                &payload,
                maybe_bundle.as_ref(),
            )
            .await?;

            if let Some(ref sidecars) = sidecars {
                node.state.blob_engine().verify_and_store(height, 0, sidecars).await?;
            }

            node.state.store_undecided_block_data(height, Round::new(0), bytes.clone()).await?;

            let certificate = CommitCertificate {
                height,
                round: Round::new(0),
                value_id: proposed.value.id(),
                commit_signatures: Vec::new(),
            };
            let mut notifier = MockExecutionNotifier::default();
            node.state.process_decided_certificate(&certificate, bytes, &mut notifier).await?;
        }
    }

    let mut restarted = build_state(&dirs, &genesis, validator, Height::new(0))?;
    restarted.state.hydrate_blob_parent_root().await?;

    // Height 0: single blob.
    let metadata_h0 =
        restarted.state.get_blob_metadata(Height::new(0)).await?.expect("metadata for height 0");
    assert_eq!(metadata_h0.blob_count(), 1);
    let decided_h0 = restarted.state.blob_engine().get_for_import(Height::new(0)).await?;
    assert_eq!(decided_h0.len(), 1);
    let rebuilt_h0 = restarted.state.rebuild_blob_sidecars_for_restream(
        &metadata_h0,
        &validator_address,
        &decided_h0,
    )?;
    assert_eq!(rebuilt_h0.len(), 1);

    // Height 1: blobless.
    let metadata_h1 =
        restarted.state.get_blob_metadata(Height::new(1)).await?.expect("metadata for height 1");
    assert_eq!(metadata_h1.blob_count(), 0);
    let decided_h1 = restarted.state.blob_engine().get_for_import(Height::new(1)).await?;
    assert!(decided_h1.is_empty());

    // Height 2: two blobs.
    let metadata_h2 =
        restarted.state.get_blob_metadata(Height::new(2)).await?.expect("metadata for height 2");
    assert_eq!(metadata_h2.blob_count(), 2);
    let decided_h2 = restarted.state.blob_engine().get_for_import(Height::new(2)).await?;
    assert_eq!(decided_h2.len(), 2);
    let rebuilt_h2 = restarted.state.rebuild_blob_sidecars_for_restream(
        &metadata_h2,
        &validator_address,
        &decided_h2,
    )?;
    assert_eq!(rebuilt_h2.len(), 2);

    // Parent root should match latest decided metadata.
    assert_eq!(restarted.state.blob_parent_root(), metadata_h2.to_beacon_header().hash_tree_root(),);

    Ok(())
}
