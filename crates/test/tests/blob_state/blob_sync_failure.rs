//! Sync failure path integration test.
//!
//! Verifies that invalid blob sidecars are rejected during the sync flow and
//! that blob sync failure metrics are incremented accordingly.

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn blob_sync_failure_rejects_invalid_proof() -> color_eyre::Result<()> {
    use common::{
        TestDirs, build_seeded_state, make_genesis, propose_with_optional_blobs,
        sample_blob_bundle, sample_execution_payload_v3_for_height,
    };
    use malachitebft_app_channel::app::types::core::Round;
    use ssz::Encode;
    use ultramarine_blob_engine::BlobEngine;
    use ultramarine_types::{height::Height, sync::SyncedValuePackage};

    let (genesis, validators) = make_genesis(1);
    let validator = &validators[0];
    let dirs = TestDirs::new();

    let mut node = build_seeded_state(&dirs, &genesis, validator, Height::new(0)).await?;

    let height = Height::new(0);
    let bundle = sample_blob_bundle(1);
    let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
    let round = Round::new(0);
    let round_i64 = round.as_i64();

    // Build proposal and sidecars with valid data first.
    let (proposed, payload_bytes, maybe_sidecars) =
        propose_with_optional_blobs(&mut node.state, height, round, &payload, Some(&bundle))
            .await?;
    let mut sidecars = maybe_sidecars.expect("sidecars expected");

    // Tamper with the sidecar proof to guarantee verification failure.
    sidecars[0].kzg_proof.0[0] ^= 0xFF;

    // Attempt to ingest via shared sync handler (should reject).
    let package = SyncedValuePackage::Full {
        value: proposed.value.clone(),
        execution_payload_ssz: payload_bytes.clone(),
        blob_sidecars: sidecars.clone(),
    };
    let encoded = package.encode().map_err(|e| color_eyre::eyre::eyre!(e))?;
    let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| color_eyre::eyre::eyre!(e))?;

    let result =
        node.state.process_synced_package(height, round, validator.address(), decoded).await?;
    assert!(result.is_none(), "Tampered blob proof should cause rejection");

    let metrics = node.blob_metrics.snapshot();
    assert_eq!(metrics.sync_failures, 1, "sync failure metric should increment");
    assert_eq!(metrics.verifications_success, 0);
    assert_eq!(metrics.verifications_failure, sidecars.len() as u64);
    assert_eq!(metrics.storage_bytes_undecided, 0);
    assert_eq!(metrics.storage_bytes_decided, 0);
    assert_eq!(metrics.lifecycle_promoted, 0);
    assert_eq!(metrics.lifecycle_dropped, 0);

    // Ensure no blobs were stored in undecided state.
    let undecided = node.state.blob_engine().get_undecided_blobs(height, round_i64).await?;
    assert!(undecided.is_empty(), "invalid blobs should not be stored");

    Ok(())
}
