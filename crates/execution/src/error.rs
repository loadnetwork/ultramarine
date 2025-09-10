#![allow(missing_docs)]

use thiserror::Error;

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
}
