#![allow(missing_docs)]

use thiserror::Error;
use ultramarine_types::aliases::BlockHash;

/// Defines the specific error types for the execution client.
///
/// This enum is used to classify errors, but functions throughout the crate will
/// return `eyre::Result`. This allows for flexible error reporting while
/// maintaining the ability to match on specific error kinds.
#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("Transport error: {0}")]
    Transport(String),

    #[error("JSON-RPC error (code {code}): {message}")]
    JsonRpc { code: i64, message: String },

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Method not supported: {0}")]
    MethodNotSupported(String),

    #[error("JWT error: {0}")]
    Jwt(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Execution layer is syncing (forkchoiceUpdated): head={head:?}")]
    SyncingForkchoice { head: BlockHash },

    #[error("Execution layer did not start payload build (no payloadId) for head={head:?}")]
    NoPayloadIdForBuild { head: BlockHash },

    #[error(
        "Built payload does not match requested head={head:?}: parent={parent:?} number={block_number} timestamp={timestamp}"
    )]
    BuiltPayloadMismatch { head: BlockHash, parent: BlockHash, block_number: u64, timestamp: u64 },
}
