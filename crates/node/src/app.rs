#![allow(missing_docs)]
use alloy_rpc_types_engine::ExecutionPayloadV3;
use alloy_rpc_types_eth::BlockNumberOrTag;
use bytes::Bytes;
use color_eyre::eyre::{self, eyre};
use malachitebft_app_channel::{
    AppMsg, Channels, NetworkMsg,
    app::{
        streaming::StreamContent,
        types::{LocallyProposedValue, core::Round, sync::RawDecidedValue},
    },
};
use malachitebft_engine::host::Next;
use ssz::Encode;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use ultramarine_blob_engine::BlobEngine;
use ultramarine_consensus::state::State;
use ultramarine_execution::client::ExecutionClient;
use ultramarine_types::{
    context::LoadContext, engine_api::ExecutionBlock, height::Height, sync::SyncedValuePackage,
};

pub async fn run(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    execution_layer: ExecutionClient,
) -> eyre::Result<()> {
    info!("🚀 App message loop starting");
    while let Some(msg) = channels.consensus.recv().await {
        debug!("📨 Received message: {:?}", std::mem::discriminant(&msg));
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

                // Try to get the latest block from the execution engine.
                // If this fails, we'll lazy-fetch it later in GetValue instead of crashing.
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
                        warn!(
                            "Execution client returned no block for 'latest'; will lazy-fetch in GetValue"
                        );
                        state.latest_block = None;
                    }
                    Err(e) => {
                        warn!(%e, "Failed to fetch latest block from execution client; will lazy-fetch in GetValue");
                        state.latest_block = None;
                    }
                }

                // Calculate start_height following Malachite's pattern:
                // - If store has decided values, start at max_decided_height + 1
                // - Otherwise, start at Height(1) (Height::INITIAL)
                let max_decided = state.get_latest_decided_height().await;
                let start_height =
                    max_decided.map(|h| h.increment()).unwrap_or_else(|| Height::new(1));

                info!(?max_decided, %start_height, "Sending StartHeight to consensus engine.");

                if reply.send((start_height, state.get_validator_set().clone())).is_err() {
                    // If we can't reply, the consensus engine has likely crashed.
                    // We should return an error instead of just logging.
                    let e = eyre!("Failed to send StartHeight reply; consensus channel closed.");
                    error!("{}", e);
                    return Err(e);
                }
            }
            // The next message to handle is the `StartRound` message, signaling to the app
            // that consensus has entered a new round (including the initial round 0)
            AppMsg::StartedRound { height, round, proposer, role, reply_value } => {
                info!(%height, %round, %proposer, ?role, "🟢🟢 Started round");

                // We can use that opportunity to update our internal state
                state.current_height = height;
                state.current_round = round;
                state.current_proposer = Some(proposer);

                // Reply with any undecided values for this round (empty for now)
                // This is needed for crash recovery
                if reply_value.send(vec![]).is_err() {
                    error!("🔴 Failed to send StartedRound reply_value");
                }
            }
            // At some point, we may end up being the proposer for that round, and the consensus
            // engine will then ask us for a value to propose to the other validators.
            AppMsg::GetValue { height, round, timeout: _, reply } => {
                if let Err(e) =
                    handle_get_value(state, channels, &execution_layer, height, round, reply).await
                {
                    error!(
                        %height,
                        %round,
                        error = ?e,
                        "GetValue handler failed; letting consensus timeout drive prevote-nil"
                    );
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

            // NOTE: PeerJoined and PeerLeft variants were removed in latest malachite

            // In order to figure out if we can help a peer that is lagging behind,
            // the engine may ask us for the height of the earliest available value in our store.
            AppMsg::GetHistoryMinHeight { reply } => {
                let min_height = state.get_earliest_height().await;

                if reply.send(min_height).is_err() {
                    error!("Failed to send GetHistoryMinHeight reply");
                }
            }

            AppMsg::RestreamProposal { height, round, valid_round, address, value_id } => {
                if state.validator_address() != &address {
                    debug!(
                        %height, %round, %address,
                        our_address = %state.validator_address(),
                        "Ignoring RestreamProposal: validator mismatch"
                    );
                    continue;
                }

                info!(%height, %round, %value_id, %address, "Restreaming our own proposal");

                // The `valid_round` indicates the round in which the proposal gathered a POLC
                // (Proof-of-Lock-Change). If it's `Round::Nil`, the proposal was
                // for the original round.
                let proposal_round = if valid_round == Round::Nil { round } else { valid_round };

                match state.load_undecided_proposal(height, proposal_round).await {
                    Ok(Some(proposal)) => {
                        // Get the block data bytes
                        let proposal_bytes = match state
                            .get_block_data(height, proposal_round)
                            .await
                        {
                            Some(bytes) => bytes,
                            None => {
                                warn!(
                                    %height, %proposal_round, %value_id,
                                    "Block data not found for restreaming; it may have been pruned"
                                );
                                continue;
                            }
                        };

                        // Fetch blob metadata so we can rebuild headers deterministically
                        let blob_metadata = match state
                            .load_blob_metadata_for_round(height, proposal_round)
                            .await
                        {
                            Ok(Some(metadata)) => metadata,
                            Ok(None) => {
                                warn!(
                                    %height,
                                    %proposal_round,
                                    "Blob metadata missing for restream; skipping proposal"
                                );
                                continue;
                            }
                            Err(e) => {
                                error!(
                                    %height,
                                    %proposal_round,
                                    "Failed to load blob metadata for restream: {}",
                                    e
                                );
                                continue;
                            }
                        };

                        // Get blobs from blob_engine if they exist, then rebuild with fresh header
                        let restream_blob_sidecars = if blob_metadata.blob_count() == 0 {
                            None
                        } else {
                            match state
                                .blob_engine()
                                .get_undecided_blobs(height, proposal_round.as_i64())
                                .await
                            {
                                Ok(sidecars) if !sidecars.is_empty() => {
                                    match state.rebuild_blob_sidecars_for_restream(
                                        &blob_metadata,
                                        &address,
                                        &sidecars,
                                    ) {
                                        Ok(rebuilt) => Some(rebuilt),
                                        Err(e) => {
                                            error!(
                                                %height,
                                                %proposal_round,
                                                "Failed to rebuild blob sidecars for restream: {}",
                                                e
                                            );
                                            continue;
                                        }
                                    }
                                }
                                Ok(_) => {
                                    warn!(
                                        %height,
                                        %proposal_round,
                                        "Blob metadata expects blobs, but none found in store"
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    error!(%height, %proposal_round, "Failed to get blobs: {}", e);
                                    continue;
                                }
                            }
                        };

                        // If we're restreaming into a *new* round, replicate the proposal and
                        // payload under the target round locally so commit() can find it later.
                        if round != proposal_round {
                            let mut restreamed_value = proposal.clone();
                            restreamed_value.round = round;
                            restreamed_value.proposer = address.clone();

                            if let Err(e) = state.store_synced_proposal(restreamed_value).await {
                                error!(
                                    %height,
                                    %round,
                                    "Failed to store restreamed proposal for new round: {}",
                                    e
                                );
                                continue;
                            }

                            if let Err(e) = state
                                .store_undecided_proposal_data(
                                    height,
                                    round,
                                    proposal_bytes.clone(),
                                )
                                .await
                            {
                                error!(
                                    %height,
                                    %round,
                                    "Failed to store restreamed block bytes: {}",
                                    e
                                );
                                continue;
                            }

                            if let Err(e) = state
                                .put_blob_metadata_undecided(height, round, &blob_metadata)
                                .await
                            {
                                error!(
                                    %height,
                                    %round,
                                    "Failed to store restreamed blob metadata: {}",
                                    e
                                );
                                continue;
                            }

                            if let Some(ref sidecars) = restream_blob_sidecars {
                                if let Err(e) = state
                                    .blob_engine()
                                    .verify_and_store(height, round.as_i64(), sidecars)
                                    .await
                                {
                                    error!(
                                        %height,
                                        %round,
                                        "Failed to store restreamed blob sidecars: {}",
                                        e
                                    );
                                    continue;
                                }
                            }
                        }

                        // Create the locally proposed value for streaming
                        let locally_proposed_value = LocallyProposedValue {
                            height,
                            round, /* Note: we use the *current* round for the stream, not
                                    * necessarily the original proposal round. */
                            value: proposal.value,
                        };

                        // Stream the proposal with our address (we are the original proposer)
                        // Pass explicit proposer so the Init part reflects the original proposer
                        // address
                        for stream_message in state.stream_proposal(
                            locally_proposed_value,
                            proposal_bytes,
                            restream_blob_sidecars.as_deref(),
                            Some(address), // Explicit proposer for restreaming
                        ) {
                            info!(%height, %round, "Restreaming proposal part: {stream_message:?}");
                            if let Err(e) = channels
                                .network
                                .send(NetworkMsg::PublishProposalPart(stream_message))
                                .await
                            {
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

            // NOTE: GetValidatorSet variant was removed in latest malachite
            // Validator set is now provided via ConsensusReady message

            // After some time, consensus will finally reach a decision on the value
            // to commit for the current height, and will notify the application,
            // providing it with a commit certificate which contains the ID of the value
            // that was decided on as well as the set of commits for that value,
            // ie. the precommits together with their (aggregated) signatures.
            AppMsg::Decided { certificate, extensions: _, reply } => {
                let height = certificate.height;
                let round = certificate.round;

                // Realign latest execution block from disk so parent-hash checks
                // stay in sync after restarts/replays.
                if let Some(prev_height) = height.decrement() {
                    if let Ok(Some(prev_meta)) = state.get_blob_metadata(prev_height).await {
                        let prev_header = prev_meta.execution_payload_header();
                        let prev_block = ExecutionBlock {
                            block_hash: prev_header.block_hash,
                            block_number: prev_header.block_number,
                            parent_hash: prev_header.parent_hash,
                            timestamp: prev_header.timestamp,
                            prev_randao: prev_header.prev_randao,
                        };
                        let needs_realignment = state
                            .latest_block
                            .map(|blk| blk.block_number != prev_block.block_number)
                            .unwrap_or(true);
                        if needs_realignment {
                            debug!(
                                "[DIAG] Realigning latest execution block to height {} ({:?})",
                                prev_height,
                                prev_block.block_hash
                            );
                            state.latest_block = Some(prev_block);
                        }
                    }
                }

                // DIAGNOSTIC: Log entry to Decided handler
                debug!(
                    "[DIAG] Decided handler entry: height={}, round={}, address={}, value_id={}",
                    height,
                    round,
                    state.validator_address(),
                    certificate.value_id
                );

                info!(
                    %height, %round, value = %certificate.value_id,
                    "🟢🟢 Consensus has decided on value"
                );

                let Some(block_bytes) = state.get_block_data(height, round).await else {
                    // Keep the application loop alive: instruct consensus to replay this height.
                    error!(
                        "[DIAG] ❌ Missing block bytes for decided value at height {} round {} on node {}; requesting restart",
                        height,
                        round,
                        state.validator_address()
                    );
                    let _ = reply.send(Next::Restart(height, state.get_validator_set().clone()));
                    continue;
                };

                // DIAGNOSTIC: Confirm block bytes found
                debug!(
                    "[DIAG] ✅ Found block bytes: {} bytes for height {} round {}",
                    block_bytes.len(),
                    height,
                    round
                );

                if block_bytes.is_empty() {
                    error!(
                        "[DIAG] ❌ Empty block bytes for decided value at height {} round {}; requesting restart",
                        height,
                        round
                    );
                    let _ = reply.send(Next::Restart(height, state.get_validator_set().clone()));
                    continue;
                }

                debug!("🎁 block size: {:?}, height: {}", block_bytes.len(), height);

                let mut notifier = execution_layer.as_notifier();

                // DIAGNOSTIC: Log before processing certificate
                debug!(
                    "[DIAG] Calling process_decided_certificate for height {} round {}",
                    height, round
                );

                let outcome = state
                    .process_decided_certificate(&certificate, block_bytes.clone(), &mut notifier)
                    .await;

                // DIAGNOSTIC: Log outcome
                match &outcome {
                    Ok(o) => {
                        debug!(
                            "[DIAG] ✅ process_decided_certificate succeeded: {} txs, {} blobs, current_height now={}",
                            o.tx_count, o.blob_count, state.current_height
                        );
                    }
                    Err(e) => {
                        error!(
                            "[DIAG] ❌ process_decided_certificate failed for height {}: {}; requesting restart",
                            height, e
                        );
                        let _ = reply.send(Next::Restart(height, state.get_validator_set().clone()));
                        continue;
                    }
                }

                let outcome = outcome.unwrap();

                debug!(
                    height = %height,
                    txs = outcome.tx_count,
                    blobs = outcome.blob_count,
                    block_hash = ?outcome.execution_block.block_hash,
                    "✅ Decided certificate processed successfully"
                );

                // Pause briefly before starting next height, just to make following the logs easier
                // tokio::time::sleep(Duration::from_millis(500)).await;

                // DIAGNOSTIC: Log before sending Next::Start
                debug!(
                    "[DIAG] Sending Next::Start for height {} (after deciding height {})",
                    state.current_height, height
                );

                // And then we instruct consensus to start the next height
                if reply
                    .send(Next::Start(state.current_height, state.get_validator_set().clone()))
                    .is_err()
                {
                    error!("[DIAG] ❌ Failed to send Decided reply - channel closed?");
                    error!("Failed to send Decided reply");
                } else {
                    // DIAGNOSTIC: Confirm sent
                    debug!(
                        "[DIAG] ✅ Successfully sent Next::Start for height {} on node {}",
                        state.current_height,
                        state.validator_address()
                    );
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
            // PHASE 5.1 (Pre-V0 Sync): Unwrap SyncedValuePackage and store payload + blobs
            AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
                info!(%height, %round, "🟢🟢 Processing synced value - unwrapping package");

                // Decode the sync package
                let package = match SyncedValuePackage::decode(&value_bytes) {
                    Ok(pkg) => pkg,
                    Err(e) => {
                        error!(%height, %round, "Failed to decode SyncedValuePackage: {}", e);
                        // Send None to signal failure (Malachite protocol expects
                        // Option<ProposedValue>)
                        let _ = reply.send(None);
                        continue;
                    }
                };

                match state.process_synced_package(height, round, proposer.clone(), package).await {
                    Ok(Some(proposed_value)) => {
                        if reply.send(Some(proposed_value)).is_err() {
                            error!("Failed to send ProcessSyncedValue success reply");
                        } else {
                            info!(%height, %round, "✅ Successfully processed Full sync package");
                        }
                    }
                    Ok(None) => {
                        let _ = reply.send(None);
                    }
                    Err(e) => {
                        error!(%height, %round, "Failed to process synced package: {}", e);
                        let _ = reply.send(None);
                    }
                }
            }

            // If, on the other hand, we are not lagging behind but are instead asked by one of
            // our peer to help them catch up because they are the one lagging behind,
            // then the engine might ask the application to provide with the value
            // that was decided at some lower height. In that case, we fetch it from our store
            // and send it to consensus.
            //
            // PHASE 5.1 (Pre-V0 Sync): Bundle execution payload + blobs for syncing peers
            AppMsg::GetDecidedValue { height, reply } => {
                info!(%height, "🟢🟢 GetDecidedValue - bundling payload + blobs for sync");

                let decided_value = state.get_decided_value(height).await;

                let raw_decided_value = match decided_value {
                    Some(decided_value) => {
                        // Get round from certificate
                        let round = decided_value.certificate.round;

                        // Attempt to retrieve execution payload bytes
                        let payload_bytes = state.get_block_data(height, round).await;

                        // Attempt to retrieve blob sidecars
                        let blob_sidecars_result = state.blob_engine().get_for_import(height).await;

                        // Build the sync package
                        let package = match (payload_bytes, blob_sidecars_result) {
                            (Some(payload), Ok(blobs)) if !blobs.is_empty() => {
                                info!(
                                    %height,
                                    %round,
                                    blob_count = blobs.len(),
                                    payload_size = payload.len(),
                                    "Sending Full sync package"
                                );
                                SyncedValuePackage::Full {
                                    value: decided_value.value.clone(),
                                    execution_payload_ssz: payload,
                                    blob_sidecars: blobs,
                                }
                            }
                            (Some(payload), Ok(_blobs)) => {
                                // No blobs, but have payload
                                info!(
                                    %height,
                                    %round,
                                    payload_size = payload.len(),
                                    "Sending Full sync package (no blobs)"
                                );
                                SyncedValuePackage::Full {
                                    value: decided_value.value.clone(),
                                    execution_payload_ssz: payload,
                                    blob_sidecars: vec![],
                                }
                            }
                            _ => {
                                // In pre-v0 this shouldn't happen (no pruning yet)
                                // But provide safe fallback
                                error!(
                                    %height,
                                    %round,
                                    "Payload or blobs missing, sending MetadataOnly (this shouldn't happen in pre-v0!)"
                                );
                                SyncedValuePackage::MetadataOnly {
                                    value: decided_value.value.clone(),
                                }
                            }
                        };

                        // Encode package into value_bytes
                        match package.encode() {
                            Ok(value_bytes) => Some(RawDecidedValue {
                                certificate: decided_value.certificate,
                                value_bytes,
                            }),
                            Err(e) => {
                                error!(%height, %round, "Failed to encode SyncedValuePackage: {}", e);
                                None
                            }
                        }
                    }
                    None => {
                        info!(%height, "No decided value found at this height");
                        None
                    }
                };

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

async fn handle_get_value(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    execution_layer: &ExecutionClient,
    height: Height,
    round: Round,
    reply: oneshot::Sender<LocallyProposedValue<LoadContext>>,
) -> eyre::Result<()> {
    info!(%height, %round, "🟢🟢 Consensus is requesting a value to propose");

    let latest_block = ensure_latest_block(state, execution_layer).await?;
    debug!("Requesting EL to build payload with blobs on top of head");

    let (execution_payload, blobs_bundle) =
        execution_layer.generate_block_with_blobs(&latest_block).await?;

    let blob_count = blobs_bundle.as_ref().map(|b| b.len()).unwrap_or(0);
    debug!("🌈 Got execution payload with {} blobs", blob_count);

    let bytes = Bytes::from(execution_payload.as_ssz_bytes());
    debug!("🎁 block size: {:?}, height: {}, blobs: {}", bytes.len(), height, blob_count);

    let proposal = state
        .propose_value_with_blobs(
            height,
            round,
            bytes.clone(),
            &execution_payload,
            blobs_bundle.as_ref(),
        )
        .await?;
    state.store_undecided_proposal_data(height, round, bytes.clone()).await?;

    // DIAGNOSTIC: Confirm proposer stored block data
    info!(
        "[DIAG] ✅ Proposer stored block data: height={}, round={}, size={} bytes, address={}",
        height,
        round,
        bytes.len(),
        state.validator_address()
    );

    let (_signed_header, sidecars) = state
        .prepare_blob_sidecar_parts(&proposal, blobs_bundle.as_ref())
        .map_err(|e| eyre!("Failed to prepare blob sidecars: {}", e))?;

    let round_i64 = round.as_i64();
    let sidecars_slice = sidecars.as_slice();
    if !sidecars_slice.is_empty() {
        debug!(
            "Storing {} blobs locally for our own proposal at height {}, round {}",
            sidecars_slice.len(),
            height,
            round
        );
    }

    state
        .blob_engine()
        .verify_and_store(height, round_i64, sidecars_slice)
        .await
        .map_err(|e| eyre!("Proposer blob storage failed: {}", e))?;

    if !sidecars_slice.is_empty() {
        info!(
            "✅ Successfully stored {} blobs for height {} (proposer's own)",
            sidecars_slice.len(),
            height
        );
    }

    if reply.send(proposal.clone()).is_err() {
        return Err(eyre!("Failed to send GetValue reply; channel closed"));
    }

    let stream_sidecars = if sidecars.is_empty() { None } else { Some(sidecars.as_slice()) };
    for stream_message in state.stream_proposal(proposal, bytes, stream_sidecars, None) {
        info!(%height, %round, "Streaming proposal part: {stream_message:?}");
        channels
            .network
            .send(NetworkMsg::PublishProposalPart(stream_message))
            .await
            .map_err(|e| eyre!("Failed to stream proposal part: {}", e))?;
    }

    debug!(%height, %round, "✅ Proposal sent");
    Ok(())
}

async fn ensure_latest_block(
    state: &mut State,
    execution_layer: &ExecutionClient,
) -> eyre::Result<ExecutionBlock> {
    if let Some(block) = state.latest_block {
        return Ok(block);
    }

    warn!("latest execution block missing; refetching from EL");
    let block = execution_layer
        .eth
        .get_block_by_number(BlockNumberOrTag::Latest, false)
        .await
        .map_err(|e| eyre!("Failed to fetch latest block: {}", e))?
        .ok_or_else(|| eyre!("Execution client returned no block for 'latest'"))?;
    state.latest_block = Some(block);
    Ok(block)
}
