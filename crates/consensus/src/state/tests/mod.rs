mod support;

use std::collections::HashSet;

use alloy_primitives::B256;
use bytes::Bytes as NetworkBytes;
use malachitebft_app_channel::app::types::{
    LocallyProposedValue,
    core::{CommitCertificate, Round, Validity},
};
use ssz::Encode;
use support::*;
use ultramarine_blob_engine::BlobEngine;
use ultramarine_types::{
    aliases::Bytes as BlobBytes,
    blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
    blob_metadata::BlobMetadata,
    engine_api::ExecutionPayloadHeader,
    height::Height,
    sync::SyncedValuePackage,
    validator_set::{Validator, ValidatorSet},
    value::Value,
    value_metadata::ValueMetadata,
};

use super::*;
use crate::{metrics::DbMetrics, store::Store};

#[tokio::test]
async fn hydrate_blob_parent_root_uses_decided_metadata() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

    let metadata = sample_blob_metadata(Height::new(1), B256::from([9u8; 32]));
    state
        .store
        .put_blob_metadata_undecided(Height::new(1), Round::new(0), &metadata)
        .await
        .expect("insert metadata");
    state
        .store
        .mark_blob_metadata_decided(Height::new(1), Round::new(0))
        .await
        .expect("promote metadata");

    state.last_blob_sidecar_root = B256::ZERO;
    state.hydrate_blob_parent_root().await.expect("hydrate");

    let expected = metadata.to_beacon_header().hash_tree_root();
    assert_eq!(state.last_blob_sidecar_root, expected);
}

#[tokio::test]
async fn hydrate_blob_parent_root_seeds_genesis_with_correct_root() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

    let expected = BlobMetadata::genesis().to_beacon_header().hash_tree_root();
    state.hydrate_blob_parent_root().await.expect("hydrate");

    assert_eq!(state.last_blob_sidecar_root, expected);
}

#[tokio::test]
async fn verify_blob_sidecars_roundtrip_canonical_proof() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let height = Height::new(1);
    let round = Round::new(0);

    let commitments = vec![KzgCommitment::new([1u8; 48]), KzgCommitment::new([2u8; 48])];
    let blobs = vec![
        Blob::new(BlobBytes::from(NetworkBytes::from(vec![0u8; BYTES_PER_BLOB]))).expect("blob0"),
        Blob::new(BlobBytes::from(NetworkBytes::from(vec![1u8; BYTES_PER_BLOB]))).expect("blob1"),
    ];
    let proofs = vec![KzgProof::new([3u8; 48]), KzgProof::new([4u8; 48])];

    let bundle = BlobsBundle::new(commitments.clone(), proofs, blobs);
    bundle.validate().expect("bundle valid");

    let metadata = ValueMetadata::new(sample_execution_payload_header(), commitments.clone());
    let value = Value::new(metadata.clone());
    let locally_proposed = LocallyProposedValue::new(height, round, value);

    let (_signed_header, sidecars) = state
        .prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))
        .expect("prepare sidecars");
    assert_eq!(sidecars.len(), commitments.len());

    state
        .verify_blob_sidecars(height, &state.address, &metadata, &sidecars)
        .await
        .expect("canonical proofs pass");

    let mut tampered = sidecars.clone();
    tampered[0].kzg_commitment_inclusion_proof[0] = B256::from([0xFFu8; 32]);

    let err = state
        .verify_blob_sidecars(height, &state.address, &metadata, &tampered)
        .await
        .expect_err("tampered proof rejected");
    assert!(
        err.contains("Invalid KZG inclusion proof"),
        "expected inclusion proof failure, got {err}"
    );
}

#[tokio::test]
async fn cleanup_stale_blob_metadata_removes_lower_entries() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(3));
    state.current_height = Height::new(4);

    let metadata_low = sample_blob_metadata(Height::new(2), B256::from([10u8; 32]));
    state
        .store
        .put_blob_metadata_undecided(Height::new(2), Round::new(0), &metadata_low)
        .await
        .expect("insert");

    let metadata_current = sample_blob_metadata(Height::new(4), B256::from([11u8; 32]));
    state
        .store
        .put_blob_metadata_undecided(Height::new(4), Round::new(1), &metadata_current)
        .await
        .expect("insert");

    state.cleanup_stale_blob_metadata().await.expect("cleanup");

    assert!(
        state
            .store
            .get_blob_metadata_undecided(Height::new(2), Round::new(0))
            .await
            .expect("get")
            .is_none()
    );
    assert!(
        state
            .store
            .get_blob_metadata_undecided(Height::new(4), Round::new(1))
            .await
            .expect("get")
            .is_some()
    );
}

#[tokio::test]
async fn store_undecided_proposal_data_uses_explicit_height_round() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(3));
    state.current_height = Height::new(3);
    state.current_round = Round::new(1);

    let target_height = Height::new(1);
    let target_round = Round::new(0);
    let payload = NetworkBytes::from_static(b"payload-h1r0");

    state
        .store_undecided_proposal_data(target_height, target_round, payload.clone(), Vec::new())
        .await
        .expect("store payload");

    let stored =
        state.store.get_block_data(target_height, target_round).await.expect("load payload");
    assert_eq!(stored, Some(payload), "payload stored at explicit key");

    let wrong_slot = state
        .store
        .get_block_data(state.current_height, state.current_round)
        .await
        .expect("load current slot");
    assert!(wrong_slot.is_none(), "current round remains untouched");
}

#[tokio::test]
async fn load_blob_metadata_for_round_falls_back_to_decided() {
    let mock_engine = MockBlobEngine::default();
    let (state, _tmp) = build_state(mock_engine, Height::new(1));
    let height = Height::new(1);

    let metadata = sample_blob_metadata(height, B256::ZERO);
    state
        .store
        .put_blob_metadata_undecided(height, Round::new(0), &metadata)
        .await
        .expect("insert undecided metadata");
    state.store.mark_blob_metadata_decided(height, Round::new(0)).await.expect("promote metadata");

    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, Round::new(0))
            .await
            .expect("get undecided")
            .is_none()
    );

    let loaded = state
        .load_blob_metadata_for_round(height, Round::new(1))
        .await
        .expect("load metadata")
        .expect("metadata fallback");

    assert_eq!(loaded.height(), metadata.height());
    assert_eq!(loaded.parent_blob_root(), metadata.parent_blob_root());
    assert_eq!(loaded.blob_kzg_commitments(), metadata.blob_kzg_commitments());
}

#[tokio::test]
async fn propose_value_with_blobs_stores_blob_metadata() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));

    let payload = sample_execution_payload_v3();
    let expected_header = ExecutionPayloadHeader::from_payload(&payload, None);
    let bundle = sample_blob_bundle(1);
    let metadata_before =
        state.store.get_blob_metadata_undecided(Height::new(1), Round::new(0)).await.expect("get");
    assert!(metadata_before.is_none());
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

    state
        .propose_value_with_blobs(
            Height::new(1),
            Round::new(0),
            NetworkBytes::new(),
            &payload,
            &[],
            Some(&bundle),
        )
        .await
        .expect("propose");

    let stored = state
        .store
        .get_blob_metadata_undecided(Height::new(1), Round::new(0))
        .await
        .expect("get")
        .expect("metadata");

    assert_eq!(stored.height(), Height::new(1));
    assert_eq!(stored.blob_count(), 1);
    assert_eq!(stored.parent_blob_root(), B256::ZERO);
    assert_eq!(stored.blob_kzg_commitments(), bundle.commitments.as_slice());
    assert_eq!(stored.execution_payload_header(), &expected_header);
    assert_eq!(stored.proposer_index_hint(), Some(0));
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
}

#[tokio::test]
async fn propose_blobless_value_uses_parent_root_hint() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(2));
    let parent_root = B256::from([7u8; 32]);
    state.last_blob_sidecar_root = parent_root;

    let payload = sample_execution_payload_v3();
    let expected_header = ExecutionPayloadHeader::from_payload(&payload, None);

    state
        .propose_value_with_blobs(
            Height::new(2),
            Round::new(0),
            NetworkBytes::new(),
            &payload,
            &[],
            None,
        )
        .await
        .expect("propose blobless");

    let stored = state
        .store
        .get_blob_metadata_undecided(Height::new(2), Round::new(0))
        .await
        .expect("get")
        .expect("metadata");

    assert_eq!(stored.height(), Height::new(2));
    assert_eq!(stored.blob_count(), 0);
    assert_eq!(stored.parent_blob_root(), parent_root);
    assert_eq!(stored.execution_payload_header(), &expected_header);
    assert_eq!(stored.proposer_index_hint(), Some(0));
    assert_eq!(state.last_blob_sidecar_root, parent_root);
}

#[tokio::test]
async fn commit_promotes_metadata_and_updates_parent_root() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);
    let round = Round::new(0);

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let (proposed, metadata, sidecars, _bundle, payload_bytes) =
        propose_blobbed_value(&mut state, height, round, 1).await;

    mock_engine
        .verify_and_store(height, round.as_i64(), &sidecars)
        .await
        .expect("seed undecided blobs");

    state
        .store
        .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
        .await
        .expect("store block bytes");

    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposed.value.id(),
        commit_signatures: Vec::new(),
    };

    let mut notifier = MockExecutionNotifier::new();
    state
        .process_decided_certificate(&certificate, payload_bytes.clone(), &mut notifier)
        .await
        .expect("process decided certificate");

    assert_eq!(state.last_blob_sidecar_root, metadata.to_beacon_header().hash_tree_root());
    assert!(state.store.get_blob_metadata_undecided(height, round).await.expect("get").is_none());
    assert!(state.store.get_blob_metadata(height).await.expect("get").is_some());

    let proposer = state.address.clone();
    let consensus = state
        .store
        .get_consensus_block_metadata(height)
        .await
        .expect("load consensus")
        .expect("consensus metadata");
    assert_eq!(consensus.height(), height);
    assert_eq!(consensus.round(), round);
    assert_eq!(consensus.proposer(), &proposer);
    assert_eq!(consensus.execution_block_hash(), metadata.execution_payload_header().block_hash);
    assert_eq!(consensus.gas_limit(), metadata.execution_payload_header().gas_limit);
    assert_eq!(consensus.gas_used(), metadata.execution_payload_header().gas_used);
    let mut expected_validator_root = [0u8; 32];
    let proposer_bytes = proposer.into_inner();
    expected_validator_root[..proposer_bytes.len()].copy_from_slice(&proposer_bytes);
    assert_eq!(consensus.validator_set_hash(), B256::from(expected_validator_root));

    assert_eq!(
        mock_engine.verify_calls(),
        vec![(height, round.as_i64(), sidecars.len())],
        "verify should record blob count"
    );
    assert_eq!(
        mock_engine.mark_decided_calls(),
        vec![(height, round.as_i64())],
        "mark_decided must be invoked once"
    );
}

#[tokio::test]
async fn commit_promotes_blobless_metadata_updates_parent_root() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(2));
    let height = Height::new(2);
    let round = Round::new(0);

    let previous_root = B256::from([3u8; 32]);
    state.last_blob_sidecar_root = previous_root;

    let header = sample_execution_payload_header();
    let metadata = BlobMetadata::blobless(height, previous_root, &header, Some(0));
    state
        .store
        .put_blob_metadata_undecided(height, round, &metadata)
        .await
        .expect("insert blobless metadata");

    let value_metadata = ValueMetadata::new(header.clone(), Vec::new());
    let value = Value::new(value_metadata.clone());
    let proposer = state.address.clone();
    let proposal = ProposedValue {
        height,
        round,
        valid_round: Round::Nil,
        proposer: proposer.clone(),
        value: value.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
    state
        .store
        .store_undecided_block_data(height, round, NetworkBytes::from_static(b"block"), Vec::new())
        .await
        .expect("store block bytes");

    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposal.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(certificate).await.expect("commit blobless");

    let expected_root = metadata.to_beacon_header().hash_tree_root();
    assert_eq!(state.last_blob_sidecar_root, expected_root);
    assert!(state.store.get_blob_metadata_undecided(height, round).await.expect("get").is_none());

    let decided =
        state.store.get_blob_metadata(height).await.expect("load decided").expect("metadata");
    assert_eq!(decided.blob_count(), 0);
    assert_eq!(decided.parent_blob_root(), previous_root);

    let consensus = state
        .store
        .get_consensus_block_metadata(height)
        .await
        .expect("load consensus")
        .expect("consensus metadata");
    assert_eq!(consensus.height(), height);
    assert_eq!(consensus.round(), round);
    assert_eq!(consensus.proposer(), &proposer);
    assert_eq!(
        consensus.execution_block_hash(),
        value_metadata.execution_payload_header.block_hash
    );
    assert_eq!(consensus.gas_limit(), value_metadata.execution_payload_header.gas_limit);
    assert_eq!(consensus.gas_used(), value_metadata.execution_payload_header.gas_used);
    let mut expected_validator_root = [0u8; 32];
    let proposer_bytes = proposer.into_inner();
    expected_validator_root[..proposer_bytes.len()].copy_from_slice(&proposer_bytes);
    assert_eq!(consensus.validator_set_hash(), B256::from(expected_validator_root));

    assert!(
        mock_engine.mark_decided_calls().is_empty(),
        "blobless commit should not touch blob engine"
    );
    assert!(mock_engine.verify_calls().is_empty());
}

#[tokio::test]
async fn rebuild_blob_sidecars_for_restream_reconstructs_headers() {
    let mock_engine = MockBlobEngine::default();
    let (state, _tmp) = build_state(mock_engine, Height::new(1));

    let height = Height::new(1);
    let round = Round::new(0);
    let payload = sample_execution_payload_v3();
    let header = ExecutionPayloadHeader::from_payload(&payload, None);
    let bundle = sample_blob_bundle(1);

    let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
    let value = Value::new(value_metadata.clone());
    let locally_proposed = LocallyProposedValue::new(height, round, value);

    let (_signed_header, sidecars) =
        state.prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle)).expect("prepare");

    let blob_metadata =
        BlobMetadata::new(height, B256::ZERO, bundle.commitments.clone(), header.clone(), Some(0));

    let rebuilt = state
        .rebuild_blob_sidecars_for_restream(&blob_metadata, state.validator_address(), &sidecars)
        .expect("rebuild");

    assert_eq!(rebuilt.len(), sidecars.len());

    for rebuilt_sidecar in &rebuilt {
        assert_eq!(
            rebuilt_sidecar.signed_block_header.message.parent_root,
            blob_metadata.parent_blob_root()
        );
        assert_eq!(
            rebuilt_sidecar.signed_block_header.message.slot,
            blob_metadata.height().as_u64()
        );
        assert_eq!(
            rebuilt_sidecar.signed_block_header.message.proposer_index,
            blob_metadata.proposer_index_hint().expect("proposer index hint")
        );
        assert!(
            !rebuilt_sidecar.kzg_commitment_inclusion_proof.is_empty(),
            "inclusion proof should be populated"
        );
    }
}

#[tokio::test]
async fn process_decided_certificate_rejects_mismatched_prev_randao() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    let height = Height::new(1);
    let round = Round::new(0);

    let mut payload = sample_execution_payload_v3();
    payload.payload_inner.payload_inner.prev_randao = B256::from([9u8; 32]);
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());

    // Store corresponding proposal/metadata so commit() would succeed if prev_randao matched.
    let header = ExecutionPayloadHeader::from_payload(&payload, None);
    let value_metadata = ValueMetadata::new(header.clone(), Vec::new());
    let value = Value::new(value_metadata.clone());
    let proposer = state.address.clone();
    let proposal = ProposedValue {
        height,
        round,
        valid_round: Round::Nil,
        proposer,
        value: value.clone(),
        validity: Validity::Valid,
    };

    let blobless_metadata = BlobMetadata::blobless(height, B256::ZERO, &header, Some(0));
    state
        .store
        .put_blob_metadata_undecided(height, round, &blobless_metadata)
        .await
        .expect("store metadata");
    state.store.store_undecided_proposal(proposal).await.expect("store proposal");
    state
        .store
        .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
        .await
        .expect("store payload bytes");

    let certificate =
        CommitCertificate { height, round, value_id: value.id(), commit_signatures: Vec::new() };
    let mut notifier = MockExecutionNotifier::new();
    let err = state
        .process_decided_certificate(&certificate, payload_bytes, &mut notifier)
        .await
        .expect_err("prev_randao mismatch must be rejected");
    assert!(err.to_string().contains("prev_randao mismatch"), "unexpected error: {err}");
}

#[tokio::test]
async fn commit_cleans_failed_round_blob_metadata() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

    let height = Height::new(1);
    let decided_round = Round::new(0);
    let losing_round = Round::new(1);

    let metadata_decided = sample_blob_metadata(height, B256::ZERO);
    let metadata_losing = sample_blob_metadata(height, B256::from([9u8; 32]));

    state
        .store
        .put_blob_metadata_undecided(height, decided_round, &metadata_decided)
        .await
        .expect("insert decided metadata");
    state
        .store
        .put_blob_metadata_undecided(height, losing_round, &metadata_losing)
        .await
        .expect("insert losing metadata");

    state
        .store
        .store_undecided_block_data(
            height,
            decided_round,
            NetworkBytes::from_static(b"block"),
            Vec::new(),
        )
        .await
        .expect("store block bytes");

    state
        .blob_rounds
        .entry(height)
        .or_insert_with(HashSet::new)
        .extend([decided_round.as_i64(), losing_round.as_i64()]);

    let value_metadata = sample_value_metadata(metadata_decided.blob_count() as usize);
    let value = Value::new(value_metadata.clone());
    let proposer = state.address.clone();

    let proposal = ProposedValue {
        height,
        round: decided_round,
        valid_round: Round::Nil,
        proposer: proposer.clone(),
        value: value.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");

    let certificate = CommitCertificate {
        height,
        round: decided_round,
        value_id: proposal.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(certificate).await.expect("commit");

    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, losing_round)
            .await
            .expect("load losing metadata")
            .is_none()
    );

    assert_eq!(mock_engine.drop_calls(), vec![(height, losing_round.as_i64())]);
}

#[tokio::test]
async fn multi_round_proposal_isolation_and_commit() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);

    // Propose at round 0
    state.current_height = height;
    state.current_round = Round::new(0);
    let payload_r0 = sample_execution_payload_v3();
    let bundle_r0 = sample_blob_bundle(2);
    state
        .propose_value_with_blobs(
            height,
            Round::new(0),
            NetworkBytes::new(),
            &payload_r0,
            &[],
            Some(&bundle_r0),
        )
        .await
        .expect("propose round 0");

    // Propose at round 1 (timeout on round 0)
    state.current_round = Round::new(1);
    let payload_r1 = sample_execution_payload_v3();
    let bundle_r1 = sample_blob_bundle(3);
    state
        .propose_value_with_blobs(
            height,
            Round::new(1),
            NetworkBytes::new(),
            &payload_r1,
            &[],
            Some(&bundle_r1),
        )
        .await
        .expect("propose round 1");

    // Both rounds should have undecided metadata
    let meta_r0 = state
        .store
        .get_blob_metadata_undecided(height, Round::new(0))
        .await
        .expect("get r0")
        .expect("metadata r0");
    let meta_r1 = state
        .store
        .get_blob_metadata_undecided(height, Round::new(1))
        .await
        .expect("get r1")
        .expect("metadata r1");

    assert_eq!(meta_r0.blob_count(), 2);
    assert_eq!(meta_r1.blob_count(), 3);

    // Commit round 1 (round 0 timed out)
    let value_metadata_r1 = sample_value_metadata(3);
    let value = Value::new(value_metadata_r1.clone());
    let proposal = ProposedValue {
        height,
        round: Round::new(1),
        valid_round: Round::Nil,
        proposer: state.address.clone(),
        value: value.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
    state
        .store
        .store_undecided_block_data(
            height,
            Round::new(1),
            NetworkBytes::from_static(b"block"),
            Vec::new(),
        )
        .await
        .expect("store block bytes");

    let certificate = CommitCertificate {
        height,
        round: Round::new(1),
        value_id: proposal.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(certificate).await.expect("commit r1");

    // Round 1 should be promoted to decided
    let decided = state
        .store
        .get_blob_metadata(height)
        .await
        .expect("get decided")
        .expect("decided metadata");
    assert_eq!(decided.blob_count(), 3);

    // Round 1 undecided should be deleted
    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, Round::new(1))
            .await
            .expect("get r1")
            .is_none()
    );

    // Round 0 should be cleaned up once round 1 commits
    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, Round::new(0))
            .await
            .expect("get r0")
            .is_none(),
        "Round 0 metadata should be pruned after round 1 commit"
    );
    assert_eq!(
        mock_engine.drop_calls(),
        vec![(height, Round::new(0).as_i64())],
        "Blob engine should drop the losing round"
    );

    // Cache should be updated from round 1 metadata
    assert_eq!(state.last_blob_sidecar_root, decided.to_beacon_header().hash_tree_root());
}

#[tokio::test]
async fn propose_at_height_zero_uses_zero_parent_blobbed() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

    // Sanity check initial state
    assert_eq!(state.current_height, Height::new(0));
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

    let payload = sample_execution_payload_v3();
    let bundle = sample_blob_bundle(1);

    state
        .propose_value_with_blobs(
            Height::new(0),
            Round::new(0),
            NetworkBytes::new(),
            &payload,
            &[],
            Some(&bundle),
        )
        .await
        .expect("propose height 0");

    let stored = state
        .store
        .get_blob_metadata_undecided(Height::new(0), Round::new(0))
        .await
        .expect("get")
        .expect("metadata");

    assert_eq!(stored.height(), Height::new(0));
    assert_eq!(stored.parent_blob_root(), B256::ZERO, "Height 0 must use ZERO parent");
    assert_eq!(stored.blob_count(), 1);
    // Cache unchanged (only updated at commit)
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
}

#[tokio::test]
async fn propose_at_height_zero_uses_zero_parent_blobless() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

    let payload = sample_execution_payload_v3();

    state
        .propose_value_with_blobs(
            Height::new(0),
            Round::new(0),
            NetworkBytes::new(),
            &payload,
            &[],
            None, // blobless
        )
        .await
        .expect("propose blobless height 0");

    let stored = state
        .store
        .get_blob_metadata_undecided(Height::new(0), Round::new(0))
        .await
        .expect("get")
        .expect("metadata");

    assert_eq!(stored.height(), Height::new(0));
    assert_eq!(stored.parent_blob_root(), B256::ZERO, "Height 0 must use ZERO parent");
    assert_eq!(stored.blob_count(), 0);
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
}

#[tokio::test]
async fn commit_fails_fast_if_blob_metadata_missing() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);
    let round = Round::new(0);

    // Store proposal and block data but NOT BlobMetadata
    let value_metadata = sample_value_metadata(2);
    let value = Value::new(value_metadata.clone());
    let proposal = ProposedValue {
        height,
        round,
        valid_round: Round::Nil,
        proposer: state.address.clone(),
        value: value.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
    state
        .store
        .store_undecided_block_data(height, round, NetworkBytes::from_static(b"block"), Vec::new())
        .await
        .expect("store block bytes");

    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposal.value.id(),
        commit_signatures: Vec::new(),
    };

    // Commit should fail fast with MissingBlobMetadata error
    let result = state.commit(certificate).await;
    assert!(result.is_err(), "Commit should fail when BlobMetadata is missing");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("BlobMetadata") || err_msg.contains("not found"),
        "Error should mention BlobMetadata: {}",
        err_msg
    );

    // Cache should remain unchanged
    assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

    // Blob engine should NOT have been called
    assert!(mock_engine.mark_decided_calls().is_empty());
}

#[tokio::test]
async fn parent_root_chain_continuity_across_mixed_blocks() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

    // Height 1: Blobbed block
    let payload_h1 = sample_execution_payload_v3();
    let bundle_h1 = sample_blob_bundle(2);
    state
        .propose_value_with_blobs(
            Height::new(1),
            Round::new(0),
            NetworkBytes::new(),
            &payload_h1,
            &[],
            Some(&bundle_h1),
        )
        .await
        .expect("propose h1");

    let meta_h1 = state
        .store
        .get_blob_metadata_undecided(Height::new(1), Round::new(0))
        .await
        .expect("get h1")
        .expect("metadata h1");

    // Commit height 1
    let value_meta_h1 = sample_value_metadata(2);
    let value_h1 = Value::new(value_meta_h1.clone());
    let proposal_h1 = ProposedValue {
        height: Height::new(1),
        round: Round::new(0),
        valid_round: Round::Nil,
        proposer: state.address.clone(),
        value: value_h1.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal_h1.clone()).await.expect("store p1");
    state
        .store
        .store_undecided_block_data(
            Height::new(1),
            Round::new(0),
            NetworkBytes::from_static(b"b1"),
            Vec::new(),
        )
        .await
        .expect("store b1");

    let cert_h1 = CommitCertificate {
        height: Height::new(1),
        round: Round::new(0),
        value_id: proposal_h1.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(cert_h1).await.expect("commit h1");

    let parent_after_h1 = state.last_blob_sidecar_root;
    assert_ne!(parent_after_h1, B256::ZERO, "Cache should be updated after commit");
    assert_eq!(parent_after_h1, meta_h1.to_beacon_header().hash_tree_root());

    // Height 2: Blobless block (should maintain chain)
    state.current_height = Height::new(2);
    let payload_h2 = sample_execution_payload_v3();
    state
        .propose_value_with_blobs(
            Height::new(2),
            Round::new(0),
            NetworkBytes::new(),
            &payload_h2,
            &[],
            None, // blobless
        )
        .await
        .expect("propose h2 blobless");

    let meta_h2 = state
        .store
        .get_blob_metadata_undecided(Height::new(2), Round::new(0))
        .await
        .expect("get h2")
        .expect("metadata h2");

    assert_eq!(meta_h2.blob_count(), 0);
    assert_eq!(meta_h2.parent_blob_root(), parent_after_h1, "Blobless block must chain to h1");

    // Commit height 2
    let value_meta_h2 =
        ValueMetadata::new(ExecutionPayloadHeader::from_payload(&payload_h2, None), Vec::new());
    let value_h2 = Value::new(value_meta_h2.clone());
    let proposal_h2 = ProposedValue {
        height: Height::new(2),
        round: Round::new(0),
        valid_round: Round::Nil,
        proposer: state.address.clone(),
        value: value_h2.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal_h2.clone()).await.expect("store p2");
    state
        .store
        .store_undecided_block_data(
            Height::new(2),
            Round::new(0),
            NetworkBytes::from_static(b"b2"),
            Vec::new(),
        )
        .await
        .expect("store b2");

    let cert_h2 = CommitCertificate {
        height: Height::new(2),
        round: Round::new(0),
        value_id: proposal_h2.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(cert_h2).await.expect("commit h2 blobless");

    let parent_after_h2 = state.last_blob_sidecar_root;
    assert_ne!(parent_after_h2, parent_after_h1, "Cache should update even for blobless");
    assert_eq!(parent_after_h2, meta_h2.to_beacon_header().hash_tree_root());

    // Height 3: Another blobbed block (should chain to h2)
    state.current_height = Height::new(3);
    let payload_h3 = sample_execution_payload_v3();
    let bundle_h3 = sample_blob_bundle(1);
    state
        .propose_value_with_blobs(
            Height::new(3),
            Round::new(0),
            NetworkBytes::new(),
            &payload_h3,
            &[],
            Some(&bundle_h3),
        )
        .await
        .expect("propose h3");

    let meta_h3 = state
        .store
        .get_blob_metadata_undecided(Height::new(3), Round::new(0))
        .await
        .expect("get h3")
        .expect("metadata h3");

    assert_eq!(meta_h3.blob_count(), 1);
    assert_eq!(meta_h3.parent_blob_root(), parent_after_h2, "Height 3 must chain to blobless h2");

    // Verify full chain: h1 → h2 (blobless) → h3
    let decided_h1 = state.store.get_blob_metadata(Height::new(1)).await.expect("d1").expect("m1");
    let decided_h2 = state.store.get_blob_metadata(Height::new(2)).await.expect("d2").expect("m2");

    assert_eq!(decided_h1.parent_blob_root(), B256::ZERO);
    assert_eq!(decided_h2.parent_blob_root(), decided_h1.to_beacon_header().hash_tree_root());
    assert_eq!(meta_h3.parent_blob_root(), decided_h2.to_beacon_header().hash_tree_root());
}

#[tokio::test]
async fn proposer_rotation_updates_metadata_hint() {
    use tempfile::tempdir;
    use ultramarine_types::signing::PrivateKey;

    let private_keys = [PrivateKey::from([1u8; 32]), PrivateKey::from([2u8; 32])];
    let validators: Vec<Validator> =
        private_keys.iter().map(|key| Validator::new(key.public_key(), 1)).collect();
    let validator_set = ValidatorSet::new(validators.clone());
    let genesis = Genesis { validator_set };

    let mut states = Vec::new();
    for (idx, key) in private_keys.iter().enumerate() {
        let tmp = tempdir().expect("tempdir");
        let store = Store::open(tmp.path().join(format!("state_{idx}.db")), DbMetrics::new())
            .expect("store");
        let engine = MockBlobEngine::default();
        let metrics = ultramarine_blob_engine::BlobEngineMetrics::new();
        let state = State::new(
            genesis.clone(),
            LoadContext::new(),
            Ed25519Provider::new(key.clone()),
            validators[idx].address.clone(),
            Height::new(0),
            store,
            engine.clone(),
            metrics,
        );
        states.push((state, engine, tmp));
    }

    for (state, _, _) in states.iter_mut() {
        state.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
        state.hydrate_blob_parent_root().await.expect("hydrate");
    }

    for height_idx in 0..4u64 {
        let proposer_idx = (height_idx as usize) % states.len();
        let (state, engine, _) = &mut states[proposer_idx];

        let height = Height::new(height_idx);
        let round = Round::new(0);
        state.current_height = height;
        state.current_round = round;

        let (proposed, metadata, sidecars, _bundle, payload_bytes) =
            propose_blobbed_value(state, height, round, 1).await;

        engine.verify_and_store(height, round.as_i64(), &sidecars).await.expect("store blobs");
        state
            .store
            .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
            .await
            .expect("store payload");

        for (idx, (other_state, _engine, _tmp)) in states.iter_mut().enumerate() {
            other_state.current_height = height;
            other_state.current_round = round;
            if idx != proposer_idx {
                let package = SyncedValuePackage::Full {
                    value: proposed.value.clone(),
                    execution_payload_ssz: payload_bytes.clone(),
                    blob_sidecars: sidecars.clone(),
                    execution_requests: Vec::new(),
                };
                other_state
                    .process_synced_package(
                        height,
                        round,
                        validators[proposer_idx].address.clone(),
                        package,
                    )
                    .await
                    .expect("sync package processed")
                    .expect("sync package yielded value");
            }
        }

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposed.value.id(),
            commit_signatures: Vec::new(),
        };
        for (idx, (state, _engine, _tmp)) in states.iter_mut().enumerate() {
            let payload = state
                .store
                .get_block_data(height, round)
                .await
                .unwrap_or(None)
                .unwrap_or_else(|| payload_bytes.clone());
            state.latest_block = None;
            let mut notifier = MockExecutionNotifier::new();
            state
                .process_decided_certificate(&certificate, payload.clone(), &mut notifier)
                .await
                .expect("commit rotation state");

            let decided = state
                .store
                .get_blob_metadata(height)
                .await
                .expect("load decided")
                .expect("metadata present");
            assert_eq!(
                decided.proposer_index_hint(),
                Some(proposer_idx as u64),
                "metadata should reflect proposer rotation"
            );
            assert_eq!(
                metadata.blob_kzg_commitments(),
                decided.blob_kzg_commitments(),
                "commitments should persist after promotion"
            );
            assert!(
                state.latest_block.is_some(),
                "state {} should update latest block after commit",
                idx
            );
        }
    }
}
