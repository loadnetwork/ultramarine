use core::fmt;
use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::Address;
use alloy_rpc_types_txpool::TxpoolStatus;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use color_eyre::eyre::{self, Result};
use hex::FromHex;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Instant, sleep},
};

use crate::tx::{make_signed_eip1559_tx, make_signed_eip4844_tx};

#[derive(Parser, Debug, Clone, PartialEq)]
pub struct SpamCmd {
    /// URL of the execution client's RPC endpoint
    #[clap(long, default_value = "http://127.0.0.1:8545")]
    rpc_url: Url,
    /// Chain ID to use for signing transactions (if omitted, fetched from the node)
    #[clap(long)]
    chain_id: Option<u64>,
    /// Number of transactions to send (0 for no limit).
    #[clap(short, long, default_value = "0")]
    num_txs: u64,
    /// Rate of transactions per second.
    #[clap(short, long, default_value = "1000")]
    rate: u64,
    /// Time to run the spammer for in seconds (0 for no limit).
    #[clap(short, long, default_value = "0")]
    time: u64,
    /// Spam EIP-4844 (blob) transactions instead of EIP-1559
    #[clap(long, default_value = "false")]
    blobs: bool,
    /// Number of blobs per EIP-4844 transaction (1-1024)
    #[clap(long, default_value = "128")]
    blobs_per_tx: usize,
    /// Index of the signer to use
    #[clap(long, default_value = "0")]
    signer_index: usize,
    /// Optional 0x-prefixed hex private key to use as the signer (overrides signer_index).
    #[clap(long)]
    private_key: Option<String>,
}

impl SpamCmd {
    pub async fn run(&self) -> Result<()> {
        // Determine chain id: use CLI override or fetch via eth_chainId
        let chain_id = if let Some(id) = self.chain_id {
            id
        } else {
            let client = RpcClient::new(self.rpc_url.clone());
            let chain_id_hex: String = client.rpc_request("eth_chainId", json!([])).await?;
            let hex = chain_id_hex.strip_prefix("0x").unwrap_or(&chain_id_hex);
            u64::from_str_radix(hex, 16)?
        };

        let spammer = Spammer::new(
            self.rpc_url.clone(),
            chain_id,
            self.num_txs,
            self.time,
            self.rate,
            self.blobs,
            self.blobs_per_tx,
            self.signer_index,
            self.private_key.clone(),
        )?;
        spammer.run().await
    }
}

/// A transaction spammer that sends Ethereum transactions at a controlled rate.
/// Tracks and reports statistics on sent transactions.
pub struct Spammer {
    /// Client for Ethereum RPC node server.
    client: RpcClient,
    /// Chain ID used for signing.
    chain_id: u64,
    /// Ethereum transaction signer.
    signer: PrivateKeySigner,
    /// Maximum number of transactions to send (0 for no limit).
    max_num_txs: u64,
    /// Maximum number of seconds to run the spammer (0 for no limit).
    max_time: u64,
    /// Maximum number of transactions to send per second.
    max_rate: u64,
    /// Whether to send EIP-4844 blob transactions.
    blobs: bool,
    /// Number of blobs per EIP-4844 transaction (1-1024).
    blobs_per_tx: usize,
}

impl Spammer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: Url,
        chain_id: u64,
        max_num_txs: u64,
        max_time: u64,
        max_rate: u64,
        blobs: bool,
        blobs_per_tx: usize,
        signer_index: usize,
        private_key: Option<String>,
    ) -> Result<Self> {
        if blobs && !(1..=1024).contains(&blobs_per_tx) {
            return Err(eyre::eyre!("blobs_per_tx must be between 1 and 1024"));
        }
        let signer = if let Some(pk) = private_key {
            let pk = pk.strip_prefix("0x").unwrap_or(&pk);
            let bytes = <[u8; 32]>::from_hex(pk)
                .map_err(|e| eyre::eyre!("invalid --private-key (expected 32-byte hex): {e}"))?;
            PrivateKeySigner::from_slice(&bytes)
                .map_err(|e| eyre::eyre!("failed to build signer from --private-key: {e}"))?
        } else {
            let signers = ultramarine_genesis::make_signers();
            signers
                .get(signer_index)
                .ok_or_else(|| eyre::eyre!("Invalid signer index"))?
                .clone()
        };
        Ok(Self {
            client: RpcClient::new(url),
            chain_id,
            signer,
            max_num_txs,
            max_time,
            max_rate,
            blobs,
            blobs_per_tx,
        })
    }

    pub async fn run(self) -> Result<()> {
        // Create channels for communication between spammer and tracker.
        let (result_sender, result_receiver) = mpsc::channel::<Result<u64>>(10000);
        let (report_sender, report_receiver) = mpsc::channel::<Instant>(1);
        let (finish_sender, finish_receiver) = mpsc::channel::<()>(1);

        let self_arc = Arc::new(self);

        // Spawn spammer.
        let spammer_handle = tokio::spawn({
            let self_arc = Arc::clone(&self_arc);
            async move { self_arc.spammer(result_sender, report_sender, finish_sender).await }
        });

        // Spawn result tracker.
        let tracker_handle = tokio::spawn({
            let self_arc = Arc::clone(&self_arc);
            async move { self_arc.tracker(result_receiver, report_receiver, finish_receiver).await }
        });

        let (spammer_res, tracker_res) = tokio::join!(spammer_handle, tracker_handle);
        spammer_res.map_err(|e| eyre::eyre!("spammer task failed: {e}"))??;
        tracker_res.map_err(|e| eyre::eyre!("tracker task failed: {e}"))??;
        Ok(())
    }

    // Fetch from an Ethereum node the latest used nonce for the given address.
    async fn get_latest_nonce(&self, address: Address) -> Result<u64> {
        let response: String =
            self.client.rpc_request("eth_getTransactionCount", json!([address, "pending"])).await?;
        // Convert hex string to integer.
        let hex_str = response.as_str().strip_prefix("0x").unwrap_or(&response);
        Ok(u64::from_str_radix(hex_str, 16)?)
    }

    /// Generate and send transactions to the Ethereum node at a controlled rate.
    async fn spammer(
        &self,
        result_sender: Sender<Result<u64>>,
        report_sender: Sender<Instant>,
        finish_sender: Sender<()>,
    ) -> Result<()> {
        // Fetch latest nonce for the sender address.
        let address = self.signer.address();
        let latest_nonce = self.get_latest_nonce(address).await?;
        println!("Spamming from {address:?} starting from nonce={latest_nonce}");

        // Initialize nonce and counters.
        let mut nonce = latest_nonce;
        let start_time = Instant::now();
        let mut txs_sent_total = 0u64;
        let mut interval = time::interval(Duration::from_secs(1));
        loop {
            // Wait for next one-second tick.
            let _ = interval.tick().await;
            let mut txs_sent_in_interval = 0u64;
            let interval_start = Instant::now();

            // Resync the nonce once per tick to avoid getting stuck if a previous send was
            // accepted by the node but our local nonce bookkeeping didn't advance.
            if let Ok(latest) = self.get_latest_nonce(address).await {
                nonce = nonce.max(latest);
            }

            // Send up to max_rate transactions per one-second interval.
            while txs_sent_in_interval < self.max_rate {
                // Check exit conditions before sending each transaction.
                if (self.max_num_txs > 0 && txs_sent_total >= self.max_num_txs) ||
                    (self.max_time > 0 && start_time.elapsed().as_secs() >= self.max_time)
                {
                    break;
                }

                // Create one transaction and sign it.
                let to_address = Address::with_last_byte((txs_sent_total % 256) as u8);
                let (tx_bytes, tx_bytes_len) = if self.blobs {
                    // EIP-4844 blob transaction (with real versioned hashes)
                    let signed_tx = make_signed_eip4844_tx(
                        &self.signer,
                        nonce,
                        to_address,
                        self.chain_id,
                        self.blobs_per_tx,
                    )
                    .await?;
                    let bytes = signed_tx.encoded_2718();
                    let len = bytes.len() as u64;
                    (bytes, len)
                } else {
                    // EIP-1559 transaction
                    let signed_tx =
                        make_signed_eip1559_tx(&self.signer, nonce, to_address, self.chain_id)
                            .await?;
                    let bytes = signed_tx.encoded_2718();
                    let len = bytes.len() as u64;
                    (bytes, len)
                };

                // Send transaction to Ethereum RPC endpoint.
                let payload = hex::encode(tx_bytes);
                let result = self
                    .client
                    .rpc_request("eth_sendRawTransaction", json!([format!("0x{}", payload)]))
                    .await
                    .map(|_: String| tx_bytes_len);

                // Report result and update counters.
                match &result {
                    Ok(_) => {
                        // Transaction accepted by node
                        nonce += 1;
                        txs_sent_total += 1;
                        txs_sent_in_interval += 1;
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        // Count the attempt for rate limiting regardless of outcome.
                        txs_sent_in_interval += 1;

                        // Nonce handling:
                        // - "already known": treat as accepted (node already has it); advance nonce
                        // - "nonce too low"/"replacement transaction underpriced": we're behind or
                        //   raced ourselves; resync from pending nonce
                        // - "insufficient funds": do NOT advance nonce (would create gaps); abort
                        if err_msg.contains("already known") {
                            nonce += 1;
                            txs_sent_total += 1;
                        } else if err_msg.contains("nonce too low") ||
                            err_msg.contains("replacement transaction underpriced")
                        {
                            // Resync nonce from the node to avoid creating gaps / getting stuck.
                            if let Ok(latest) = self.get_latest_nonce(address).await {
                                nonce = nonce.max(latest);
                            }
                        } else if err_msg.contains("insufficient funds") {
                            return Err(eyre::eyre!("{err_msg}"));
                        }
                    }
                }
                result_sender.send(result).await?;
            }

            // Give time to the in-flight results to be received.
            sleep(Duration::from_millis(20)).await;

            // Signal tracker to report stats after this batch.
            report_sender.try_send(interval_start)?;

            // Check exit conditions after each tick.
            if (self.max_num_txs > 0 && txs_sent_total >= self.max_num_txs) ||
                (self.max_time > 0 && start_time.elapsed().as_secs() >= self.max_time)
            {
                break;
            }
        }
        finish_sender.send(()).await?;
        Ok(())
    }

    // Track and report statistics on sent transactions.
    async fn tracker(
        &self,
        mut result_receiver: Receiver<Result<u64>>,
        mut report_receiver: Receiver<Instant>,
        mut finish_receiver: Receiver<()>,
    ) -> Result<()> {
        // Initialize counters
        let start_time = Instant::now();
        let mut stats_total = Stats::new(start_time);
        let mut stats_last_second = Stats::new(start_time);
        loop {
            tokio::select! {
                // Update counters
                Some(res) = result_receiver.recv() => {
                    match res {
                        Ok(tx_length) => stats_last_second.incr_ok(tx_length),
                        Err(error) => stats_last_second.incr_err(&error.to_string()),
                    }
                }
                // Report stats
                Some(interval_start) = report_receiver.recv() => {
                    // Wait what's missing to complete one second.
                    let elapsed = interval_start.elapsed();
                    if elapsed < Duration::from_secs(1) {
                        sleep(Duration::from_secs(1) - elapsed).await;
                    }

            let pool_status: Result<TxpoolStatus> =
                self.client.rpc_request("txpool_status", json!([])).await;
            match pool_status {
                Ok(status) => {
                    println!(
                        "{stats_last_second}; pending_txpool={}; queued_txpool={}",
                        status.pending, status.queued
                    );
                }
                Err(err) => {
                    println!("{stats_last_second}; txpool_status_error={err}");
                }
            }

                    // Update total, then reset last second stats
                    stats_total.add(&stats_last_second);
                    stats_last_second.reset();
                }
                // Stop tracking
                _ = finish_receiver.recv() => {
                    break;
                }
            }
        }
        println!("Total: {stats_total}");
        Ok(())
    }
}

/// Statistics on sent transactions.
struct Stats {
    start_time: Instant,
    succeed: u64,
    bytes: u64,
    errors_counter: HashMap<String, u64>,
}

impl Stats {
    fn new(start_time: Instant) -> Self {
        Self { start_time, succeed: 0, bytes: 0, errors_counter: HashMap::new() }
    }

    fn incr_ok(&mut self, tx_length: u64) {
        self.succeed += 1;
        self.bytes += tx_length;
    }

    fn incr_err(&mut self, error: &str) {
        self.errors_counter.entry(error.to_string()).and_modify(|count| *count += 1).or_insert(1);
    }

    fn add(&mut self, other: &Self) {
        self.succeed += other.succeed;
        self.bytes += other.bytes;
        for (error, count) in &other.errors_counter {
            self.errors_counter
                .entry(error.to_string())
                .and_modify(|c| *c += count)
                .or_insert(*count);
        }
    }

    fn reset(&mut self) {
        self.succeed = 0;
        self.bytes = 0;
        self.errors_counter.clear();
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let elapsed = self.start_time.elapsed().as_millis();
        let stats = format!(
            "elapsed {:.3}s: Sent {} txs ({} bytes)",
            elapsed as f64 / 1000f64,
            self.succeed,
            self.bytes
        );
        let stats_failed = if self.errors_counter.is_empty() {
            String::new()
        } else {
            let failed = self.errors_counter.values().copied().sum::<u64>();
            format!("; {} failed with {:?}", failed, self.errors_counter)
        };
        write!(f, "{stats}{stats_failed}")
    }
}

struct RpcClient {
    client: Client,
    url: Url,
}

impl RpcClient {
    pub fn new(url: Url) -> Self {
        let client = Client::new();
        Self { client, url }
    }

    pub async fn rpc_request<D: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<D> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });
        let request = self
            .client
            .post(self.url.clone())
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .json(&body);
        let body: JsonResponseBody = request.send().await?.error_for_status()?.json().await?;

        if let Some(JsonError { code, message }) = body.error {
            Err(eyre::eyre!("Server Error {}: {}", code, message))
        } else {
            serde_json::from_value(body.result).map_err(Into::into)
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonResponseBody {
    pub jsonrpc: String,
    #[serde(default)]
    pub error: Option<JsonError>,
    #[serde(default)]
    pub result: serde_json::Value,
    pub id: serde_json::Value,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonError {
    pub code: i64,
    pub message: String,
}
