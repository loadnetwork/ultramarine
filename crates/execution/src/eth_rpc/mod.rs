// crates/execution/src/eth_rpc/mod.rs

pub mod alloy_impl;

use async_trait::async_trait;
use color_eyre::eyre;

/// A trait representing the standard Ethereum JSON-RPC API.
#[async_trait]
pub trait EthRpc: Send + Sync {
    // e.g., async fn get_block_by_number(&self) -> eyre::Result<()>;
}
