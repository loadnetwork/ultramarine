#![allow(missing_docs)]
use std::time::{Duration, Instant};

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
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use ultramarine_blob_engine::BlobEngine;
use ultramarine_consensus::state::State;
use ultramarine_execution::{ExecutionError, ExecutionNotifier, client::ExecutionClient};
use ultramarine_types::{
    archive::ArchiveNotice,
    context::LoadContext,
    engine_api::{ExecutionBlock, load_prev_randao},
    height::Height,
    proposal_part::ProposalPart,
    sync::SyncedValuePackage,
};

use crate::archiver::ArchiveJobSubmitter;

pub async fn run(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    execution_layer: ExecutionClient,
    archiver_job_tx: Option<ArchiveJobSubmitter>,
    mut archive_notice_rx: Option<mpsc::Receiver<ArchiveNotice>>,
    shutdown: CancellationToken,
) -> eyre::Result<()> {
    info!("🚀 App message loop starting");
    state.rehydrate_pending_prunes().await?;

    loop {
        // Use tokio::select! to poll shutdown, consensus channel, and archive notice channel
        // The shutdown branch ensures we exit immediately when signaled, not just at loop start
        let msg = tokio::select! {
            // Check for shutdown signal - this allows immediate exit when signaled
            _ = shutdown.cancelled() => {
                info!("Shutdown signal detected, exiting main loop...");
                None
            }
            // Poll consensus channel
            consensus_msg = channels.consensus.recv() => {
                match consensus_msg {
                    Some(msg) => Some(msg),
                    None => {
                        // Consensus channel closed - exit the loop
                        return Err(eyre!("Consensus channel closed unexpectedly"));
                    }
                }
            }
            // Poll archive notice channel if available
            notice = async {
                match &mut archive_notice_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match notice {
                    Some(notice) => {
                        // Handle archive notice from the archiver worker
                        debug!(
                            height = %notice.body.height,
                            blob_index = %notice.body.blob_index,
                            "📦 Received archive notice from worker"
                        );
                        if let Err(e) = state.handle_archive_notice(notice.clone()).await {
                            error!("Failed to handle archive notice: {}", e);
                        }

                        // Broadcast notice to peers via ProposalPart::ArchiveNotice
                        let broadcast_start = std::time::Instant::now();
                        let stream_message = state.stream_archive_notice(notice.clone());
                        match channels
                            .network
                            .send(NetworkMsg::PublishProposalPart(stream_message))
                            .await
                        {
                            Ok(_) => {
                                state.observe_notice_propagation(broadcast_start.elapsed());
                                info!(
                                    height = %notice.body.height,
                                    index = %notice.body.blob_index,
                                    "📡 Broadcast ArchiveNotice from worker"
                                );
                            }
                            Err(e) => {
                                error!(
                                    height = %notice.body.height,
                                    index = %notice.body.blob_index,
                                    error = %e,
                                    "Failed to broadcast ArchiveNotice from worker"
                                );
                            }
                        }
                    }
                    None => {
                        // Archive notice channel closed (worker stopped)
                        // Clear the receiver so we fall through to pending() and don't spin
                        warn!("Archive notice channel closed, archiver worker stopped");
                        archive_notice_rx = None;
                    }
                }
                // Continue polling - don't exit the loop
                continue;
            }
        };

        // Exit loop if shutdown was signaled
        let Some(msg) = msg else {
            break;
        };

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

                // Establish latest_block for timestamp/parent validation.
                // Prefer consensus store (decided metadata) so restarts don't regress
                // to an EL that is lagging or restarting.
                if let Some(decided_block) = load_decided_block(state).await? {
                    info!(
                        height = %decided_block.block_number,
                        block_hash = %decided_block.block_hash,
                        "Initialized latest_block from consensus store"
                    );
                    state.latest_block = Some(decided_block);
                }

                if state.latest_block.is_none() {
                    return Err(eyre!(
                        "latest_block is None after startup; execution genesis not initialized"
                    ));
                }

                // Calculate start_height following Malachite's pattern:
                // - If store has decided values, start at max_decided_height + 1
                // - Otherwise, start at Height(1) (Height::INITIAL)
                let max_decided = state.get_latest_decided_height().await;
                let start_height =
                    max_decided.map(|h| h.increment()).unwrap_or_else(|| Height::new(1));

                // Best-effort alignment: apply FCU to the CL decided head. If EL reports
                // SYNCING/INVALID, enter observer-only mode until EL catches up.
                if let Err(e) =
                    ensure_el_matches_cl_head(state, &execution_layer, "consensus_ready").await
                {
                    warn!(
                        error = %e,
                        "EL not aligned with CL head at startup; proposals/votes will be gated until alignment"
                    );
                }

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

                // Cleanup stale round blobs to prevent memory/storage leak when consensus is stuck.
                // This handles the case where rounds keep timing out but no commit happens.
                // Only cleanup after round 0 (we need at least one previous round to have stale
                // data).
                if round.as_u32().is_some_and(|r| r > 0) &&
                    let Err(e) = state.cleanup_stale_round_blobs(height, round).await
                {
                    warn!(
                        %height,
                        %round,
                        error = %e,
                        "Failed to cleanup stale round blobs"
                    );
                    // Don't fail the round start - this is best-effort cleanup
                }

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
            AppMsg::GetValue { height, round, timeout, reply } => {
                match handle_get_value(
                    state,
                    channels,
                    &execution_layer,
                    height,
                    round,
                    timeout,
                    reply,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        error!(
                            %height,
                            %round,
                            error = ?e,
                            "GetValue handler failed; timeout will drive prevote-nil"
                        );
                        // NOTE: Not sending reply here - Malachite will timeout and do prevote-nil.
                        // LocallyProposedValue doesn't have a "nil" representation.
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
                            Ok(Some(bytes)) => bytes,
                            Ok(None) => {
                                warn!(
                                    %height, %proposal_round, %value_id,
                                    "Block data not found for restreaming; it may have been pruned"
                                );
                                continue;
                            }
                            Err(e) => {
                                error!(
                                    %height, %proposal_round, %value_id,
                                    "Failed to load block data for restream: {}",
                                    e
                                );
                                continue;
                            }
                        };

                        let execution_requests =
                            match state.get_execution_requests(height, proposal_round).await {
                                Ok(maybe) => maybe.unwrap_or_default(),
                                Err(e) => {
                                    error!(
                                        %height,
                                        %proposal_round,
                                        "Failed to load execution requests for restream: {}",
                                        e
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

                        // Fetch blobs via serving contract; fall back to metadata-only when pruned
                        let mut pruned_locators: Option<Vec<String>> = None;
                        let restream_blob_sidecars = if blob_metadata.blob_count() == 0 {
                            None
                        } else {
                            match state
                                .get_undecided_blobs_with_status_check(height, proposal_round)
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
                                Err(ultramarine_blob_engine::BlobEngineError::BlobsPruned {
                                    locators,
                                    ..
                                }) => {
                                    pruned_locators = Some(locators.clone());
                                    warn!(
                                        %height,
                                        %proposal_round,
                                        locator_count = %locators.len(),
                                        "Blobs pruned for restream; sending metadata-only with archive notices"
                                    );
                                    if let Some(first) = locators.first() {
                                        debug!(
                                            %height,
                                            %proposal_round,
                                            locator = %first,
                                            "First locator for pruned restream height"
                                        );
                                    }
                                    if let Err(e) =
                                        restream_archive_notices(state, channels, height).await
                                    {
                                        error!(
                                            %height,
                                            "Failed to restream archive notices for pruned height: {}",
                                            e
                                        );
                                    }
                                    None
                                }
                                Err(e) => {
                                    error!(%height, %proposal_round, "Failed to get blobs: {}", e);
                                    continue;
                                }
                            }
                        };

                        if pruned_locators.is_some() && blob_metadata.blob_count() > 0 {
                            info!(
                                %height,
                                %proposal_round,
                                "Restreaming metadata-only payload (blobs pruned on sender; locators included for external consumers)"
                            );
                        }

                        // If we're restreaming into a *new* round, replicate the proposal and
                        // payload under the target round locally so commit() can find it later.
                        if round != proposal_round {
                            let mut restreamed_value = proposal.clone();
                            restreamed_value.round = round;
                            restreamed_value.proposer = address;

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
                                    execution_requests.clone(),
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

                            if let Some(ref sidecars) = restream_blob_sidecars &&
                                let Err(e) = state
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
                            &execution_requests,
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

                        if let Err(e) = restream_archive_notices(state, channels, height).await {
                            error!(
                                %height,
                                %round,
                                error = %e,
                                "Failed to restream archive notices"
                            );
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
                if let Some(proposal_part) = part.content.as_data() &&
                    let Some(notice) = proposal_part.as_archive_notice()
                {
                    match state.handle_archive_notice(notice.clone()).await {
                        Ok(_) => {
                            debug!(
                                height = %notice.body.height,
                                index = %notice.body.blob_index,
                                "Processed ArchiveNotice"
                            );
                        }
                        Err(e) => {
                            error!("Error processing ArchiveNotice: {}", e);
                        }
                    }
                    if reply.send(None).is_err() {
                        error!("Failed to send reply for ArchiveNotice");
                    }
                    continue;
                }
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

                            // Safety gate: if EL head doesn't match CL decided head, vote nil.
                            if let Err(e) = ensure_el_matches_cl_head(
                                state,
                                &execution_layer,
                                "received_proposal_part",
                            )
                            .await
                            {
                                warn!(
                                    error = %e,
                                    "EL not aligned with CL head; refusing to vote on proposal"
                                );
                                if reply.send(None).is_err() {
                                    error!("Failed to send ReceivedProposalPart reply");
                                }
                                continue;
                            }
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
                // Uses load_execution_block which prefers DecidedValue (immune to
                // BUG-014 blob metadata corruption) over blob metadata.
                if let Some(prev_height) = height.decrement() &&
                    let Ok(Some(prev_block)) = load_execution_block(state, prev_height).await
                {
                    let needs_realignment = state
                        .latest_block
                        .map(|blk| blk.block_number != prev_block.block_number)
                        .unwrap_or(true);
                    if needs_realignment {
                        debug!(
                            "[DIAG] Realigning latest execution block to height {} ({:?})",
                            prev_height, prev_block.block_hash
                        );
                        state.latest_block = Some(prev_block);
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

                let block_bytes = match state.get_block_data(height, round).await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        // Keep the application loop alive: instruct consensus to replay this
                        // height.
                        error!(
                            "[DIAG] ❌ Missing block bytes for decided value at height {} round {} on node {}; requesting restart",
                            height,
                            round,
                            state.validator_address()
                        );
                        let _ =
                            reply.send(Next::Restart(height, state.get_validator_set().clone()));
                        continue;
                    }
                    Err(e) => {
                        error!(
                            "[DIAG] ❌ Failed to load block bytes for decided value at height {} round {} on node {}: {}; requesting restart",
                            height,
                            round,
                            state.validator_address(),
                            e
                        );
                        let _ =
                            reply.send(Next::Restart(height, state.get_validator_set().clone()));
                        continue;
                    }
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
                        height, round
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

                let outcome = match state
                    .process_decided_certificate(&certificate, block_bytes.clone(), &mut notifier)
                    .await
                {
                    Ok(outcome) => {
                        debug!(
                            "[DIAG] ✅ process_decided_certificate succeeded: {} txs, {} blobs, current_height now={}",
                            outcome.tx_count, outcome.blob_count, state.current_height
                        );
                        if outcome.execution_pending {
                            warn!(
                                height = %height,
                                "Execution layer still syncing; execution finalization deferred"
                            );
                        }
                        outcome
                    }
                    Err(e) => {
                        error!(
                            "[DIAG] ❌ process_decided_certificate failed for height {}: {}; requesting restart",
                            height, e
                        );
                        let _ =
                            reply.send(Next::Restart(height, state.get_validator_set().clone()));
                        continue;
                    }
                };

                debug!(
                    height = %height,
                    txs = outcome.tx_count,
                    blobs = outcome.blob_count,
                    block_hash = ?outcome.execution_block.block_hash,
                    "✅ Decided certificate processed successfully"
                );

                // Phase 6: Handle archiving - either via worker or sync notices
                if let Some(job) = outcome.archive_job.clone() {
                    if let Some(job_tx) = &archiver_job_tx {
                        let job_height = job.height;
                        let blob_count = job.blob_indices.len();
                        if let Err(e) = job_tx.send(job) {
                            error!(
                                height = %job_height,
                                blob_count = blob_count,
                                error = %e,
                                "Failed to submit archive job; requesting consensus restart"
                            );
                            let _ = reply
                                .send(Next::Restart(height, state.get_validator_set().clone()));
                            continue;
                        }

                        info!(
                            height = %job_height,
                            blob_count = blob_count,
                            "📦 Submitted archive job to dispatcher"
                        );
                    } else {
                        warn!(
                            height = %height,
                            "Archiver worker disabled but node produced blobs; \
                             these blobs will remain pending until the worker is configured"
                        );
                    }
                }

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

                match state.process_synced_package(height, round, proposer, package).await {
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

                let decided_value = match state.get_decided_value(height).await {
                    Ok(value) => value,
                    Err(e) => {
                        error!(%height, "Failed to load decided value for GetDecidedValue: {}", e);
                        let _ = reply.send(None);
                        continue;
                    }
                };

                let raw_decided_value = match decided_value {
                    Some(decided_value) => {
                        // Get round from certificate
                        let round = decided_value.certificate.round;

                        // Attempt to retrieve execution payload bytes and execution requests
                        let payload_bytes = match state.get_block_data(height, round).await {
                            Ok(payload_bytes) => payload_bytes,
                            Err(e) => {
                                error!(
                                    %height,
                                    %round,
                                    "Failed to load block data for GetDecidedValue: {}",
                                    e
                                );
                                let _ = reply.send(None);
                                continue;
                            }
                        };
                        let execution_requests =
                            match state.get_execution_requests(height, round).await {
                                Ok(maybe) => maybe.unwrap_or_default(),
                                Err(e) => {
                                    error!(
                                        %height,
                                        %round,
                                        "Failed to load execution requests for GetDecidedValue: {}",
                                        e
                                    );
                                    let _ = reply.send(None);
                                    continue;
                                }
                            };

                        // Attempt to retrieve blob sidecars (with pruned status check)
                        let blob_sidecars_result = state.get_blobs_with_status_check(height).await;
                        let archive_notices = match state.load_archive_notices(height).await {
                            Ok(notices) => notices,
                            Err(e) => {
                                error!(
                                    %height,
                                    "Failed to load archive notices for GetDecidedValue: {}",
                                    e
                                );
                                let _ = reply.send(None);
                                continue;
                            }
                        };

                        // Build the sync package
                        let package = match (payload_bytes, &blob_sidecars_result) {
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
                                    blob_sidecars: blobs.clone(),
                                    execution_requests: execution_requests.clone(),
                                    archive_notices: archive_notices.clone(),
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
                                    execution_requests: execution_requests.clone(),
                                    archive_notices: archive_notices.clone(),
                                }
                            }
                            (
                                Some(payload),
                                Err(ultramarine_blob_engine::BlobEngineError::BlobsPruned {
                                    locators,
                                    ..
                                }),
                            ) => {
                                // Blobs have been pruned but payload is available - send
                                // MetadataOnly WITH the payload so
                                // the receiving peer can import the block without
                                // blob sidecars.
                                //
                                // Load Network pruning policy: Archive event is the boundary
                                // for blob pruning (NOT Ethereum's time-based DA window).
                                // This follows the Lighthouse pattern where blocks can be
                                // imported without blob sidecars once archived.
                                //
                                // The receiving peer will:
                                // 1. Import the execution payload to EL
                                // 2. Store consensus metadata (commitments from Value)
                                // 3. Mark blobs as pruned using archive notices
                                info!(
                                    %height,
                                    %round,
                                    payload_size = payload.len(),
                                    locator_count = locators.len(),
                                    notice_count = archive_notices.len(),
                                    "Blobs pruned, sending MetadataOnly WITH payload for import"
                                );
                                if !locators.is_empty() {
                                    debug!(
                                        %height,
                                        %round,
                                        first_locator = %locators[0],
                                        "Blob archived; operators should serve via external locator"
                                    );
                                }
                                SyncedValuePackage::MetadataOnly {
                                    value: decided_value.value.clone(),
                                    archive_notices: archive_notices.clone(),
                                    execution_payload_ssz: Some(payload),
                                    execution_requests: execution_requests.clone(),
                                }
                            }
                            _ => {
                                // Payload missing or other error - send MetadataOnly without
                                // payload The receiving peer cannot
                                // import this block, will try another peer
                                error!(
                                    %height,
                                    %round,
                                    "Payload or blobs missing/error, sending MetadataOnly without payload"
                                );
                                SyncedValuePackage::MetadataOnly {
                                    value: decided_value.value.clone(),
                                    archive_notices: archive_notices.clone(),
                                    execution_payload_ssz: None,
                                    execution_requests: vec![],
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

    // Graceful shutdown cleanup
    info!("Performing graceful shutdown cleanup...");

    // Flush blob engine to ensure all data is persisted
    if let Err(e) = state.blob_engine().flush_sync() {
        warn!("Failed to flush blob engine during shutdown: {}", e);
    } else {
        info!("Blob engine flushed successfully");
    }

    info!("Graceful shutdown complete");
    Ok(())
}

async fn restream_archive_notices(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    height: Height,
) -> eyre::Result<()> {
    let notices = state.load_archive_notices(height).await?;
    if notices.is_empty() {
        return Ok(());
    }
    for notice in notices {
        let stream_message = state.stream_archive_notice(notice);
        channels.network.send(NetworkMsg::PublishProposalPart(stream_message)).await?;
    }
    Ok(())
}

async fn handle_get_value(
    state: &mut State,
    channels: &mut Channels<LoadContext>,
    execution_layer: &ExecutionClient,
    height: Height,
    round: Round,
    timeout: Duration,
    reply: oneshot::Sender<LocallyProposedValue<LoadContext>>,
) -> eyre::Result<()> {
    info!(%height, %round, "🟢🟢 Consensus is requesting a value to propose");
    let started_at = Instant::now();

    // BUG-014 fix: During WAL recovery, the consensus engine replays the
    // proposal from the WAL but still fires GetValue because the app returned
    // empty reply_value in StartedRound.  If we build a NEW block here, it gets
    // a different timestamp → different block_hash → corrupts blob metadata →
    // permanent parent-hash-mismatch stall.
    //
    // Guard: if block data already exists for this (height, round), another code
    // path (WAL replay or sync) already stored the correct payload.  Skip the EL
    // build entirely.  Malachite already has the value from WAL and does not wait
    // for our reply.
    if let Ok(Some(_)) = state.get_block_data(height, round).await {
        warn!(
            %height, %round,
            "Block data already exists for this height/round (WAL recovery); skipping EL build"
        );
        // Don't send reply — Malachite already has the value from WAL replay
        // and will proceed to decision without waiting for this response.
        return Ok(());
    }

    if state.is_el_degraded() {
        warn!(
            %height,
            %round,
            "Execution layer marked degraded; attempting to realign before proposing"
        );
    }

    let latest_block = ensure_el_matches_cl_head(state, execution_layer, "get_value").await?;

    // Slot throttle: wait until we can propose with valid timestamp
    // IMPORTANT: Use Duration with subsecond precision to avoid ~1s jitter
    use ultramarine_types::constants::LOAD_MIN_BLOCK_TIME_SECS;

    let next_allowed_ts = latest_block.timestamp + LOAD_MIN_BLOCK_TIME_SECS;
    let target_time = Duration::from_secs(next_allowed_ts);

    let now_duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO); // Fallback to 0 on time error - will wait full slot

    if now_duration < target_time {
        // Precise wait with subseconds: e.g., if now is 1000.8s and target is 1001s, wait only 0.2s
        let wait = target_time.saturating_sub(now_duration);
        let remaining = timeout.saturating_sub(started_at.elapsed());
        let max_wait = remaining.saturating_sub(Duration::from_millis(200)); // margin for EL call

        if wait > max_wait {
            return Err(eyre!(
                "Slot throttle {:?} exceeds remaining GetValue timeout {:?}; refusing to propose",
                wait,
                remaining
            ));
        }

        debug!(%height, %round, ?wait, "Throttling: waiting for slot boundary");
        tokio::time::sleep(wait).await;
    }

    debug!("Requesting EL to build payload with blobs on top of head");

    let (execution_payload, blobs_bundle) =
        execution_layer.generate_block_with_blobs(&latest_block).await?;

    let execution_requests = execution_payload.execution_requests.clone();

    let blob_count = blobs_bundle.as_ref().map(|b| b.len()).unwrap_or(0);
    debug!("🌈 Got execution payload with {} blobs", blob_count);

    let bytes = Bytes::from(execution_payload.payload.as_ssz_bytes());
    debug!("🎁 block size: {:?}, height: {}, blobs: {}", bytes.len(), height, blob_count);

    let proposal = state
        .propose_value_with_blobs(
            height,
            round,
            bytes.clone(),
            &execution_payload.payload,
            &execution_requests,
            blobs_bundle.as_ref(),
        )
        .await?;
    state
        .store_undecided_proposal_data(height, round, bytes.clone(), execution_requests.clone())
        .await?;

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
        .await
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
    for stream_message in
        state.stream_proposal(proposal, bytes, stream_sidecars, &execution_requests, None)
    {
        let part_desc = match &stream_message.content {
            StreamContent::Fin => "Fin".to_string(),
            StreamContent::Data(part) => match part {
                ProposalPart::Init(_) => "Init".to_string(),
                ProposalPart::Data(data) => format!("Data(len={} bytes)", data.size_bytes()),
                ProposalPart::BlobSidecar(sidecar) => {
                    format!(
                        "BlobSidecar(index={}, bytes={})",
                        sidecar.index,
                        sidecar.blob.data().len()
                    )
                }
                ProposalPart::ArchiveNotice(notice) => format!(
                    "ArchiveNotice(height={}, index={})",
                    notice.body.height, notice.body.blob_index
                ),
                ProposalPart::Fin(_) => "Fin(part)".to_string(),
            },
        };
        info!(
            %height,
            %round,
            sequence = stream_message.sequence,
            part = %part_desc,
            "Streaming proposal part"
        );
        channels
            .network
            .send(NetworkMsg::PublishProposalPart(stream_message))
            .await
            .map_err(|e| eyre!("Failed to stream proposal part: {}", e))?;
    }

    debug!(%height, %round, "✅ Proposal sent");
    Ok(())
}

/// Loads the [`ExecutionBlock`] for a given decided height.
///
/// Prefers the consensus-authoritative [`DecidedValue`] (immune to BUG-014
/// blob-metadata corruption) over blob metadata.  Falls back to blob metadata
/// only when the decided value is unavailable (e.g. pruned).
async fn load_execution_block(
    state: &State,
    height: Height,
) -> eyre::Result<Option<ExecutionBlock>> {
    // Primary: DecidedValue — stored from the undecided *proposal* which uses
    // an is_none() guard and therefore cannot be overwritten during WAL replay.
    if let Ok(Some(decided_value)) = state.get_decided_value(height).await {
        let header = &decided_value.value.metadata.execution_payload_header;
        return Ok(Some(ExecutionBlock {
            block_hash: header.block_hash,
            block_number: header.block_number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
            prev_randao: load_prev_randao(),
        }));
    }

    // Fallback: blob metadata (may be corrupted by BUG-014 but useful when
    // decided values have been pruned).
    match state.get_blob_metadata(height).await? {
        Some(meta) => {
            let header = meta.execution_payload_header();
            Ok(Some(ExecutionBlock {
                block_hash: header.block_hash,
                block_number: header.block_number,
                parent_hash: header.parent_hash,
                timestamp: header.timestamp,
                prev_randao: load_prev_randao(),
            }))
        }
        None => Ok(None),
    }
}

async fn load_decided_block(state: &State) -> eyre::Result<Option<ExecutionBlock>> {
    let Some(height) = state.get_latest_decided_height().await else {
        return Ok(None);
    };
    let block = load_execution_block(state, height).await?;
    if block.is_none() {
        warn!(
            %height,
            "Missing both DecidedValue and BlobMetadata for latest decided height; cannot derive CL head"
        );
    }
    Ok(block)
}

async fn resolve_cl_head(state: &mut State) -> eyre::Result<ExecutionBlock> {
    if let Some(block) = load_decided_block(state).await? {
        state.latest_block = Some(block);
        return Ok(block);
    }
    state.latest_block.ok_or_else(|| eyre!("latest_block is None; CL head unavailable"))
}

async fn ensure_el_matches_cl_head(
    state: &mut State,
    execution_layer: &ExecutionClient,
    context: &str,
) -> eyre::Result<ExecutionBlock> {
    let cl_head = resolve_cl_head(state).await?;

    if state.is_el_degraded() &&
        let Some(since) = state.el_degraded_since
    {
        const EL_GATE_COOLDOWN: Duration = Duration::from_millis(500);
        if since.elapsed() < EL_GATE_COOLDOWN {
            return Err(eyre!("EL degraded; skipping FCU during cooldown (context={})", context));
        }
    }

    if let (Some(last_head), Some(last_success)) = (state.last_fcu_head, state.last_fcu_success) {
        const EL_FCU_SUCCESS_CACHE: Duration = Duration::from_millis(500);
        if last_head == cl_head.block_hash && last_success.elapsed() < EL_FCU_SUCCESS_CACHE {
            return Ok(cl_head);
        }
    }

    let mut notifier = execution_layer.as_notifier();
    match notifier.set_latest_forkchoice_state(cl_head.block_hash).await {
        Ok(_) => {
            state.clear_el_degraded();
            state.last_fcu_head = Some(cl_head.block_hash);
            state.last_fcu_success = Some(tokio::time::Instant::now());
            Ok(cl_head)
        }
        Err(e) => {
            let is_syncing = matches!(
                e.downcast_ref::<ExecutionError>(),
                Some(ExecutionError::SyncingForkchoice { .. })
            );
            let reason = if is_syncing {
                format!("EL syncing for head {}", cl_head.block_hash)
            } else {
                format!("EL rejected forkchoice for head {}: {}", cl_head.block_hash, e)
            };
            state.mark_el_degraded(reason);
            Err(e)
        }
    }
}
