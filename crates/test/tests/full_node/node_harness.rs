//! Tier 1 full-node harness placeholder.
//!
//! This test will eventually spin real Ultramarine nodes (channel actors + WAL + libp2p)
//! and drive blobbed proposals across `/proposal_parts`. For now it acts as a
//! scaffold so we can iterate incrementally.

#[tokio::test]
#[ignore = "full-node harness not implemented yet"]
async fn full_node_blob_roundtrip_placeholder() -> color_eyre::Result<()> {
    // TODO(P1): start single Ultramarine node via channel actors/WAL + HTTP Engine stub
    // and exercise a blobbed proposal end-to-end.
    Ok(())
}
