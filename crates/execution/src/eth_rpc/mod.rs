pub mod alloy_impl;

use alloy_rpc_types::{Block, BlockNumberOrTag, Filter, Log, SyncStatus};
use alloy_rpc_types_txpool::{TxpoolInspect, TxpoolStatus};
use async_trait::async_trait;
use color_eyre::eyre;

/// A trait representing the standard Ethereum JSON-RPC API.
///
/// This client is responsible for all non-Engine API calls to the execution node.
#[async_trait]
pub trait EthRpc: Send + Sync {
    /// Corresponds to the `eth_chainId` RPC method
    async fn get_chain_id(&self) -> eyre::Result<String>;
    /// Corresponds to the `eth_syncing` RPC method.
    /// Returns the sync status of the execution node.
    async fn syncing(&self) -> eyre::Result<SyncStatus>;

    /// Corresponds to the `eth_getLogs` RPC method.
    /// Used to find deposit logs for the validator deposit contract.
    async fn get_logs(&self, filter: &Filter) -> eyre::Result<Vec<Log>>;

    /// Corresponds to the `eth_getBlockByNumber` RPC method.
    /// A general-purpose utility to fetch block information.
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        full_transactions: bool,
    ) -> eyre::Result<Option<Block>>;

    /// Corresponds to the `txpool_status` RPC method.
    async fn txpool_status(&self) -> eyre::Result<TxpoolStatus>;

    /// Corresponds to the `txpool_inspect` RPC method.
    async fn txpool_inspect(&self) -> eyre::Result<TxpoolInspect>;
}
