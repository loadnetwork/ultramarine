//! Multi-validator restream integration test.
//!
//! Validates that a follower node can restream blob sidecars from a proposer,
//! verify them via `received_proposal_part`, and commit the block with all blob
//! metadata and metrics intact.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_restream_multi_validator() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis,
        mocks::{MockEngineApi, MockExecutionNotifier},
        propose_with_optional_blobs, sample_blob_bundle, sample_execution_payload_v3_for_height,
        test_peer_id,
    };
    use malachitebft_app_channel::app::{
        streaming::StreamMessage,
        types::core::{CommitCertificate, Round},
    };
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_execution::EngineApi;
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height};

    let (genesis, validators) = make_genesis(2);
    let proposer_key = &validators[0];
    let follower_key = &validators[1];

    let proposer_dirs = TestDirs::new();
    let follower_dirs = TestDirs::new();

    let mut proposer =
        build_seeded_state(&proposer_dirs, &genesis, proposer_key, Height::new(0)).await?;

    let mut follower =
        build_seeded_state(&follower_dirs, &genesis, follower_key, Height::new(0)).await?;

    let height = Height::new(0);
    let raw_bundle = sample_blob_bundle(1);
    let raw_payload = sample_execution_payload_v3_for_height(height, Some(&raw_bundle));
    let payload_id = common::payload_id(3);
    let mock_engine = MockEngineApi::default().with_payload(
        payload_id,
        raw_payload.clone(),
        Some(raw_bundle.clone()),
    );
    let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
    let bundle = maybe_bundle.expect("bundle");
    let round = Round::new(0);
    let round_i64 = round.as_i64();

    let (proposed, payload_bytes, maybe_sidecars) =
        propose_with_optional_blobs(&mut proposer.state, height, round, &payload, Some(&bundle))
            .await?;
    let sidecars = maybe_sidecars.expect("sidecars expected");

    if !sidecars.is_empty() {
        proposer.state.blob_engine().verify_and_store(height, round_i64, &sidecars).await?;
    }
    proposer.state.store_undecided_block_data(height, round, payload_bytes.clone()).await?;

    let stream_messages: Vec<StreamMessage<_>> = proposer
        .state
        .stream_proposal(proposed.clone(), payload_bytes.clone(), Some(&sidecars), None)
        .collect();

    let mut received = None;
    for msg in stream_messages {
        let peer_id = test_peer_id(1);
        if let Some(value) = follower.state.received_proposal_part(peer_id, msg).await? {
            received = Some(value);
        }
    }
    let received = received.expect("follower should reconstruct proposal");

    let certificate = CommitCertificate {
        height,
        round,
        value_id: received.value.id(),
        commit_signatures: Vec::new(),
    };
    let follower_payload =
        follower.state.get_block_data(height, round).await.unwrap_or_else(|| payload_bytes.clone());

    let mut follower_notifier = MockExecutionNotifier::default();
    let follower_outcome = follower
        .state
        .process_decided_certificate(&certificate, follower_payload, &mut follower_notifier)
        .await?;

    let imported = follower.state.blob_engine().get_for_import(height).await?;
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].kzg_commitment, bundle.commitments[0]);
    assert_eq!(follower.state.current_height, Height::new(1));

    assert_eq!(follower_outcome.blob_count, sidecars.len());
    assert_eq!(follower_notifier.new_block_calls.lock().unwrap().len(), 1);

    let metrics = follower.blob_metrics.snapshot();
    assert_eq!(metrics.verifications_success, sidecars.len() as u64);
    assert_eq!(metrics.verifications_failure, 0);
    assert_eq!(metrics.storage_bytes_undecided, 0);
    assert_eq!(metrics.storage_bytes_decided, (sidecars.len() * BYTES_PER_BLOB) as i64);
    assert_eq!(metrics.lifecycle_promoted, sidecars.len() as u64);

    // Ensure proposer can also commit successfully.
    let proposer_payload =
        proposer.state.get_block_data(height, round).await.unwrap_or_else(|| payload_bytes.clone());
    let mut proposer_notifier = MockExecutionNotifier::default();
    proposer
        .state
        .process_decided_certificate(&certificate, proposer_payload, &mut proposer_notifier)
        .await?;
    let proposer_blobs = proposer.state.blob_engine().get_for_import(height).await?;
    assert_eq!(proposer_blobs.len(), sidecars.len());
    assert_eq!(proposer.state.current_height, Height::new(1));
    assert_eq!(proposer_notifier.new_block_calls.lock().unwrap().len(), 1);

    Ok(())
}
