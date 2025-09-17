//! An implementation of the `EthRpc` trait using the `alloy` library.
//!
//! This module provides a client for the standard Ethereum JSON-RPC API. It is
//! responsible for all non-Engine API calls, such as fetching logs, checking
//! sync status, or getting blocks.
#![allow(missing_docs)]
use std::fmt;

use alloy_network::Ethereum;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types::{BlockNumberOrTag, Filter, Log, SyncStatus};
use alloy_rpc_types_txpool::{TxpoolInspect, TxpoolStatus};
use alloy_transport_http::Http;
use async_trait::async_trait;
use color_eyre::eyre;
use ultramarine_types::engine_api::ExecutionBlock;
use url::Url;

use super::EthRpc;

/// A client for the standard Ethereum JSON-RPC API, built on top of `alloy`.
///
/// This struct uses a concrete `RootProvider` for the `Ethereum` network,
/// which provides high-level methods for most standard RPC calls.
pub struct AlloyEthRpc {
    /// The underlying provider that handles RPC requests.
    /// The provider is generic over a `Network` (e.g., `Ethereum`), not just a transport.
    provider: RootProvider<Ethereum>,
}

impl AlloyEthRpc {
    pub fn new(url: Url) -> Self {
        // The initialization process follows a 3-step stack:
        // 1. Create the low-level HTTP transport from the URL.
        let transport = Http::new(url);
        // 2. Create a low-level `RpcClient` from the transport. The `true` flag tells the client to
        //    spawn a background task to manage the connection.
        let client = RpcClient::new(transport, true);
        // 3. Create the high-level `RootProvider` for the `Ethereum` network, which wraps the
        //    `RpcClient` and provides the user-friendly methods.
        let provider = RootProvider::new(client);
        Self { provider }
    }
}

impl fmt::Debug for AlloyEthRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlloyEthRpc").field("provider", &"<alloy RootProvider>").finish()
    }
}

#[async_trait]
impl EthRpc for AlloyEthRpc {
    /// Fetches the chain ID of the connected node.
    /// Corresponds to the `eth_chainId` RPC method.
    async fn get_chain_id(&self) -> eyre::Result<String> {
        let chain_id_u64 = self.provider.get_chain_id().await?;
        Ok(format!("0x{:x}", chain_id_u64))
    }

    /// Fetches the sync status of the connected node.
    /// Corresponds to the `eth_syncing` RPC method.
    async fn syncing(&self) -> eyre::Result<SyncStatus> {
        Ok(self.provider.syncing().await?)
    }

    /// Fetches logs matching a given filter.
    /// Corresponds to the `eth_getLogs` RPC method.
    async fn get_logs(&self, filter: &Filter) -> eyre::Result<Vec<Log>> {
        Ok(self.provider.get_logs(filter).await?)
    }

    /// Fetches a block by its number or tag (e.g., "latest").
    /// Corresponds to the `eth_getBlockByNumber` RPC method.
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        full_transactions: bool,
    ) -> eyre::Result<Option<ExecutionBlock>> {
        let mut request = self.provider.get_block(block_number.into());
        if full_transactions {
            request = request.full();
        }

        let maybe_full_block = request.await?;
        let execution_block = maybe_full_block.map(|block| ExecutionBlock {
            block_number: block.header.number,
            block_hash: block.header.hash,
            parent_hash: block.header.parent_hash,
            prev_randao: block.header.mix_hash,
            timestamp: block.header.timestamp,
        });
        Ok(execution_block)
    }

    /// Fetches the status of the node's transaction pool.
    /// Corresponds to the `txpool_status` RPC method.
    async fn txpool_status(&self) -> eyre::Result<TxpoolStatus> {
        // For RPC methods not covered by the high-level `Provider` trait,
        // we use `raw_request`. We must convert the method name into the
        // required `Cow<'static, str>` type using `.into()`.
        Ok(self.provider.raw_request("txpool_status".into(), ()).await?)
    }

    /// Fetches an inspection of the node's transaction pool.
    /// Corresponds to the `txpool_inspect` RPC method.
    async fn txpool_inspect(&self) -> eyre::Result<TxpoolInspect> {
        Ok(self.provider.raw_request("txpool_inspect".into(), ()).await?)
    }
}
