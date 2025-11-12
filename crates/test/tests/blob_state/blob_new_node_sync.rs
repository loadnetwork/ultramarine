//! New node join & sync integration test.
//!
//! Simulates two validators producing a blobbed block and a third validator
//! joining afterwards. The newcomer receives the synced package (execution
//! payload + blobs + metadata) and must promote the blobs, commit the block,
//! and expose the same metrics/import surface as the original participants.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_new_node_sync() -> color_eyre::Result<()> {
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
    use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height, sync::SyncedValuePackage};

    let (genesis, validators) = make_genesis(3);
    let proposer_key = &validators[0];
    let follower_key = &validators[1];
    let newcomer_key = &validators[2];

    let proposer_dirs = TestDirs::new();
    let follower_dirs = TestDirs::new();
    let newcomer_dirs = TestDirs::new();

    let mut proposer =
        build_seeded_state(&proposer_dirs, &genesis, proposer_key, Height::new(0)).await?;

    let mut follower =
        build_seeded_state(&follower_dirs, &genesis, follower_key, Height::new(0)).await?;

    let mut newcomer =
        build_seeded_state(&newcomer_dirs, &genesis, newcomer_key, Height::new(0)).await?;

    let height = Height::new(0);
    let round = Round::new(0);
    let round_i64 = round.as_i64();

    let bundle = sample_blob_bundle(1);
    let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));

    let payload_id = common::payload_id(42);
    let mock_engine =
        MockEngineApi::default().with_payload(payload_id, payload.clone(), Some(bundle.clone()));
    let (_payload_echo, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
    let sidecars_bundle = maybe_bundle.expect("blob bundle present");

    let (proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
        &mut proposer.state,
        height,
        round,
        &payload,
        Some(&sidecars_bundle),
    )
    .await?;
    let sidecars = maybe_sidecars.expect("sidecars present");

    let stream_messages: Vec<StreamMessage<_>> = proposer
        .state
        .stream_proposal(proposed, payload_bytes.clone(), Some(sidecars.as_slice()), None)
        .collect();

    let mut received = None;
    for msg in stream_messages {
        let peer_id = test_peer_id(7);
        if let Some(value) = follower.state.received_proposal_part(peer_id, msg).await? {
            received = Some(value);
        }
    }
    let received = received.expect("follower reconstructed proposal");

    follower.state.store_synced_block_data(height, round, payload_bytes.clone()).await?;
    follower.state.store_synced_proposal(received.clone()).await?;

    follower.state.blob_engine().verify_and_store(height, round_i64, &sidecars).await?;
    follower.state.blob_engine().mark_decided(height, round_i64).await?;

    let certificate = CommitCertificate {
        height,
        round,
        value_id: received.value.id(),
        commit_signatures: Vec::new(),
    };
    let follower_payload =
        follower.state.get_block_data(height, round).await.unwrap_or_else(|| payload_bytes.clone());
    let mut follower_notifier = MockExecutionNotifier::default();
    follower
        .state
        .process_decided_certificate(&certificate, follower_payload, &mut follower_notifier)
        .await?;

    // --- New node joins and consumes synced data via shared handler ---
    let newcomer_package = SyncedValuePackage::Full {
        value: received.value.clone(),
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: sidecars.clone(),
    };

    let proposed_from_sync = newcomer
        .state
        .process_synced_package(height, round, received.proposer.clone(), newcomer_package)
        .await?
        .expect("synced package should yield proposed value");

    let newcomer_payload =
        newcomer.state.get_block_data(height, round).await.unwrap_or_else(|| payload_bytes.clone());
    let mut newcomer_notifier = MockExecutionNotifier::default();
    newcomer
        .state
        .process_decided_certificate(&certificate, newcomer_payload, &mut newcomer_notifier)
        .await?;

    let imported = newcomer.state.blob_engine().get_for_import(height).await?;
    assert_eq!(imported.len(), sidecars.len());
    assert_eq!(imported[0].kzg_commitment, sidecars[0].kzg_commitment);

    let newcomer_metadata = newcomer.state.get_blob_metadata(height).await?;
    let metadata = newcomer_metadata.expect("blob metadata should be stored after sync commit");
    assert_eq!(metadata.blob_kzg_commitments.len(), sidecars.len());
    assert_eq!(metadata.blob_kzg_commitments()[0], sidecars[0].kzg_commitment);
    assert_eq!(
        proposed_from_sync.value.id(),
        received.value.id(),
        "synced proposal should match original value"
    );

    let metrics = newcomer.blob_metrics.snapshot();
    assert_eq!(metrics.verifications_success, sidecars.len() as u64);
    assert_eq!(metrics.lifecycle_promoted, sidecars.len() as u64);
    assert_eq!(metrics.lifecycle_dropped, 0);
    assert_eq!(metrics.sync_failures, 0);
    assert_eq!(metrics.storage_bytes_decided, (sidecars.len() * BYTES_PER_BLOB) as i64);

    assert_eq!(newcomer.state.current_height, Height::new(1));

    Ok(())
}
