//! Sync package round-trip integration test harness.
//!
//! Ensures that ingesting a `SyncedValuePackage::Full` promotes blob data and
//! metadata immediately, matching the expectations laid out in Phase 5B.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn sync_package_roundtrip() -> color_eyre::Result<()> {
    use bytes::Bytes;
    use color_eyre::eyre::eyre;
    use common::{
        TestDirs, build_seeded_state, make_genesis,
        mocks::{MockEngineApi, MockExecutionNotifier},
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::{
        LocallyProposedValue,
        core::{CommitCertificate, Round},
    };
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_execution::EngineApi;
    use ultramarine_types::{
        blob::BYTES_PER_BLOB, engine_api::ExecutionPayloadHeader, height::Height,
        sync::SyncedValuePackage, value::Value, value_metadata::ValueMetadata,
    };

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

    // Build sample payload + blobs mirroring a decided block.
    let height = Height::new(0);
    let raw_bundle = sample_blob_bundle(1);
    let raw_payload = sample_execution_payload_v3_for_height(height, Some(&raw_bundle));
    let payload_id = common::payload_id(2);
    let mock_engine = MockEngineApi::default().with_payload(
        payload_id,
        raw_payload.clone(),
        Some(raw_bundle.clone()),
    );
    let (payload, maybe_bundle) = mock_engine.get_payload_with_blobs(payload_id).await?;
    let bundle = maybe_bundle.expect("bundle");
    let blob_count = bundle.commitments.len();
    let payload_bytes = Bytes::from(payload.as_ssz_bytes());
    let round = Round::new(0);

    let header = ExecutionPayloadHeader::from_payload(&payload);
    let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
    let value = Value::new(value_metadata.clone());

    // Generate blob sidecars matching the package contents.
    let locally_proposed = LocallyProposedValue::new(height, round, value.clone());
    let (_signed_header, sidecars) =
        node.state.prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))?;

    let package = SyncedValuePackage::Full {
        value: value.clone(),
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: sidecars.clone(),
    };

    let encoded = package.encode().map_err(|e| eyre!(e))?;
    let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| eyre!(e))?;

    let proposer = validator.address();
    let proposed_value = node
        .state
        .process_synced_package(height, round, proposer.clone(), decoded)
        .await?
        .expect("sync package should produce proposed value");

    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposed_value.value.id(),
        commit_signatures: Vec::new(),
    };
    let payload_bytes_for_commit =
        node.state.get_block_data(height, round).await.unwrap_or_else(|| payload_bytes.clone());
    let mut notifier = MockExecutionNotifier::default();
    node.state
        .process_decided_certificate(&certificate, payload_bytes_for_commit, &mut notifier)
        .await?;

    // Verify blob availability and consensus metadata after sync commit.
    let imported = node.state.blob_engine().get_for_import(height).await?;
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].kzg_commitment, bundle.commitments[0]);

    let decided_metadata = node.state.get_blob_metadata(height).await?;
    assert!(decided_metadata.is_some());

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.verifications_success, blob_count as u64);
    assert_eq!(metrics.verifications_failure, 0);
    assert_eq!(metrics.storage_bytes_undecided, 0);
    assert_eq!(metrics.storage_bytes_decided, (blob_count * BYTES_PER_BLOB) as i64);
    assert_eq!(metrics.lifecycle_promoted, blob_count as u64);
    assert_eq!(metrics.sync_failures, 0);

    Ok(())
}
