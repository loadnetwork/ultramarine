use alloy_consensus::error;
use alloy_rpc_types_engine::{ExecutionPayloadV3, payload};
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
use tokio::time::timeout;
use tracing::{debug, error, info};
use ultramarine_consensus::state::{State, decode_value};
use ultramarine_execution::client::ExecutionClient;
use ultramarine_types::{
    aliases::{Block, BlockHash},
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::ExecutionBlock,
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
            AppMsg::GetValue { height, round, timeout, reply } => {
                info!(%height, %round, "🟢🟢 Consensus is requesting a value to propose");

                // Define an async task to generate the full proposal.
                // All operations that can fail or take time are inside this block.
                let get_proposal_task = async {
                    let latest_block = state.latest_block.expect("Head block hash is not set");

                    // 1. Generate the block from the execution layer.
                    let payload = execution_layer.generate_block(&latest_block).await?;
                    let bytes = Bytes::from(payload.as_ssz_bytes());

                    // 2. Create the proposal value and store it.
                    let proposal = state.propose_value(height, round, bytes.clone()).await?;

                    // 3. Store the associated block data.
                    state.store_undecided_proposal_data(bytes.clone()).await?;
                    // Return both the proposal for the reply and the bytes for streaming.
                    eyre::Result::<_>::Ok((proposal, bytes))
                };

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
                        error!("Failed to generate proposal: {}. Proposing nil.", e);
                        // Attempt to propose a nil value.
                        if let Ok(nil_proposal) =
                            state.propose_value(height, round, Bytes::new()).await
                        {
                            if reply.send(nil_proposal).is_err() {
                                error!("Failed to send GetValue nil reply; channel closed.");
                            }
                        } else {
                            error!(
                                "Failed to create even a nil proposal. Letting consensus time out."
                            );
                        }
                    }
                    // Task took too long and timed out.
                    Err(_) => {
                        info!("GetValue task timed out after {:?}. Proposing nil.", timeout);
                        // Attempt to propose a nil value.
                        if let Ok(nil_proposal) =
                            state.propose_value(height, round, Bytes::new()).await
                        {
                            if reply.send(nil_proposal).is_err() {
                                error!("Failed to send GetValue nil reply; channel closed.");
                            }
                        } else {
                            error!(
                                "Failed to create even a nil proposal. Letting consensus time out."
                            );
                        }
                    }
                }
            }
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

            AppMsg::RestreamProposal { height, round, valid_round, address, value_id } => {
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

            AppMsg::Decided { certificate, extensions, reply } => {
                unimplemented!()
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
                    value_bytes: ProtobufCodec.encode(&decided_value.value).unwrap(),
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
