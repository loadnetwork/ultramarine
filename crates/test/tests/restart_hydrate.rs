//! Restart hydration integration test harness.
//!
//! Validates that blob metadata and parent-root caches survive process
//! restarts by reusing on-disk state and invoking hydration paths.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn restart_hydrate() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis,
        mocks::{MockEngineApi, MockExecutionNotifier},
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_execution::EngineApi;
    use ultramarine_types::height::Height;

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    {
        let mut first = build_state(&dirs, &genesis, validator, Height::new(0))?;
        first.state.seed_genesis_blob_metadata().await?;
        first.state.hydrate_blob_parent_root().await?;

        let height = Height::new(0);
        let raw_payload = sample_execution_payload_v3_for_height(height);
        let raw_bundle = sample_blob_bundle(1);
        let payload_id = common::payload_id(1);
        let mock_engine = MockEngineApi::default().with_payload(
            payload_id,
            raw_payload.clone(),
            Some(raw_bundle.clone()),
        );
        let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
        let bundle = maybe_bundle.expect("bundle");
        let payload_bytes = Bytes::from(payload.as_ssz_bytes());
        let round = Round::new(0);
        let round_i64 = round.as_i64();

        let proposed = first
            .state
            .propose_value_with_blobs(height, round, payload_bytes.clone(), &payload, Some(&bundle))
            .await?;

        let (_signed_header, sidecars) =
            first.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

        first.state.blob_engine().verify_and_store(height, round_i64, &sidecars).await?;

        first.state.store_undecided_block_data(height, round, payload_bytes.clone()).await?;

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposed.value.id(),
            commit_signatures: Vec::new(),
        };
        let mut notifier = MockExecutionNotifier::default();
        first
            .state
            .process_decided_certificate(&certificate, payload_bytes.clone(), &mut notifier)
            .await?;
        // Scope ends here to drop RocksDB handles before restart.
    }

    let mut restarted = build_state(&dirs, &genesis, validator, Height::new(0))?;
    restarted.state.hydrate_blob_parent_root().await?;

    let height = Height::new(0);
    let metadata = restarted.state.get_blob_metadata(height).await?;
    let decided = metadata.expect("blob metadata should persist across restart");

    let imported = restarted.state.blob_engine().get_for_import(height).await?;
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].kzg_commitment, decided.blob_kzg_commitments()[0]);

    let cached_parent = restarted.state.blob_parent_root();
    assert_eq!(cached_parent, decided.to_beacon_header().hash_tree_root());

    Ok(())
}
