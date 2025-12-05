//! Deterministic execution-request helpers shared by consensus + harness tests.

use alloy_rpc_types_engine::ExecutionPayloadV3;
use ultramarine_types::{aliases::Bytes, height::Height};

/// Convenience alias for execution-request vectors.
pub type ExecutionRequests = Vec<Bytes>;

/// Trait implemented by generators that derive execution requests from payloads.
pub trait ExecutionRequestGenerator: Send + Sync {
    /// Produce execution requests for the provided payload.
    fn generate(&self, payload: &ExecutionPayloadV3) -> ExecutionRequests;
}

impl<F> ExecutionRequestGenerator for F
where
    F: Fn(&ExecutionPayloadV3) -> ExecutionRequests + Send + Sync,
{
    fn generate(&self, payload: &ExecutionPayloadV3) -> ExecutionRequests {
        (self)(payload)
    }
}

/// Deterministic request list tied to a payload's block height.
pub fn default_execution_request_generator(payload: &ExecutionPayloadV3) -> ExecutionRequests {
    let height = Height::new(payload.payload_inner.payload_inner.block_number);
    sample_execution_requests_for_height(height)
}

/// Produce deterministic requests for the given height.
///
/// The harness uses this helper to make consensus + execution stubs agree on the
/// requests_hash that appears in Prague payload headers.
pub fn sample_execution_requests_for_height(height: Height) -> ExecutionRequests {
    let h = height.as_u64();
    vec![
        Bytes::copy_from_slice(&[0x01, (h & 0xFF) as u8, ((h >> 8) & 0xFF) as u8]),
        Bytes::copy_from_slice(&[0x05, 0xC0, (h.wrapping_add(1) & 0xFF) as u8]),
    ]
}
