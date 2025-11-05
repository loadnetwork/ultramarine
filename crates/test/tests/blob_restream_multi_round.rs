//! Multi-round restream integration test with round cleanup.
//!
//! Covers the scenario where a proposer emits blobbed proposals for multiple
//! rounds of the same height and ensures followers drop losing rounds while
//! keeping metrics consistent.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
#[ignore = "integration test - run with: cargo test -p ultramarine-test -- --ignored"]
async fn blob_restream_multiple_rounds() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use common::{
        TestDirs, build_state, make_genesis,
        mocks::{MockEngineApi, MockExecutionNotifier},
        sample_blob_bundle, sample_execution_payload_v3_for_height, test_peer_id,
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

    let mut proposer = build_state(&proposer_dirs, &genesis, proposer_key, Height::new(0))?;
    proposer.state.seed_genesis_blob_metadata().await?;
    proposer.state.hydrate_blob_parent_root().await?;

    let mut follower = build_state(&follower_dirs, &genesis, follower_key, Height::new(0))?;
    follower.state.seed_genesis_blob_metadata().await?;
    follower.state.hydrate_blob_parent_root().await?;

    let height = Height::new(0);
    let rounds = [Round::new(0), Round::new(1)];
    let payload_ids = [common::payload_id(6), common::payload_id(7)];

    let raw_payloads = [
        sample_execution_payload_v3_for_height(height),
        sample_execution_payload_v3_for_height(height),
    ];
    let raw_bundles = [sample_blob_bundle(1), sample_blob_bundle(1)];

    let mut mock_engine = MockEngineApi::default();
    for ((payload_id, payload), bundle) in
        payload_ids.iter().zip(raw_payloads.iter()).zip(raw_bundles.iter())
    {
        mock_engine = mock_engine.with_payload(*payload_id, payload.clone(), Some(bundle.clone()));
    }

    let mut total_success = 0usize;
    let mut dropped_count = 0usize;
    let mut promoted_count = 0usize;
    let mut winning_proposal = None;
    let mut winning_payload_bytes = None;

    for (idx, round) in rounds.iter().enumerate() {
        let payload_id = payload_ids[idx];
        proposer.state.current_round = *round;
        follower.state.current_round = *round;
        let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
        let bundle = maybe_bundle.expect("bundle");
        let payload_bytes = Bytes::from(payload.as_ssz_bytes());

        let proposed = proposer
            .state
            .propose_value_with_blobs(
                height,
                *round,
                payload_bytes.clone(),
                &payload,
                Some(&bundle),
            )
            .await?;

        let (_signed_header, sidecars) =
            proposer.state.prepare_blob_sidecar_parts(&proposed, Some(&bundle))?;

        if !sidecars.is_empty() {
            proposer
                .state
                .blob_engine()
                .verify_and_store(height, round.as_i64(), &sidecars)
                .await?;
        }
        proposer.state.store_undecided_block_data(height, *round, payload_bytes.clone()).await?;

        total_success += sidecars.len();
        if round.as_u32() == Some(0) {
            dropped_count = sidecars.len();
        } else {
            promoted_count = sidecars.len();
        }

        let stream_messages: Vec<StreamMessage<_>> = proposer
            .state
            .stream_proposal(proposed, payload_bytes.clone(), Some(sidecars.as_slice()), None)
            .collect();

        let mut received = None;
        for msg in stream_messages {
            if let Some(value) = follower
                .state
                .received_proposal_part(
                    test_peer_id(round.as_u32().expect("round") as u8 + 20),
                    msg,
                )
                .await?
            {
                received = Some(value);
            }
        }
        let received = received.expect("follower should reconstruct proposal");
        if round.as_u32() == Some(1) {
            winning_proposal = Some(received);
            winning_payload_bytes = Some(payload_bytes.clone());
        }
    }

    let winning_round = rounds[1];
    let certificate = CommitCertificate {
        height,
        round: winning_round,
        value_id: winning_proposal.as_ref().expect("winning proposal stored").value.id(),
        commit_signatures: Vec::new(),
    };
    let payload_bytes = winning_payload_bytes.expect("winning payload bytes tracked");

    let follower_payload = follower
        .state
        .get_block_data(height, winning_round)
        .await
        .unwrap_or_else(|| payload_bytes.clone());
    let mut follower_notifier = MockExecutionNotifier::default();
    let follower_outcome = follower
        .state
        .process_decided_certificate(&certificate, follower_payload, &mut follower_notifier)
        .await?;

    let imported = follower.state.blob_engine().get_for_import(height).await?;

    let metrics = follower.blob_metrics.snapshot();
    assert_eq!(metrics.verifications_success, total_success as u64);

    let undecided =
        follower.state.blob_engine().get_undecided_blobs(height, rounds[0].as_i64()).await?;
    assert!(undecided.is_empty(), "losing round blobs should be dropped");
    assert_eq!(follower.state.current_height, Height::new(1));
    assert_eq!(follower_outcome.blob_count, promoted_count);
    assert!(
        imported.len() >= follower_outcome.blob_count,
        "expected at least {} decided blobs, found {}",
        follower_outcome.blob_count,
        imported.len()
    );
    assert_eq!(follower_notifier.new_block_calls.lock().unwrap().len(), 1);

    let proposer_payload = proposer
        .state
        .get_block_data(height, winning_round)
        .await
        .unwrap_or_else(|| payload_bytes.clone());
    let mut proposer_notifier = MockExecutionNotifier::default();
    proposer
        .state
        .process_decided_certificate(&certificate, proposer_payload, &mut proposer_notifier)
        .await?;
    assert_eq!(proposer.state.current_height, Height::new(1));
    assert_eq!(proposer_notifier.new_block_calls.lock().unwrap().len(), 1);

    // Verify proposer also cleaned up losing round blobs (Bug #1 fix verification)
    let proposer_undecided =
        proposer.state.blob_engine().get_undecided_blobs(height, rounds[0].as_i64()).await?;
    assert!(
        proposer_undecided.is_empty(),
        "proposer should also drop losing round blobs during commit"
    );

    Ok(())
}
