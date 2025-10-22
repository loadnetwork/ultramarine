#![allow(missing_docs)]
use alloy_rpc_types_engine::ExecutionPayloadV3;
use bytes::Bytes;
use color_eyre::eyre::{self, eyre};
use malachitebft_app_channel::{
    AppMsg, Channels, ConsensusMsg, NetworkMsg,
    app::{
        streaming::StreamContent,
        types::{
            LocallyProposedValue, ProposedValue,
            codec::Codec,
            core::{Round, Validity},
            sync::RawDecidedValue,
        },
    },
};
use ssz::{Decode, Encode};
use tracing::{debug, error, info};
use ultramarine_blob_engine::BlobEngine;
use ultramarine_consensus::state::{State, decode_value};
use ultramarine_execution::client::ExecutionClient;
use ultramarine_types::{
    aliases::{Block, BlockHash},
    blob::BlobsBundle,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::ExecutionBlock,
    sync::SyncedValueExtensions,
};

pub async fn run(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    execution_layer: ExecutionClient,
) -> eyre::Result<()> {
    while let Some(msg) = channels.consensus.recv().await {
        match msg {
            // The first message to handle is the `ConsensusReady` message, signaling to the app
            // that Malachite is ready to start consensus
            AppMsg::ConsensusReady { reply } => {
                info!("🟢🟢 Consensus is ready");
                // Node start-up: https://hackmd.io/@danielrachi/engine_api#Node-startup
                // Check compatibility with execution client
                if let Err(e) = execution_layer.check_capabilities().await {
                    error!("Execution client capability check failed: {}", e);
                    return Err(e);
                };

                info!("✅ Execution client capabilities check passed.");

                // Get the latest block from the execution engine
                match execution_layer
                    .eth
                    .get_block_by_number(alloy_rpc_types_eth::BlockNumberOrTag::Latest, false)
                    .await
                {
                    Ok(Some(latest_block)) => {
                        debug!(block_hash = %latest_block.block_hash, "Fetched latest block from execution client");
                        state.latest_block = Some(latest_block);
                    }
                    Ok(None) => {
                        let e = eyre!("Execution client returned no block for 'latest'");
                        error!("{}", e);
                        return Err(e)
                    }
                    Err(e) => {
                        error!("Failed to get latest block from execution client: {}", e);
                        return Err(e)
                    }
                }

                info!(start_height = %state.current_height, "Sending StartHeight to consensus engine.");

                if reply
                    .send(ConsensusMsg::StartHeight(
                        state.current_height,
                        state.get_validator_set().clone(),
                    ))
                    .is_err()
                {
                    // If we can't reply, the consensus engine has likely crashed.
                    // We should return an error instead of just logging.
                    let e = eyre!("Failed to send StartHeight reply; consensus channel closed.");
                    error!("{}", e);
                    return Err(e);
                }
            }
            // The next message to handle is the `StartRound` message, signaling to the app
            // that consensus has entered a new round (including the initial round 0)
            AppMsg::StartedRound { height, round, proposer } => {
                info!(%height, %round, %proposer, "🟢🟢 Started round");

                // We can use that opportunity to update our internal state
                state.current_height = height;
                state.current_round = round;
                state.current_proposer = Some(proposer);
            }
            // At some point, we may end up being the proposer for that round, and the consensus
            // engine will then ask us for a value to propose to the other validators.
            AppMsg::GetValue { height, round, timeout: _, reply } => {
                // TODO(round-0-timeout): On round 0 at startup, peers may still be wiring up and
                // miss the streamed proposal, leading to Prevote(nil) and rebroadcast loops.
                // Consider a small proposer grace (sleep until N-1 peers are connected or
                // an env-configured delay) or increase initial TimeoutConfig for dev/testnets.
               // NOTE: We can ignore the timeout as we are building the value right away.
                // If we were let's say reaping as many txes from a mempool and executing them,
                // then we would need to respect the timeout and stop at a certain point.

                info!(%height, %round, "🟢🟢 Consensus is requesting a value to propose");

                // Phase 3 Integration: Request execution payload WITH blobs
                let latest_block = state.latest_block.expect("Head block hash is not set");
                debug!("Requesting EL to build payload with blobs on top of head");

                // Call generate_block_with_blobs() instead of generate_block()
                let (execution_payload, blobs_bundle) = execution_layer
                    .generate_block_with_blobs(&latest_block)
                    .await?;

                // Log blob information
                let blob_count = blobs_bundle.as_ref().map(|b| b.len()).unwrap_or(0);
                debug!(
                    "🌈 Got execution payload with {} blobs",
                    blob_count
                );

                // Store block in state and propagate to peers.
                let bytes = Bytes::from(execution_payload.as_ssz_bytes());
                debug!("🎁 block size: {:?}, height: {}, blobs: {}", bytes.len(), height, blob_count);

                // Prepare block proposal using new method that creates proper metadata
                let proposal: LocallyProposedValue<LoadContext> =
                    state.propose_value_with_blobs(height, round, bytes.clone(), &execution_payload, blobs_bundle.as_ref()).await?;

                // When the node is not the proposer, store the block data,
                // which will be passed to the execution client (EL) on commit.
                state.store_undecided_proposal_data(bytes.clone()).await?;

                // Send it to consensus
                if reply.send(proposal.clone()).is_err() {
                    error!(%height, %round, "Failed to send GetValue reply; channel closed");
                }

                // Now what's left to do is to break down the value to propose into parts,
                // and send those parts over the network to our peers, for them to re-assemble the full value.
                // Phase 3: Stream with blobs!
                for stream_message in state.stream_proposal(proposal, bytes, blobs_bundle) {
                    info!(%height, %round, "Streaming proposal part: {stream_message:?}");
                    if let Err(e) = channels
                        .network
                        .send(NetworkMsg::PublishProposalPart(stream_message))
                        .await
                    {
                        error!(%height, %round, "Failed to stream proposal part: {e}");
                        return Err(e.into());
                    }
                }
                debug!(%height, %round, "✅ Proposal sent");
            }
                /*
                info!(%height, %round, ?timeout, "🟢🟢 Consensus is requesting a value to propose");

                // Define an async task to generate the full proposal.
                // All operations that can fail or take time are inside this block.
                let get_proposal_task = async {
                    let started = std::time::Instant::now();
                    let latest_block = state.latest_block.expect("Head block hash is not set");

                    // 1. Generate the block from the execution layer.
                    let payload = execution_layer.generate_block(&latest_block).await?;
                    let bytes = Bytes::from(payload.as_ssz_bytes());

                    // 2. Create the proposal value and store it.
                    let proposal = state.propose_value(height, round, bytes.clone()).await?;

                    // 3. Store the associated block data.
                    state.store_undecided_proposal_data(bytes.clone()).await?;

                    let elapsed = started.elapsed();
                    debug!(?elapsed, "Built proposal and prepared bytes");

                    // Return both the proposal for the reply and the bytes for streaming.
                    eyre::Result::<_>::Ok((proposal, bytes))
                };

                // Allow bypassing the timeout (malaketh-layered behavior) while tuning.
                let ignore_timeout = std::env::var("ULTRAMARINE_IGNORE_PROPOSE_TIMEOUT")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
                    .unwrap_or(false);

                if ignore_timeout {
                    match get_proposal_task.await {
                        Ok((proposal, bytes)) => {
                            if reply.send(proposal.clone()).is_err() {
                                error!("Failed to send GetValue reply; channel closed.");
                                return Ok(());
                            }
                            for stream_message in state.stream_proposal(proposal, bytes) {
                                if let Err(e) = channels.network.send(NetworkMsg::PublishProposalPart(stream_message)).await {
                                    error!("Failed to stream proposal part: {}", e);
                                    break;
                                }
                            }
                            debug!(%height, %round, "✅ Proposal sent and streamed");
                        }
                        Err(e) => {
                            error!("Failed to generate proposal: {}. Not replying; letting timeout drive prevote-nil.", e);
                        }
                    }
                    continue;
                }

                // Race the proposal generation against the consensus timeout.
                match tokio::time::timeout(timeout, get_proposal_task).await {
                    // Task completed successfully within the timeout.
                    Ok(Ok((proposal, bytes))) => {
                        debug!(
                            "Successfully generated proposal for height {} round {}",
                            height, round
                        );
                        // Reply to the consensus engine immediately.
                        if reply.send(proposal.clone()).is_err() {
                            // If we can't reply, the consensus engine has moved on. Nothing more to
                            // do.
                            error!("Failed to send GetValue reply; channel closed.");
                            return Ok(());
                        }

                        // Now that we've replied, stream the proposal parts.
                        // This part is not bound by the initial timeout.
                        for stream_message in state.stream_proposal(proposal, bytes) {
                            if let Err(e) = channels
                                .network
                                .send(NetworkMsg::PublishProposalPart(stream_message))
                                .await
                            {
                                error!("Failed to stream proposal part: {}", e);
                                break; // Stop streaming if network channel is closed.
                            }
                        }
                        debug!(%height, %round, "✅ Proposal sent and streamed");
                    }
                    // Task failed internally before the timeout.
                    Ok(Err(e)) => {
                        // Do not fabricate a proposal. Let consensus handle propose timeout
                        // and progress via prevote-nil.
                        error!(
                            "Failed to generate proposal: {}. Not replying; letting timeout drive prevote-nil.",
                            e
                        );
                    }
                    // Task took too long and timed out.
                    Err(_) => {
                        info!(?timeout, "GetValue task timed out; not replying; letting timeout drive prevote-nil.");
                }
            }
            }
*/
            AppMsg::ExtendVote { reply, .. } => {
                if reply.send(None).is_err() {
                    error!("🔴 Failed to send ExtendVote reply");
                }
            }
            AppMsg::VerifyVoteExtension { reply, .. } => {
                if reply.send(Ok(())).is_err() {
                    error!("🔴 Failed to send VerifyVoteExtension reply");
                }
            }

            AppMsg::PeerJoined { peer_id } => {
                info!(%peer_id, "🟢🟢 Peer joined our local view of network");

                // You might want to track connected peers in your state
                state.peers.insert(peer_id);
            }

            AppMsg::PeerLeft { peer_id } => {
                info!(%peer_id, "🔴 Peer left our local view of network");

                // Remove the peer from tracking
                state.peers.remove(&peer_id);
            }

            // In order to figure out if we can help a peer that is lagging behind,
            // the engine may ask us for the height of the earliest available value in our store.
            AppMsg::GetHistoryMinHeight { reply } => {
                let min_height = state.get_earliest_height().await;

                if reply.send(min_height).is_err() {
                    error!("Failed to send GetHistoryMinHeight reply");
                }
            }

            AppMsg::RestreamProposal { height: _, round: _, valid_round: _, address: _, value_id: _ } => {
                error!("🔴 RestreamProposal not implemented");
                /*
                info!(%height, %round, %value_id, %address, "Received request to restream proposal");

                // The `valid_round` indicates the round in which the proposal gathered a POLC (Proof-of-Lock-Change).
                // If it's `Round::Nil`, the proposal was for the original round.
                let proposal_round = if valid_round == Round::Nil {
                    round
                } else {
                    valid_round
                };

                match state.store.get_undecided_proposal(height, proposal_round, value_id).await {
                    Ok(Some(proposal)) => {
                        // Sanity check: ensure the proposal we found was from the correct original proposer.
                        if proposal.proposer != address {
                            error!(
                                "Found proposal for restreaming, but its proposer ({}) does not match the requested address ({}).",
                                proposal.proposer, address
                            );
                            return;
                        }

                        // We found the proposal in our store. Now, we need to stream its parts.
                        
                        // TODO: The `state.stream_proposal` function uses the address of the *current* node (`self.address`)
                        // when creating the `Init` part of the stream. For restreaming, it should use the address
                        // of the *original* proposer, which is available here as `address`.
                        // This requires either modifying `stream_proposal` to accept an optional proposer address,
                        // or creating a new `restream_proposal` function in `state.rs`.
                        
                        let locally_proposed_value = LocallyProposedValue {
                            height,
                            round, // Note: we use the *current* round for the stream, not necessarily the original proposal round.
                            value: proposal.value,
                        };

                        let proposal_bytes = Bytes::from(proposal.value.as_ssz_bytes());

                        for stream_message in state.stream_proposal(locally_proposed_value, proposal_bytes) {
                            info!(%height, %round, "Restreaming proposal part: {stream_message:?}");
                            if let Err(e) = channels.network.send(NetworkMsg::PublishProposalPart(stream_message)).await {
                                error!("Failed to restream proposal part: {}", e);
                                // If the network channel is closed, we can't continue.
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        // This can happen if we've already pruned the proposal from our store.
                        warn!(
                            %height, 
                            %proposal_round, 
                            %value_id, 
                            "Could not find proposal to restream. It might have been pruned."
                        );
                    }
                    Err(e) => {
                        error!("Failed to access store to restream proposal: {}", e);
                    }
                }
                */
            }

            // On the receiving end of these proposal parts (ie. when we are not the proposer),
            // we need to process these parts and re-assemble the full value.
            // To this end, we store each part that we receive and assemble the full value once we
            // have all its constituent parts. Then we send that value back to consensus for it to
            // consider and vote for or against it (ie. vote `nil`), depending on its validity.
            AppMsg::ReceivedProposalPart { from, part, reply } => {
                let (part_type, part_size) = match &part.content {
                    StreamContent::Data(part) => (part.get_type(), part.size_bytes()),
                    StreamContent::Fin => ("end of stream", 0),
                };

                info!(
                    %from, %part.sequence, part.type = %part_type, part.size = %part_size,
                    "Received proposal part"
                );

                match state.received_proposal_part(from, part).await {
                    Ok(proposed_value) => {
                        // This is the success path
                        if let Some(ref complete_proposal) = proposed_value {
                            debug!("✅ Received complete proposal: {:?}", complete_proposal);
                        }
                        if reply.send(proposed_value).is_err() {
                            error!("Failed to send ReceivedProposalPart reply");
                        }
                    }
                    Err(e) => {
                        // This is the failure path
                        error!("Error processing received proposal part: {}. Discarding.", e);
                        // We tell the engine that no proposal was completed.
                        if reply.send(None).is_err() {
                            error!("Failed to send ReceivedProposalPart reply after error");
                        }
                    }
                }
            }

            // In some cases, e.g. to verify the signature of a vote received at a higher height
            // than the one we are at (e.g. because we are lagging behind a little bit),
            // the engine may ask us for the validator set at that height.
            //
            // In our case, our validator set stays constant between heights so we can
            // send back the validator set found in our genesis state.
            // TODO: For a production client, this is a major simplification. This handler
            // must be updated to support dynamic validator sets. The `state.get_validator_set()`
            // function should be modified to accept the `height` parameter and look up the
            // correct historical validator set for that height.
            AppMsg::GetValidatorSet { height: _, reply } => {
                if reply.send(state.get_validator_set().clone()).is_err() {
                    error!("🔴 Failed to send GetValidatorSet reply");
                }
            }

            // After some time, consensus will finally reach a decision on the value
            // to commit for the current height, and will notify the application,
            // providing it with a commit certificate which contains the ID of the value
            // that was decided on as well as the set of commits for that value,
            // ie. the precommits together with their (aggregated) signatures.
            AppMsg::Decided { certificate, extensions: _, reply } => {
                let height = certificate.height;
                let round = certificate.round;
                info!(
                    %height, %round, value = %certificate.value_id,
                    "🟢🟢 Consensus has decided on value"
                );

                let Some(block_bytes) = state.get_block_data(height, round).await else {
                    let e = eyre!(
                        "Missing block bytes for decided value at height {} round {}",
                        height,
                        round
                    );
                    error!(%e, "Cannot decode decided value: block bytes not found");
                    return Err(e);
                };

                if block_bytes.is_empty() {
                    let e = eyre!(
                        "Empty block bytes for decided value at height {} round {}",
                        height,
                        round
                    );
                    error!(%e, "Cannot decode decided value: empty bytes");
                    return Err(e);
                }

                debug!("🎁 block size: {:?}, height: {}", block_bytes.len(), height);

                // Decode bytes into execution payload (a block)

                let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&block_bytes).unwrap();

                let parent_block_hash = execution_payload.payload_inner.payload_inner.parent_hash;
                let new_block_hash = execution_payload.payload_inner.payload_inner.block_hash;

                assert_eq!(state.latest_block.unwrap().block_hash, parent_block_hash);

                let new_block_timestamp = execution_payload.timestamp();
                let new_block_number = execution_payload.payload_inner.payload_inner.block_number;
                let new_block_prev_randao =
                    execution_payload.payload_inner.payload_inner.prev_randao;

                // Log stats

                let tx_count = execution_payload.payload_inner.payload_inner.transactions.len();
                state.txs_count += tx_count as u64;
                state.chain_bytes += block_bytes.len() as u64;
                let elapsed_time = state.start_time.elapsed();

                info!(
                    "👉 stats at height {}: #txs={}, txs/s={:.2}, chain_bytes={}, bytes/s={:.2}",
                    height,
                    state.txs_count,
                    state.txs_count as f64 / elapsed_time.as_secs_f64(),
                    state.chain_bytes,
                    state.chain_bytes as f64 / elapsed_time.as_secs_f64(),
                );

                debug!("🦄 Block at height {height} contains {tx_count} transactions");

                let block: Block = execution_payload.clone().try_into_block().unwrap();

                let versioned_hashes: Vec<BlockHash> =
                    block.body.blob_versioned_hashes_iter().copied().collect();

                // PHASE 5: Validate blob availability before import
                // Ensure blobs exist in blob_engine before finalizing block
                if !versioned_hashes.is_empty() {
                    debug!(
                        "Validating availability of {} blobs for height {}",
                        versioned_hashes.len(),
                        height
                    );

                    let blobs = state.blob_engine().get_for_import(height).await
                        .map_err(|e| eyre!("Failed to retrieve blobs for import at height {}: {}", height, e))?;

                    // Verify blob count matches versioned hashes
                    if blobs.len() != versioned_hashes.len() {
                        let e = eyre!(
                            "Blob count mismatch at height {}: blob_engine has {} blobs, but block expects {}",
                            height,
                            blobs.len(),
                            versioned_hashes.len()
                        );
                        error!(%e, "Cannot import block: blob availability check failed");
                        return Err(e);
                    }

                    // LIGHTHOUSE PARITY: Recompute versioned hashes from stored commitments
                    // and verify they match the payload hashes (defense-in-depth)
                    // See: lighthouse/beacon_node/execution_layer/src/engine_api/versioned_hashes.rs
                    use sha2::{Digest, Sha256};
                    let computed_hashes: Vec<BlockHash> = blobs.iter()
                        .map(|sidecar| {
                            // Hash the KZG commitment: SHA256(commitment)[0] = 0x01
                            let mut hash = Sha256::digest(sidecar.kzg_commitment.as_bytes());
                            hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
                            BlockHash::from_slice(&hash)
                        })
                        .collect();

                    // Verify computed hashes match payload hashes
                    if computed_hashes != versioned_hashes {
                        let e = eyre!(
                            "Versioned hash mismatch at height {}: \
                            computed from stored commitments != hashes in execution payload. \
                            This indicates either blob data corruption or a malicious proposal.",
                            height
                        );
                        error!(%e, "Cannot import block: versioned hash verification failed");
                        return Err(e);
                    }

                    info!(
                        "✅ Verified {} blobs available and versioned hashes match for height {}",
                        blobs.len(),
                        height
                    );
                }

                let payload_status =
                    execution_layer.notify_new_block(execution_payload, versioned_hashes).await?;
                if payload_status.is_invalid() {
                    return Err(eyre::eyre!("Invalid payload status: {}", payload_status.status))
                }

                debug!("💡 New block added at height {} with hash: {}", height, new_block_hash);

                // Notify the execution client (EL) of the new block.
                // Update the execution head state to this block.
                let latest_valid_hash =
                    execution_layer.set_latest_forkchoice_state(new_block_hash).await?;
                debug!(
                    "🚀 Forkchoice updated to height {} for block hash={} and latest_valid_hash={}",
                    height, new_block_hash, latest_valid_hash
                );

                // When that happens, we store the decided value in our store
                state.commit(certificate).await?;

                // Save the latest block
                state.latest_block = Some(ExecutionBlock {
                    block_hash: new_block_hash,
                    block_number: new_block_number,
                    parent_hash: latest_valid_hash,
                    timestamp: new_block_timestamp,
                    prev_randao: new_block_prev_randao,
                });

                // Pause briefly before starting next height, just to make following the logs easier
                // tokio::time::sleep(Duration::from_millis(500)).await;

                // And then we instruct consensus to start the next height
                if reply
                    .send(ConsensusMsg::StartHeight(
                        state.current_height,
                        state.get_validator_set().clone(),
                    ))
                    .is_err()
                {
                    error!("Failed to send Decided reply");
                }
            }

            // It may happen that our node is lagging behind its peers. In that case,
            // a synchronization mechanism will automatically kick to try and catch up to
            // our peers. When that happens, some of these peers will send us decided values
            // for the heights in between the one we are currently at (included) and the one
            // that they are at. When the engine receives such a value, it will forward to the
            // application to decode it from its wire format and send back the decoded
            // value to consensus.
            //
            // TODO: store the received value somewhere here
            AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
                info!(%height, %round, "🟢🟢 Processing synced value");

                let value = decode_value(value_bytes);

                // We send to consensus to see if it has been decided on
                if reply
                    .send(ProposedValue {
                        height,
                        round,
                        valid_round: Round::Nil,
                        proposer,
                        value,
                        validity: Validity::Valid,
                    })
                    .is_err()
                {
                    error!("Failed to send ProcessSyncedValue reply");
                }
                                /*
                // TODO: This handler is critical for state sync to work correctly.
                // The commented-out code below provides a robust implementation.
                // When a synced value is received, it MUST be stored in the application's
                // undecided store before being sent back to the consensus engine. This ensures
                // that when the engine later sends an `AppMsg::Decided` message for this height,
                // our application can find the proposal in its own database to commit.
                // Failure to store the value here will cause the commit to fail.

                info!(%height, %round, "🟢🟢 Processing synced value");

                // First, decode the raw bytes into a proper Value.
                let value = match decode_value(value_bytes) {
                    Some(v) => v,
                    None => {
                        error!("Failed to decode synced value for height {}", height);
                        // If we can't decode it, we can't process it.
                        // Sending `None` tells the engine the value was invalid.
                        if reply.send(None).is_err() {
                            error!("Failed to send ProcessSyncedValue reply for invalid value");
                        }
                        return; // Stop processing
                    }
                };

                // Create the full ProposedValue struct.
                let proposed_value = ProposedValue {
                    height,
                    round,
                    valid_round: Round::Nil, // Synced values are already committed, so POL round is not relevant here.
                    proposer,
                    value,
                    validity: Validity::Valid, // We assume synced values from peers are valid.
                };

                // Before replying, store the proposal in our own database.
                if let Err(e) = state.store.store_undecided_proposal(proposed_value.clone()).await {
                    error!("Failed to store synced value for height {}: {}", height, e);
                    // If we can't store it, we can't proceed with the commit later.
                    // It's better to tell the engine this value is invalid.
                    if reply.send(None).is_err() {
                        error!("Failed to send ProcessSyncedValue reply for storage failure");
                    }
                    return;
                }

                // Now, send the valid, stored ProposedValue to consensus.
                if reply.send(Some(proposed_value)).is_err() {
                    error!("Failed to send ProcessSyncedValue reply");
                }
                */

            }

            // If, on the other hand, we are not lagging behind but are instead asked by one of
            // our peer to help them catch up because they are the one lagging behind,
            // then the engine might ask the application to provide with the value
            // that was decided at some lower height. In that case, we fetch it from our store
            // and send it to consensus.
            AppMsg::GetDecidedValue { height, reply } => {
                info!(%height, "🟢🟢 GetDecidedValue");
                let decided_value = state.get_decided_value(height).await;

                let raw_decided_value = decided_value.map(|decided_value| RawDecidedValue {
                    certificate: decided_value.certificate,
                    value_bytes: ProtobufCodec
                        .encode(&decided_value.value)
                        .expect("Not correct value bytes"),
                });

                if reply.send(raw_decided_value).is_err() {
                    error!("Failed to send GetDecidedValue reply");
                }
            }
        }
    }

    // If we get there, it can only be because the channel we use to receive message
    // from consensus has been closed, meaning that the consensus actor has died.
    // We can do nothing but return an error here.
    Err(eyre!("Consensus channel closed unexpectedly"))
}
