mod support;

use std::{collections::HashSet, str::FromStr, time::Duration};

use alloy_primitives::B256;
use bytes::Bytes as NetworkBytes;
use malachitebft_app_channel::app::types::{
    LocallyProposedValue, PeerId,
    core::{CommitCertificate, Round, Validity},
};
use ssz::Encode;
use support::*;
use ultramarine_blob_engine::{BlobEngine, BlobEngineError};
use ultramarine_types::{
    aliases::{Bytes as AlloyBytes, Bytes as BlobBytes},
    archive::{ArchiveNotice, ArchiveNoticeBody},
    blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
    blob_metadata::BlobMetadata,
    constants::{LOAD_MAX_FUTURE_DRIFT_SECS, LOAD_MIN_BLOCK_TIME_SECS},
    engine_api::{ExecutionBlock, ExecutionPayloadHeader, load_prev_randao},
    height::Height,
    signing::Ed25519Provider,
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
        .await
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
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let payload = sample_execution_payload_v3();
    let requests_hash = Some(ExecutionPayloadHeader::compute_requests_hash(&[] as &[BlobBytes]));
    let expected_header =
        ExecutionPayloadHeader::from_payload(&payload, requests_hash).expect("build header");
    let bundle = sample_blob_bundle(1);
    let metadata_before =
        state.store.get_blob_metadata_undecided(Height::new(1), Round::new(0)).await.expect("get");
    assert!(metadata_before.is_none());
    let genesis = state
        .store
        .get_blob_metadata(Height::new(0))
        .await
        .expect("get genesis")
        .expect("genesis metadata");
    let expected_parent_root = genesis.to_beacon_header().hash_tree_root();
    assert_eq!(state.last_blob_sidecar_root, expected_parent_root);

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
    assert_eq!(stored.parent_blob_root(), expected_parent_root);
    assert_eq!(stored.blob_kzg_commitments(), bundle.commitments.as_slice());
    assert_eq!(stored.execution_payload_header(), &expected_header);
    assert_eq!(stored.proposer_index_hint(), Some(0));
    assert_eq!(state.last_blob_sidecar_root, expected_parent_root);
}

#[tokio::test]
async fn propose_blobless_value_uses_parent_root_hint() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(2));
    let prev_height = Height::new(1);
    let parent_metadata = sample_blob_metadata(prev_height, B256::from([7u8; 32]));
    state
        .store
        .put_blob_metadata_undecided(prev_height, Round::new(0), &parent_metadata)
        .await
        .expect("store parent metadata");
    state.store.mark_blob_metadata_decided(prev_height, Round::new(0)).await.expect("mark decided");
    let parent_root = parent_metadata.to_beacon_header().hash_tree_root();

    let payload = sample_execution_payload_v3();
    let requests_hash = Some(ExecutionPayloadHeader::compute_requests_hash(&[] as &[BlobBytes]));
    let expected_header =
        ExecutionPayloadHeader::from_payload(&payload, requests_hash).expect("build header");

    let initial_cache_root = state.last_blob_sidecar_root;

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
    assert_eq!(state.last_blob_sidecar_root, initial_cache_root);
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

    let proposer = state.address;
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
    let proposer = state.address;
    let proposal = ProposedValue {
        height,
        round,
        valid_round: Round::Nil,
        proposer,
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
async fn process_decided_certificate_marks_el_degraded_on_syncing() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(10));
    let height = Height::new(10);
    let round = Round::new(0);

    state.set_execution_retry_config(ExecutionRetryConfig {
        new_payload_timeout: Duration::from_millis(2),
        new_payload_sync_timeout: Duration::from_millis(1),
        forkchoice_timeout: Duration::from_millis(2),
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(1),
    });

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");
    seed_decided_blob_metadata(&mut state, Height::new(9), B256::ZERO)
        .await
        .expect("seed parent metadata");

    let (proposed, _metadata, sidecars, _bundle, payload_bytes) =
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

    let syncing_status = PayloadStatus::from_status(PayloadStatusEnum::Syncing);
    let mut notifier = MockExecutionNotifier::with_payload_statuses(vec![
        syncing_status.clone(),
        syncing_status.clone(),
        syncing_status,
    ]);

    let outcome = state
        .process_decided_certificate(&certificate, payload_bytes, &mut notifier)
        .await
        .expect("process decided certificate");

    assert!(outcome.execution_pending, "execution should be pending while EL syncs");
    assert!(state.is_el_degraded(), "state should be marked EL-degraded");
    assert!(
        state.store.get_decided_value(height).await.expect("decided value").is_some(),
        "decided value should be persisted even when execution is pending"
    );
}

#[tokio::test]
async fn rebuild_blob_sidecars_for_restream_reconstructs_headers() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let height = Height::new(1);
    let round = Round::new(0);
    let payload = sample_execution_payload_v3();
    let header = ExecutionPayloadHeader::from_payload(&payload, None).expect("build header");
    let bundle = sample_blob_bundle(1);

    let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
    let value = Value::new(value_metadata.clone());
    let locally_proposed = LocallyProposedValue::new(height, round, value);

    let (_signed_header, sidecars) =
        state.prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle)).await.expect("prepare");

    let blob_hashes = bundle.blob_keccak_hashes();
    let blob_metadata = BlobMetadata::new(
        height,
        B256::ZERO,
        bundle.commitments.clone(),
        blob_hashes,
        header.clone(),
        Some(0),
    );

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
    let header = ExecutionPayloadHeader::from_payload(&payload, None).expect("build header");
    let value_metadata = ValueMetadata::new(header.clone(), Vec::new());
    let value = Value::new(value_metadata.clone());
    let proposer = state.address;
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
async fn propose_value_rejects_invalid_execution_requests() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let height = Height::new(1);
    let round = Round::new(0);
    state.current_height = height;
    state.current_round = round;

    let payload = sample_execution_payload_v3();
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());
    let invalid_requests = vec![
        AlloyBytes::copy_from_slice(&[0x05, 0xAA]),
        AlloyBytes::copy_from_slice(&[0x04, 0xBB]),
    ];

    let err = state
        .propose_value_with_blobs(height, round, payload_bytes, &payload, &invalid_requests, None)
        .await
        .expect_err("invalid execution requests must be rejected");

    assert!(err.to_string().contains("Invalid execution requests"), "unexpected error: {err}");
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
    let proposer = state.address;

    let proposal = ProposedValue {
        height,
        round: decided_round,
        valid_round: Round::Nil,
        proposer,
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
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

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
        proposer: state.address,
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
        proposer: state.address,
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
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

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
        proposer: state.address,
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
    let value_meta_h2 = ValueMetadata::new(
        ExecutionPayloadHeader::from_payload(&payload_h2, None).expect("build header"),
        Vec::new(),
    );
    let value_h2 = Value::new(value_meta_h2.clone());
    let proposal_h2 = ProposedValue {
        height: Height::new(2),
        round: Round::new(0),
        valid_round: Round::Nil,
        proposer: state.address,
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
    let decided_h0 = state.store.get_blob_metadata(Height::new(0)).await.expect("d0").expect("m0");

    assert_eq!(decided_h1.parent_blob_root(), decided_h0.to_beacon_header().hash_tree_root());
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
        let archive_metrics = crate::archive_metrics::ArchiveMetrics::new();
        let state = State::new(
            genesis.clone(),
            LoadContext::new(),
            Ed25519Provider::new(key.clone()),
            validators[idx].address,
            Height::new(0),
            store,
            std::sync::Arc::new(engine.clone()),
            metrics,
            archive_metrics,
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
                    archive_notices: Vec::new(),
                };
                other_state
                    .process_synced_package(
                        height,
                        round,
                        validators[proposer_idx].address,
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
                .expect("load payload")
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

#[tokio::test]
async fn cleanup_stale_round_blobs_removes_old_rounds() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    // Create blobs at rounds 0, 1, 2, 3
    for round_num in 0..=3u32 {
        state.current_height = height;
        state.current_round = Round::new(round_num);
        let payload = sample_execution_payload_v3();
        let bundle = sample_blob_bundle(1);
        state
            .propose_value_with_blobs(
                height,
                Round::new(round_num),
                NetworkBytes::new(),
                &payload,
                &[],
                Some(&bundle),
            )
            .await
            .expect("propose");

        state
            .store
            .store_undecided_block_data(
                height,
                Round::new(round_num),
                NetworkBytes::from_static(b"block"),
                Vec::new(),
            )
            .await
            .expect("store block data");
    }

    // Verify all rounds are tracked in blob_rounds
    assert_eq!(state.blob_rounds.get(&height).unwrap().len(), 4);

    // At round 6 with default retention=3, cutoff = 6-3 = 3
    // Should cleanup rounds < 3, i.e., rounds 0, 1, 2
    let cleaned = state.cleanup_stale_round_blobs(height, Round::new(6)).await.expect("cleanup");

    assert_eq!(cleaned, 3, "Should clean 3 stale rounds");

    // Only round 3 should remain in tracking
    let remaining = state.blob_rounds.get(&height).unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(remaining.contains(&3));

    // Verify drop_round was called for rounds 0, 1, 2
    let drop_calls = mock_engine.drop_calls();
    assert!(drop_calls.contains(&(height, 0)), "Should drop round 0");
    assert!(drop_calls.contains(&(height, 1)), "Should drop round 1");
    assert!(drop_calls.contains(&(height, 2)), "Should drop round 2");
    assert!(!drop_calls.contains(&(height, 3)), "Should NOT drop round 3");

    for round_num in 0..=2u32 {
        let round = Round::new(round_num);
        assert!(
            state
                .store
                .get_undecided_proposal(height, round)
                .await
                .expect("load proposal")
                .is_none(),
            "proposal should be removed for stale round {}",
            round_num
        );
        assert!(
            state.store.get_block_data(height, round).await.expect("load block").is_none(),
            "block data should be removed for stale round {}",
            round_num
        );
        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, round)
                .await
                .expect("load metadata")
                .is_none(),
            "metadata should be removed for stale round {}",
            round_num
        );
    }

    let retained_round = Round::new(3);
    assert!(
        state
            .store
            .get_undecided_proposal(height, retained_round)
            .await
            .expect("load proposal")
            .is_some(),
        "proposal should remain for retained round"
    );
    assert!(
        state.store.get_block_data(height, retained_round).await.expect("load block").is_some(),
        "block data should remain for retained round"
    );
    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, retained_round)
            .await
            .expect("load metadata")
            .is_some(),
        "metadata should remain for retained round"
    );
}

#[tokio::test]
async fn cleanup_stale_round_blobs_no_cleanup_when_retention_window_covers_all() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    // Create blobs at rounds 0, 1
    for round_num in 0..=1u32 {
        state.current_height = height;
        state.current_round = Round::new(round_num);
        let payload = sample_execution_payload_v3();
        let bundle = sample_blob_bundle(1);
        state
            .propose_value_with_blobs(
                height,
                Round::new(round_num),
                NetworkBytes::new(),
                &payload,
                &[],
                Some(&bundle),
            )
            .await
            .expect("propose");

        state
            .store
            .store_undecided_block_data(
                height,
                Round::new(round_num),
                NetworkBytes::from_static(b"block"),
                Vec::new(),
            )
            .await
            .expect("store block data");
    }

    // At round 3 with retention=3, cutoff = 3-3 = 0
    // Nothing should be cleaned since rounds 0, 1 are >= cutoff(0) is false for round 0
    // Actually cutoff < 0 would return early, and cutoff = 0 means rounds < 0 cleaned
    // Let's test with round 2: cutoff = 2-3 = -1, so nothing cleaned
    let cleaned = state.cleanup_stale_round_blobs(height, Round::new(2)).await.expect("cleanup");

    assert_eq!(cleaned, 0, "No rounds should be cleaned");
    assert_eq!(state.blob_rounds.get(&height).unwrap().len(), 2);
    assert!(mock_engine.drop_calls().is_empty());

    for round_num in 0..=1u32 {
        let round = Round::new(round_num);
        assert!(
            state
                .store
                .get_undecided_proposal(height, round)
                .await
                .expect("load proposal")
                .is_some(),
            "proposal should remain for round {}",
            round_num
        );
        assert!(
            state.store.get_block_data(height, round).await.expect("load block").is_some(),
            "block data should remain for round {}",
            round_num
        );
        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, round)
                .await
                .expect("load metadata")
                .is_some(),
            "metadata should remain for round {}",
            round_num
        );
    }
}

#[tokio::test]
async fn cleanup_stale_round_blobs_handles_empty_state() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

    // No blobs created, should not panic
    let cleaned =
        state.cleanup_stale_round_blobs(Height::new(1), Round::new(10)).await.expect("cleanup");

    assert_eq!(cleaned, 0);
    assert!(mock_engine.drop_calls().is_empty());
}

#[tokio::test]
async fn get_blobs_with_status_check_returns_pruned_error() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));
    let height = Height::new(1);
    let round = Round::new(0);

    let commitments = vec![KzgCommitment::new([0x11; 48]), KzgCommitment::new([0x22; 48])];
    let blob_hashes = vec![B256::from([0xAA; 32]), B256::from([0xBB; 32])];
    let header = sample_execution_payload_header();
    let metadata = BlobMetadata::new(
        height,
        B256::ZERO,
        commitments.clone(),
        blob_hashes.clone(),
        header,
        Some(0),
    );
    state
        .store
        .put_blob_metadata_undecided(height, round, &metadata)
        .await
        .expect("store undecided metadata");
    state.store.mark_blob_metadata_decided(height, round).await.expect("promote metadata");

    let signer = Ed25519Provider::new(state.signing_provider.private_key().clone());
    for (idx, commitment) in commitments.iter().enumerate() {
        let body = ArchiveNoticeBody {
            height,
            round,
            blob_index: idx as u16,
            kzg_commitment: *commitment,
            blob_keccak: blob_hashes[idx],
            provider_id: "test-provider".to_string(),
            locator: format!("test://{}", idx),
            archived_by: state.address,
            archived_at: 42,
        };
        let notice = ArchiveNotice::sign(body, &signer);
        state.handle_archive_notice(notice).await.expect("handle archive notice");
    }

    let mut decided_metadata = state
        .store
        .get_blob_metadata(height)
        .await
        .expect("load metadata")
        .expect("metadata present");
    state
        .prune_archived_height(height, &mut decided_metadata)
        .await
        .expect("prune archived height");

    match state.get_blobs_with_status_check(height).await {
        Err(BlobEngineError::BlobsPruned { blob_count, locators, .. }) => {
            assert_eq!(blob_count, 2);
            assert_eq!(locators, vec!["test://0".to_string(), "test://1".to_string()]);
        }
        other => panic!("expected BlobsPruned error, got {:?}", other),
    }
}

/// Regression test: Ensure `process_synced_package` uses the store-derived parent_root,
/// NOT the in-memory cache. This test sets the cache to a WRONG value and verifies
/// that the written BlobMetadata.parent_blob_root matches the store, not the corrupted cache.
///
/// Background: A bug existed where `process_synced_package` used `self.blob_parent_root()`
/// (the cache) instead of loading from the store. This caused nodes to diverge when
/// the cache was stale, leading to chain halts during high-traffic scenarios.
///
/// This test uses a BLOBLESS sync to isolate the parent_root logic without blob verification
/// complexity.
#[tokio::test]
async fn sync_path_uses_store_not_cache_for_parent_root() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(0));

    // Seed genesis metadata
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis");
    state.hydrate_blob_parent_root().await.expect("hydrate");

    // Commit height 1 (blobless) to establish a known parent
    let height_1 = Height::new(1);
    let round = Round::new(0);

    // Use helper to create decided blobless metadata at h1
    let genesis_root = state.blob_parent_root();
    let correct_parent_root =
        seed_decided_blob_metadata(&mut state, height_1, genesis_root).await.expect("seed h1");

    // Update state to point to h1
    state.last_blob_sidecar_root = correct_parent_root;
    state.last_blob_sidecar_height = height_1;

    // CORRUPT the in-memory cache to a WRONG value
    let wrong_root = B256::from([0xDE; 32]);
    state.last_blob_sidecar_root = wrong_root;

    // Verify cache is corrupted
    assert_eq!(state.blob_parent_root(), wrong_root);
    assert_ne!(wrong_root, correct_parent_root, "test setup: roots must differ");

    // Now sync height 2 (blobless) via process_synced_package
    let height_2 = Height::new(2);
    state.current_height = height_2;
    state.current_round = round;

    // Build a blobless value for h2
    let payload_h2 = sample_execution_payload_v3();
    let payload_bytes_h2 = NetworkBytes::from(payload_h2.as_ssz_bytes());
    let metadata_h2 = ValueMetadata::new(sample_execution_payload_header(), vec![]); // No blobs
    let value_h2 = ultramarine_types::value::Value::new(metadata_h2.clone());

    // Store the payload for sync
    state
        .store
        .store_undecided_block_data(height_2, round, payload_bytes_h2.clone(), Vec::new())
        .await
        .expect("store h2 payload");

    // Process the synced value - this should use STORE, not CACHE
    let package = SyncedValuePackage::Full {
        value: value_h2,
        execution_payload_ssz: payload_bytes_h2.clone(),
        blob_sidecars: vec![], // No blobs
        execution_requests: Vec::new(),
        archive_notices: Vec::new(),
    };

    let result = state
        .process_synced_package(height_2, round, state.address, package)
        .await
        .expect("process sync")
        .expect("sync succeeded");

    assert_eq!(result.height, height_2);

    // THE KEY ASSERTION: The written BlobMetadata must use the STORE-derived parent_root,
    // NOT the corrupted cache value
    let synced_metadata =
        state.store.get_blob_metadata(height_2).await.expect("load h2").expect("h2 metadata");

    assert_eq!(
        synced_metadata.parent_blob_root(),
        correct_parent_root,
        "BUG: process_synced_package used cache ({:?}) instead of store ({:?})",
        wrong_root,
        correct_parent_root
    );
    assert_ne!(
        synced_metadata.parent_blob_root(),
        wrong_root,
        "INVARIANT VIOLATED: parent_root must NOT match corrupted cache"
    );
}

/// Regression test for BUG-002: get_earliest_height() was returning the pruned minimum
/// instead of genesis height. The fix makes get_earliest_height() return Height(0) if
/// genesis metadata exists in BLOB_METADATA_DECIDED_TABLE.
///
/// This ensures that peers can sync from genesis even after pruning operations have
/// removed older blob data, following the Lighthouse pattern where beacon blocks
/// are kept forever.
#[tokio::test]
async fn test_history_min_height_returns_genesis_after_pruning() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

    // Step 1: Seed genesis metadata
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis blob metadata");

    // Step 2: Add blocks up to height 5
    let mut parent_root = BlobMetadata::genesis().to_beacon_header().hash_tree_root();
    for h in 1..=5u64 {
        parent_root = seed_decided_blob_metadata(&mut state, Height::new(h), parent_root)
            .await
            .expect(&format!("seed decided blob metadata at height {}", h));
    }

    // Step 3: Verify get_earliest_height() returns Height(0) even with blocks up to height 5
    let earliest = state.get_earliest_height().await;
    assert_eq!(
        earliest,
        Height::new(0),
        "BUG-002: get_earliest_height() should return genesis (Height 0) when genesis metadata exists"
    );
}

/// Test that reorg/fork handling correctly drops orphaned blobs.
/// When a different round is committed, all other rounds' blobs at that height should be cleaned.
#[tokio::test]
async fn test_reorg_drops_orphaned_blobs() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
    let height = Height::new(1);

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    // Create blobs at multiple rounds (simulating a fork scenario)
    let rounds = [Round::new(0), Round::new(1), Round::new(2)];
    for &round in &rounds {
        state.current_height = height;
        state.current_round = round;
        let payload = sample_execution_payload_v3();
        let bundle = sample_blob_bundle(1);
        state
            .propose_value_with_blobs(
                height,
                round,
                NetworkBytes::new(),
                &payload,
                &[],
                Some(&bundle),
            )
            .await
            .expect("propose");

        state
            .store
            .store_undecided_block_data(
                height,
                round,
                NetworkBytes::from_static(b"block"),
                Vec::new(),
            )
            .await
            .expect("store block data");

        // Register round in blob_rounds for tracking
        state.blob_rounds.entry(height).or_insert_with(HashSet::new).insert(round.as_i64());
    }

    // Verify all rounds have metadata
    for &round in &rounds {
        assert!(
            state.store.get_blob_metadata_undecided(height, round).await.expect("get").is_some(),
            "Round {:?} should have metadata",
            round
        );
    }

    // Commit round 1 (middle round wins the fork)
    let winning_round = Round::new(1);
    let value_metadata = sample_value_metadata(1);
    let value = Value::new(value_metadata);
    let proposal = ProposedValue {
        height,
        round: winning_round,
        valid_round: Round::Nil,
        proposer: state.address,
        value: value.clone(),
        validity: Validity::Valid,
    };

    state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");

    let certificate = CommitCertificate {
        height,
        round: winning_round,
        value_id: proposal.value.id(),
        commit_signatures: Vec::new(),
    };

    state.commit(certificate).await.expect("commit");

    // Winning round should be promoted to decided
    assert!(
        state.store.get_blob_metadata(height).await.expect("get").is_some(),
        "Winning round metadata should be promoted to decided"
    );

    // Orphaned rounds (0 and 2) should be cleaned up
    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, Round::new(0))
            .await
            .expect("get")
            .is_none(),
        "Round 0 (orphaned) should be cleaned up"
    );
    assert!(
        state
            .store
            .get_blob_metadata_undecided(height, Round::new(2))
            .await
            .expect("get")
            .is_none(),
        "Round 2 (orphaned) should be cleaned up"
    );

    // Verify blob engine received drop calls for orphaned rounds
    let drop_calls = mock_engine.drop_calls();
    assert!(drop_calls.contains(&(height, 0)), "Blob engine should drop round 0");
    assert!(drop_calls.contains(&(height, 2)), "Blob engine should drop round 2");
    assert!(!drop_calls.contains(&(height, 1)), "Blob engine should NOT drop winning round 1");

    // FIX-007: Verify orphaned_blobs_dropped metric is recorded correctly
    // Each orphaned round (0 and 2) had 1 blob each, so total should be 2
    let metrics = state.blob_metrics.snapshot();
    assert_eq!(
        metrics.orphaned_blobs_dropped, 2,
        "Should record 2 orphaned blobs dropped (1 from round 0 + 1 from round 2)"
    );
}

/// Test that sync rejects packages with fewer sidecars than claimed in metadata.
#[tokio::test]
async fn test_sync_rejects_partial_sidecars() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let height = Height::new(1);
    let round = Round::new(0);
    state.current_height = height;
    state.current_round = round;

    // Create a valid package with 3 blobs, then truncate sidecars to 1
    let payload = sample_execution_payload_v3();
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());
    let bundle = sample_blob_bundle(3); // Create 3 blobs

    let header = ExecutionPayloadHeader::from_payload(&payload, None).expect("build header");
    let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
    let value = Value::new(value_metadata);

    // Store payload for sync
    state
        .store
        .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
        .await
        .expect("store payload");

    // Prepare valid sidecars first, then truncate to create mismatch
    let locally_proposed = LocallyProposedValue::new(height, round, value.clone());
    let (_signed_header, mut full_sidecars) = state
        .prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))
        .await
        .expect("prepare sidecars");

    assert_eq!(full_sidecars.len(), 3, "Should have 3 sidecars");

    // Truncate to only 1 sidecar - simulating partial sidecar receipt
    full_sidecars.truncate(1);
    let partial_sidecars = full_sidecars;

    // The package has mismatched sidecar count vs metadata
    let package = SyncedValuePackage::Full {
        value,
        execution_payload_ssz: payload_bytes,
        blob_sidecars: partial_sidecars, // Only 1 sidecar but metadata claims 3
        execution_requests: Vec::new(),
        archive_notices: Vec::new(),
    };

    // Process should return None (rejection) due to count mismatch
    let result = state
        .process_synced_package(height, round, state.address, package)
        .await
        .expect("process should not error");

    assert!(
        result.is_none(),
        "Sync should reject package with partial sidecars (fewer than metadata claims)"
    );

    // FIX-007: Verify sync_packages_rejected metric is recorded
    let metrics = state.blob_metrics.snapshot();
    assert_eq!(
        metrics.sync_packages_rejected, 1,
        "Should record 1 rejected sync package due to partial sidecars"
    );
}

/// Test that sync rejects packages with duplicate blob indices.
/// This tests the FIX-003 duplicate index check.
#[tokio::test]
async fn test_sync_rejects_duplicate_indices() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    let height = Height::new(1);
    let round = Round::new(0);
    state.current_height = height;
    state.current_round = round;

    // Create a valid package first, then tamper with indices
    let payload = sample_execution_payload_v3();
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());
    let bundle = sample_blob_bundle(2);

    let header = ExecutionPayloadHeader::from_payload(&payload, None).expect("build header");
    let value_metadata = ValueMetadata::new(header, bundle.commitments.clone());
    let value = Value::new(value_metadata);

    // Store payload for sync
    state
        .store
        .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
        .await
        .expect("store payload");

    // Prepare valid sidecars first
    let locally_proposed = LocallyProposedValue::new(height, round, value.clone());
    let (_signed_header, mut sidecars) = state
        .prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))
        .await
        .expect("prepare sidecars");

    assert_eq!(sidecars.len(), 2, "Should have 2 sidecars");

    // Tamper: set both sidecars to have the same index (index 0)
    sidecars[1].index = 0; // Duplicate index!

    let package = SyncedValuePackage::Full {
        value,
        execution_payload_ssz: payload_bytes,
        blob_sidecars: sidecars,
        execution_requests: Vec::new(),
        archive_notices: Vec::new(),
    };

    // Process should return None (rejection) due to duplicate indices
    let result = state
        .process_synced_package(height, round, state.address, package)
        .await
        .expect("process should not error");

    assert!(result.is_none(), "Sync should reject package with duplicate blob indices");

    // FIX-007: Verify sync_packages_rejected metric is recorded
    let metrics = state.blob_metrics.snapshot();
    assert_eq!(
        metrics.sync_packages_rejected, 1,
        "Should record 1 rejected sync package due to duplicate indices"
    );
}

/// Test sequential multi-height sync chain continuity - verifies that sync operations
/// maintain parent root chain integrity across multiple heights from different proposers.
/// Note: This tests sequential processing, not true concurrency (which would require
/// Arc<Mutex<State>>).
///
/// FIXME: This test has structural issues - process_synced_package validation
/// rejects packages that don't match EL verification. Needs refactoring to
/// properly mock the execution layer or use a different approach.
#[ignore = "Needs refactoring to properly handle EL verification in sync flow"]
#[tokio::test]
async fn test_sequential_multi_height_sync_chain_continuity() {
    use ultramarine_types::signing::PrivateKey;

    // Setup multiple validator keys to simulate different peers
    let private_keys =
        [PrivateKey::from([1u8; 32]), PrivateKey::from([2u8; 32]), PrivateKey::from([3u8; 32])];
    let validators: Vec<Validator> =
        private_keys.iter().map(|key| Validator::new(key.public_key(), 1)).collect();

    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(0));

    state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
    state.hydrate_blob_parent_root().await.expect("hydrate parent root");

    // Process each height sequentially - must do one at a time because preparing
    // sidecars for height N+1 requires decided metadata for height N to exist.
    for h in 1..=3u64 {
        let height = Height::new(h);
        let round = Round::new(0);
        let peer_idx = (h as usize - 1) % validators.len();
        let proposer = validators[peer_idx].address;

        // Set state to current height
        state.current_height = height;
        state.current_round = round;

        // Prepare sync package
        let payload = sample_execution_payload_v3();
        let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());
        let bundle = sample_blob_bundle(1);

        let header = ExecutionPayloadHeader::from_payload(&payload, None).expect("build header");
        let value_metadata = ValueMetadata::new(header, bundle.commitments.clone());
        let value = Value::new(value_metadata);

        // Store payload for this height
        state
            .store
            .store_undecided_block_data(height, round, payload_bytes.clone(), Vec::new())
            .await
            .expect("store payload");

        // Prepare sidecars - this needs parent height's decided metadata to exist
        let locally_proposed = LocallyProposedValue::new(height, round, value.clone());
        let (_signed_header, sidecars) = state
            .prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))
            .await
            .expect("prepare sidecars");

        // Create and process sync package
        let package = SyncedValuePackage::Full {
            value,
            execution_payload_ssz: payload_bytes,
            blob_sidecars: sidecars,
            execution_requests: Vec::new(),
            archive_notices: Vec::new(),
        };

        let result = state
            .process_synced_package(height, round, proposer, package)
            .await
            .expect("process should succeed")
            .expect("sync should return value");

        assert_eq!(result.height, height, "Synced height should match");
        assert_eq!(result.round, round, "Synced round should match");
        assert_eq!(result.proposer, proposer, "Proposer should match");
    }

    // Verify all heights were synced correctly
    for h in 1..=3u64 {
        let height = Height::new(h);
        assert!(
            state.store.get_blob_metadata(height).await.expect("load").is_some(),
            "Height {} should have decided metadata",
            h
        );
    }

    // Verify parent root chain is continuous
    let genesis_root = BlobMetadata::genesis().to_beacon_header().hash_tree_root();
    let h1_meta = state.store.get_blob_metadata(Height::new(1)).await.expect("load").expect("h1");
    let h2_meta = state.store.get_blob_metadata(Height::new(2)).await.expect("load").expect("h2");
    let h3_meta = state.store.get_blob_metadata(Height::new(3)).await.expect("load").expect("h3");

    assert_eq!(h1_meta.parent_blob_root(), genesis_root, "Height 1 should chain from genesis");
    assert_eq!(
        h2_meta.parent_blob_root(),
        h1_meta.to_beacon_header().hash_tree_root(),
        "Height 2 should chain from height 1"
    );
    assert_eq!(
        h3_meta.parent_blob_root(),
        h2_meta.to_beacon_header().hash_tree_root(),
        "Height 3 should chain from height 2"
    );
}

// ============================================================================
// Timestamp validation tests (BUG-011 fix)
// ============================================================================

fn test_peer_id() -> PeerId {
    PeerId::from_str("12D3KooWHRyfTBKcjkqjNk5UZarJhzT7rXZYfr4DmaCWJgen62Xk").expect("valid peer id")
}

fn set_latest_block(state: &mut State<MockBlobEngine>, block_hash: B256, timestamp: u64) {
    state.latest_block = Some(ExecutionBlock {
        block_hash,
        block_number: 0,
        parent_hash: B256::ZERO,
        timestamp,
        prev_randao: load_prev_randao(),
    });
}

async fn send_payload_as_proposal(
    state: &mut State<MockBlobEngine>,
    payload: alloy_rpc_types_engine::ExecutionPayloadV3,
) -> bool {
    let height = state.current_height;
    let round = state.current_round;
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());
    let proposed = state
        .propose_value_with_blobs(height, round, payload_bytes.clone(), &payload, &[], None)
        .await
        .expect("propose value");

    let msgs: Vec<_> = state.stream_proposal(proposed, payload_bytes, None, &[], None).collect();
    let peer_id = test_peer_id();

    for msg in msgs {
        if state.received_proposal_part(peer_id, msg).await.expect("received proposal").is_some() {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn timestamp_validation_rejects_future_drift() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis");
    state.hydrate_blob_parent_root().await.expect("hydrate");

    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs();
    let parent_hash = B256::from([2u8; 32]);
    let parent_ts = now.saturating_sub(1);
    set_latest_block(&mut state, parent_hash, parent_ts);

    let mut payload = sample_execution_payload_v3();
    payload.payload_inner.payload_inner.parent_hash = parent_hash;
    payload.payload_inner.payload_inner.block_number = 1;
    payload.payload_inner.payload_inner.timestamp = now + LOAD_MAX_FUTURE_DRIFT_SECS + 5;

    let accepted = send_payload_as_proposal(&mut state, payload).await;
    assert!(!accepted, "proposal should be rejected for future drift");
}

#[tokio::test]
async fn timestamp_validation_rejects_parent_hash_mismatch() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis");
    state.hydrate_blob_parent_root().await.expect("hydrate");

    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs();
    let parent_hash = B256::from([2u8; 32]);
    let parent_ts = now.saturating_sub(1);
    set_latest_block(&mut state, parent_hash, parent_ts);

    let mut payload = sample_execution_payload_v3();
    payload.payload_inner.payload_inner.parent_hash = B256::from([3u8; 32]); // mismatch
    payload.payload_inner.payload_inner.block_number = 1;
    payload.payload_inner.payload_inner.timestamp = parent_ts + LOAD_MIN_BLOCK_TIME_SECS;

    let accepted = send_payload_as_proposal(&mut state, payload).await;
    assert!(!accepted, "proposal should be rejected for parent hash mismatch");
}

#[tokio::test]
async fn timestamp_validation_rejects_not_strictly_increasing() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis");
    state.hydrate_blob_parent_root().await.expect("hydrate");

    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs();
    let parent_hash = B256::from([2u8; 32]);
    let parent_ts = now;
    set_latest_block(&mut state, parent_hash, parent_ts);

    let mut payload = sample_execution_payload_v3();
    payload.payload_inner.payload_inner.parent_hash = parent_hash;
    payload.payload_inner.payload_inner.block_number = 1;
    payload.payload_inner.payload_inner.timestamp = parent_ts;

    let accepted = send_payload_as_proposal(&mut state, payload).await;
    assert!(!accepted, "proposal should be rejected for non-increasing timestamp");
}

#[tokio::test]
async fn timestamp_validation_accepts_valid_timestamp() {
    let mock_engine = MockBlobEngine::default();
    let (mut state, _tmp) = build_state(mock_engine, Height::new(1));
    state.store.seed_genesis_blob_metadata().await.expect("seed genesis");
    state.hydrate_blob_parent_root().await.expect("hydrate");

    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs();
    let parent_hash = B256::from([2u8; 32]);
    let parent_ts = now.saturating_sub(1);
    set_latest_block(&mut state, parent_hash, parent_ts);

    let mut payload = sample_execution_payload_v3();
    payload.payload_inner.payload_inner.parent_hash = parent_hash;
    payload.payload_inner.payload_inner.block_number = 1;
    payload.payload_inner.payload_inner.timestamp = parent_ts + LOAD_MIN_BLOCK_TIME_SECS;

    let accepted = send_payload_as_proposal(&mut state, payload).await;
    assert!(accepted, "proposal should be accepted with valid timestamp");
}
