//! Execution-layer rejection regression test.
//!
//! Verifies that `State::process_decided_certificate` surfaces an execution-layer
//! rejection (INVALID payload status) and prevents the state transition from
//! finalizing the block.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_decided_el_rejection_blocks_commit() -> color_eyre::Result<()> {
    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
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

    let mut node = build_state(&dirs, &genesis, validator, Height::new(0))?;
    node.state.seed_genesis_blob_metadata().await?;
    node.state.hydrate_blob_parent_root().await?;

    let height = Height::new(0);
    let payload = sample_execution_payload_v3_for_height(height);
    let bundle = sample_blob_bundle(1);
    let payload_id = common::payload_id(99);
    let mock_engine =
        MockEngineApi::default().with_payload(payload_id, payload.clone(), Some(bundle.clone()));
    let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
    let bundle = maybe_bundle.expect("bundle");
    let payload_bytes = Bytes::from(payload.as_ssz_bytes());
    let round = Round::new(0);
    let round_i64 = round.as_i64();

    let proposed = node
        .state
        .propose_value_with_blobs(height, round, payload_bytes.clone(), &payload, Some(&bundle))
        .await?;
    let (_header, sidecars) = node.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

    node.state.blob_engine().verify_and_store(height, round_i64, &sidecars).await?;
    node.state.store_undecided_block_data(height, round, payload_bytes.clone()).await?;

    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposed.value.id(),
        commit_signatures: Vec::new(),
    };

    let invalid_status = PayloadStatus::new(
        PayloadStatusEnum::Invalid { validation_error: "EL rejected payload".into() },
        None,
    );
    let mut notifier = MockExecutionNotifier::new().with_payload_status(invalid_status);

    let before_decided = node.state.get_blob_metadata(height).await?;

    let result = node
        .state
        .process_decided_certificate(&certificate, payload_bytes.clone(), &mut notifier)
        .await;
    assert!(result.is_err(), "invalid payload status should surface as error");

    // Height should remain unchanged because the commit path was aborted.
    assert_eq!(node.state.current_height, Height::new(0));

    // Decided metadata visibility must not change after rejection.
    let after_decided = node.state.get_blob_metadata(height).await?;
    assert_eq!(
        after_decided.is_some(),
        before_decided.is_some(),
        "metadata promotion state must be unchanged after EL rejection"
    );

    // The block should remain undecided.
    assert!(
        node.state.get_decided_value(height).await.is_none(),
        "decided value should not exist after rejection"
    );

    // Execution notifier should have been invoked exactly once.
    assert_eq!(notifier.new_block_calls.lock().unwrap().len(), 1);

    Ok(())
}
