use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use zinder_core::{
    BlockBlobArtifact, BlockFinalNoteCommitmentRoots, BlockHash, BlockHeaderArtifact, BlockHeight,
    BlockHeightRange, BlockId, BlockTransactionIndexArtifact, BlockValuePoolBalances,
    CanonicalBlockFacts, CanonicalTransactionFacts, ChainEpoch, CompactBlockArtifact,
    SerializedBytesDigest, SubtreeRootArtifact, TransactionBlobArtifact, TransactionFactsArtifact,
    TransactionId, TransactionIntrinsicValueBalancesArtifact, TransactionLocation,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, TreeStateArtifact,
    decode_canonical_block_replay,
};

use crate::{
    ChainEpochArtifacts, RawBlobRetention, ReorgWindowChange, StoreError,
    block_artifact::read_block_header_artifact, kv::RocksChainStore,
};

use super::ChainStoreOptions;

pub(super) fn validate_chain_store_options(options: ChainStoreOptions) -> Result<(), StoreError> {
    if options.reorg_window_blocks == 0 {
        return Err(StoreError::InvalidChainStoreOptions {
            reason: "reorg window blocks must be greater than zero",
        });
    }
    if options.retention_sweep_max_heights_per_pass == 0 {
        return Err(StoreError::InvalidChainStoreOptions {
            reason: "retention sweep max heights per pass must be greater than zero",
        });
    }
    if options.retention_sweep_max_outpoints_per_pass == 0 {
        return Err(StoreError::InvalidChainStoreOptions {
            reason: "retention sweep max outpoints per pass must be greater than zero",
        });
    }
    options
        .rocksdb_resource_budget
        .validate()
        .map_err(|reason| StoreError::InvalidChainStoreOptions { reason })?;

    Ok(())
}

pub(super) struct ValidatedBlockReplayOrder {
    pub(super) block_heights: Vec<BlockHeight>,
}

pub(super) fn validate_chain_epoch_artifacts(
    artifacts: &ChainEpochArtifacts,
) -> Result<ValidatedBlockReplayOrder, StoreError> {
    if artifacts.chain_epoch.id.value() == 0 {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "chain epoch id must be greater than zero",
        });
    }

    validate_artifact_presence(artifacts)?;

    let tip_height = artifacts.chain_epoch.visible_tip_height;
    validate_settled_tip_height(artifacts.chain_epoch)?;
    let block_hash_by_height = block_hash_by_height(&artifacts.block_headers)?;
    validate_committed_boundary_hash_if_present(
        artifacts.chain_epoch.visible_tip_height,
        artifacts.chain_epoch.visible_tip_hash,
        &block_hash_by_height,
        "tip hash must match the committed block at tip height",
    )?;
    validate_committed_boundary_hash_if_present(
        artifacts.chain_epoch.settled_tip_height,
        artifacts.chain_epoch.settled_tip_hash,
        &block_hash_by_height,
        "safe_tip_hash must match the committed block at safe_tip_height",
    )?;
    validate_block_header_artifacts(&artifacts.block_headers, tip_height)?;
    let validated_block_replay_order =
        validate_block_replay_envelopes(artifacts, &block_hash_by_height)?;
    validate_compact_block_artifacts(&artifacts.compact_blocks, tip_height, &block_hash_by_height)?;
    validate_transaction_facts_artifacts(
        &artifacts.transaction_facts,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_transaction_intrinsic_value_balances(
        &artifacts.transaction_intrinsic_value_balances,
        &artifacts.transaction_facts,
    )?;
    validate_tree_state_artifacts(&artifacts.tree_states, tip_height, &block_hash_by_height)?;
    validate_final_note_commitment_roots(
        &artifacts.final_note_commitment_roots,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_block_value_pool_balances(
        &artifacts.block_value_pool_balances,
        tip_height,
        &artifacts.block_headers,
    )?;
    validate_subtree_root_artifacts(&artifacts.subtree_roots, tip_height, &block_hash_by_height)?;
    validate_transparent_output_artifacts(
        &artifacts.transparent_outputs_by_outpoint,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_transparent_spend_facts(
        &artifacts.transparent_spend_facts,
        tip_height,
        &block_hash_by_height,
    )?;

    Ok(validated_block_replay_order)
}

pub(super) fn validate_retained_blob_artifacts(
    artifacts: &ChainEpochArtifacts,
    retention: RawBlobRetention,
) -> Result<(), StoreError> {
    let block_blob_count_matches = match retention {
        RawBlobRetention::None | RawBlobRetention::Transactions => artifacts.block_blobs.is_empty(),
        RawBlobRetention::All => artifacts.block_blobs.len() == artifacts.block_headers.len(),
    };
    if !block_blob_count_matches {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block blob artifacts must match the configured raw blob retention",
        });
    }

    let transaction_blob_count_matches = match retention {
        RawBlobRetention::None => artifacts.transaction_blobs.is_empty(),
        RawBlobRetention::Transactions | RawBlobRetention::All => {
            artifacts.transaction_blobs.len() == artifacts.transaction_facts.len()
        }
    };
    if !transaction_blob_count_matches {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction blob artifacts must match the configured raw blob retention",
        });
    }

    Ok(())
}

pub(super) fn committed_block_range(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Result<BlockHeightRange, StoreError> {
    if let Some(changed_block_range) = changed_block_range(artifacts, current_chain_epoch) {
        return Ok(changed_block_range);
    }

    if safe_tip_only_commit_without_artifacts(artifacts) {
        return Ok(BlockHeightRange::empty_at(
            artifacts.chain_epoch.settled_tip_height,
        ));
    }

    block_height_range(
        artifacts
            .block_headers
            .iter()
            .map(|artifact| artifact.height),
    )
}

fn validate_artifact_presence(artifacts: &ChainEpochArtifacts) -> Result<(), StoreError> {
    if safe_tip_only_commit_without_artifacts(artifacts) {
        return Ok(());
    }

    if artifacts.block_headers.is_empty() {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "at least one safe-tip block artifact is required",
        });
    }

    if artifacts.compact_blocks.is_empty() {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "at least one compact block artifact is required",
        });
    }

    Ok(())
}

fn safe_tip_only_commit_without_artifacts(artifacts: &ChainEpochArtifacts) -> bool {
    matches!(
        artifacts.reorg_window_change,
        ReorgWindowChange::AdvanceSafeTipTo { .. }
    ) && artifacts.block_headers.is_empty()
        && artifacts.block_replay_envelopes.is_empty()
        && artifacts.compact_blocks.is_empty()
        && artifacts.transaction_facts.is_empty()
        && artifacts.transaction_intrinsic_value_balances.is_empty()
        && artifacts.tree_states.is_empty()
        && artifacts.final_note_commitment_roots.is_empty()
        && artifacts.block_value_pool_balances.is_empty()
        && artifacts.subtree_roots.is_empty()
        && artifacts.transparent_outputs_by_outpoint.is_empty()
        && artifacts.transparent_spend_facts.is_empty()
}

pub(super) fn validate_reorg_window_change(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
    options: ChainStoreOptions,
) -> Result<(), StoreError> {
    validate_chain_epoch_range_coverage(artifacts, current_chain_epoch)?;
    validate_non_reorg_chain_epoch_progression(artifacts, current_chain_epoch)?;

    match artifacts.reorg_window_change {
        ReorgWindowChange::Replace { from_height } => {
            let current_chain_epoch =
                current_chain_epoch.ok_or(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement requires an existing chain epoch",
                })?;
            let minimum_reorg_height =
                minimum_reorg_height(current_chain_epoch, options.reorg_window_blocks);

            if from_height < minimum_reorg_height {
                return Err(StoreError::ReorgWindowExceeded {
                    attempted_from_height: from_height,
                    minimum_reorg_height,
                    safe_tip_height: current_chain_epoch.settled_tip_height,
                });
            }

            if from_height > artifacts.chain_epoch.visible_tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement start height cannot exceed tip height",
                });
            }

            if from_height > current_chain_epoch.visible_tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement start height cannot exceed current tip height",
                });
            }

            validate_replacement_preserves_safe_tip_anchor(artifacts, current_chain_epoch)?;
            validate_replacement_artifact_coverage(artifacts, from_height)
        }
        ReorgWindowChange::AdvanceSafeTipTo { height } => {
            if height > artifacts.chain_epoch.settled_tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "AdvanceSafeTipTo height cannot exceed epoch safe_tip_height",
                });
            }

            Ok(())
        }
        ReorgWindowChange::Extend { block_range } => {
            if block_range.end > artifacts.chain_epoch.visible_tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "reorg-window extension cannot exceed tip height",
                });
            }

            Ok(())
        }
        ReorgWindowChange::Unchanged => Ok(()),
    }
}

pub(super) fn validate_visible_chain_commit(
    inner: &RocksChainStore,
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Result<(), StoreError> {
    let changed_block_range = changed_block_range(artifacts, current_chain_epoch);

    validate_committed_block_heights_are_publishable(artifacts, changed_block_range)?;
    validate_committed_block_parent_links(
        inner,
        artifacts,
        current_chain_epoch,
        changed_block_range,
    )?;
    validate_safe_tip_hash_against_visible_chain(inner, artifacts, current_chain_epoch)
}

pub(super) fn block_height_range(
    heights: impl Iterator<Item = BlockHeight>,
) -> Result<BlockHeightRange, StoreError> {
    let mut min_height = None;
    let mut max_height = None;

    for height in heights {
        min_height = Some(min_height.map_or(height, |current: BlockHeight| current.min(height)));
        max_height = Some(max_height.map_or(height, |current: BlockHeight| current.max(height)));
    }

    let min_height = min_height.ok_or(StoreError::InvalidChainEpochArtifacts {
        reason: "at least one block height is required",
    })?;
    let max_height = max_height.ok_or(StoreError::InvalidChainEpochArtifacts {
        reason: "at least one block height is required",
    })?;

    Ok(BlockHeightRange::inclusive(min_height, max_height))
}

fn changed_block_range(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Option<BlockHeightRange> {
    // Bootstrap commit (empty store seeded by an operator-supplied
    // checkpoint) publishes no block artifacts, so it has no changed range.
    // See `validate_chain_epoch_range_coverage` for the full contract.
    if current_chain_epoch.is_none() && safe_tip_only_commit_without_artifacts(artifacts) {
        return None;
    }

    match artifacts.reorg_window_change {
        ReorgWindowChange::Replace { from_height } => Some(BlockHeightRange::inclusive(
            from_height,
            artifacts.chain_epoch.visible_tip_height,
        )),
        ReorgWindowChange::Extend { .. }
        | ReorgWindowChange::AdvanceSafeTipTo { .. }
        | ReorgWindowChange::Unchanged => match current_chain_epoch {
            Some(current_chain_epoch)
                if artifacts.chain_epoch.visible_tip_height
                    > current_chain_epoch.visible_tip_height =>
            {
                let next_height = current_chain_epoch
                    .visible_tip_height
                    .value()
                    .saturating_add(1);
                Some(BlockHeightRange::inclusive(
                    BlockHeight::new(next_height),
                    artifacts.chain_epoch.visible_tip_height,
                ))
            }
            Some(_) => None,
            None => Some(BlockHeightRange::inclusive(
                first_committed_block_height(artifacts),
                artifacts.chain_epoch.visible_tip_height,
            )),
        },
    }
}

fn validate_committed_block_heights_are_publishable(
    artifacts: &ChainEpochArtifacts,
    changed_block_range: Option<BlockHeightRange>,
) -> Result<(), StoreError> {
    let Some(changed_block_range) = changed_block_range else {
        if artifacts.block_headers.is_empty() && artifacts.compact_blocks.is_empty() {
            return Ok(());
        }

        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "commit without a changed block range cannot publish block artifacts",
        });
    };

    for block in &artifacts.block_headers {
        if block.height < changed_block_range.start || block.height > changed_block_range.end {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block artifacts can only publish newly appended or replaced heights",
            });
        }
    }

    Ok(())
}

fn validate_committed_block_parent_links(
    inner: &RocksChainStore,
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
    changed_block_range: Option<BlockHeightRange>,
) -> Result<(), StoreError> {
    let Some(changed_block_range) = changed_block_range else {
        return Ok(());
    };

    let block_headers_by_height = block_header_by_height(&artifacts.block_headers)?;
    let mut expected_parent_hash = match current_chain_epoch {
        None => None,
        Some(current_chain_epoch)
            if changed_block_range.start.value().saturating_sub(1)
                == current_chain_epoch.visible_tip_height.value() =>
        {
            // First appended block links directly to the visible chain tip
            // recorded on the chain epoch. This path is required when the
            // store was bootstrapped from a checkpoint and has no stored
            // block at the checkpoint height.
            Some(current_chain_epoch.visible_tip_hash)
        }
        Some(current_chain_epoch) => Some(visible_block_hash_at(
            inner,
            Some(current_chain_epoch),
            BlockHeight::new(changed_block_range.start.value().saturating_sub(1)),
        )?),
    };

    for height in changed_block_range {
        let block = block_headers_by_height.get(&height).ok_or({
            StoreError::InvalidChainEpochArtifacts {
                reason: "committed block range must contain every linked block height",
            }
        })?;

        if let Some(expected_parent_hash) = expected_parent_hash
            && block.parent_hash != expected_parent_hash
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block artifact parent hash must link to the previous visible block",
            });
        }

        expected_parent_hash = Some(block.block_hash);
    }

    Ok(())
}

fn validate_safe_tip_hash_against_visible_chain(
    inner: &RocksChainStore,
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Result<(), StoreError> {
    let safe_tip_height = artifacts.chain_epoch.settled_tip_height;
    if safe_tip_height.value() == 0 {
        return Ok(());
    }

    // Bootstrap commit: the operator-supplied safe_tip_hash is the
    // checkpoint's anchor of trust; there is no prior chain to validate
    // against.
    if current_chain_epoch.is_none() && safe_tip_only_commit_without_artifacts(artifacts) {
        return Ok(());
    }

    let committed_hash_by_height = block_hash_by_height(&artifacts.block_headers)?;
    if let Some(committed_hash) = committed_hash_by_height.get(&safe_tip_height) {
        if *committed_hash != artifacts.chain_epoch.settled_tip_hash {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "safe_tip_hash must match the committed block at safe_tip_height",
            });
        }

        return Ok(());
    }

    if let Some(current_chain_epoch) = current_chain_epoch
        && safe_tip_height <= current_chain_epoch.visible_tip_height
    {
        let safe_tip_hash =
            visible_block_hash_at(inner, Some(current_chain_epoch), safe_tip_height)?;
        if safe_tip_hash == artifacts.chain_epoch.settled_tip_hash {
            return Ok(());
        }
    }

    Err(StoreError::InvalidChainEpochArtifacts {
        reason: "safe_tip_hash must match the visible block at safe_tip_height",
    })
}

fn visible_block_hash_at(
    inner: &RocksChainStore,
    current_chain_epoch: Option<ChainEpoch>,
    height: BlockHeight,
) -> Result<BlockHash, StoreError> {
    let current_chain_epoch =
        current_chain_epoch.ok_or(StoreError::InvalidChainEpochArtifacts {
            reason: "visible block validation requires an existing chain epoch",
        })?;
    let Some(block) = read_block_header_artifact(inner, current_chain_epoch, height)? else {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "visible block validation height is not present in the current chain",
        });
    };

    Ok(block.block_hash)
}

fn validate_chain_epoch_range_coverage(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Result<(), StoreError> {
    // Bootstrap commit: an empty store seeded by an operator-supplied
    // checkpoint. Validation cannot demand block coverage because the
    // operator deliberately did not replay the chain prefix; reads at
    // heights below the checkpoint return `ArtifactUnavailable`.
    if current_chain_epoch.is_none() && safe_tip_only_commit_without_artifacts(artifacts) {
        return Ok(());
    }

    match artifacts.reorg_window_change {
        ReorgWindowChange::Replace { .. } => Ok(()),
        ReorgWindowChange::Extend { .. }
        | ReorgWindowChange::AdvanceSafeTipTo { .. }
        | ReorgWindowChange::Unchanged => {
            let required_range = match current_chain_epoch {
                Some(current_chain_epoch)
                    if artifacts.chain_epoch.visible_tip_height
                        > current_chain_epoch.visible_tip_height =>
                {
                    let next_height = current_chain_epoch
                        .visible_tip_height
                        .value()
                        .saturating_add(1);
                    Some(BlockHeightRange::inclusive(
                        BlockHeight::new(next_height),
                        artifacts.chain_epoch.visible_tip_height,
                    ))
                }
                Some(_) => None,
                None => Some(BlockHeightRange::inclusive(
                    first_committed_block_height(artifacts),
                    artifacts.chain_epoch.visible_tip_height,
                )),
            };

            if let Some(required_range) = required_range {
                validate_required_block_coverage(
                    artifacts,
                    required_range,
                    "commits that advance the tip must include every new block and compact block",
                )?;
            }

            Ok(())
        }
    }
}

fn validate_non_reorg_chain_epoch_progression(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: Option<ChainEpoch>,
) -> Result<(), StoreError> {
    if matches!(
        artifacts.reorg_window_change,
        ReorgWindowChange::Replace { .. }
    ) {
        return Ok(());
    }

    let Some(current_chain_epoch) = current_chain_epoch else {
        return Ok(());
    };

    let chain_epoch = artifacts.chain_epoch;
    if chain_epoch.visible_tip_height < current_chain_epoch.visible_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot lower tip height",
        });
    }

    if chain_epoch.settled_tip_height < current_chain_epoch.settled_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot lower safe_tip_height",
        });
    }

    if chain_epoch.visible_tip_height == current_chain_epoch.visible_tip_height
        && chain_epoch.visible_tip_hash != current_chain_epoch.visible_tip_hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot change the tip hash at the current tip height",
        });
    }

    if chain_epoch.settled_tip_height == current_chain_epoch.settled_tip_height
        && chain_epoch.settled_tip_hash != current_chain_epoch.settled_tip_hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot change safe_tip_hash at the current safe_tip_height",
        });
    }

    Ok(())
}

fn minimum_reorg_height(chain_epoch: ChainEpoch, reorg_window_blocks: u32) -> BlockHeight {
    let safe_tip_floor = chain_epoch.settled_tip_height.value().saturating_add(1);
    let window_floor = chain_epoch
        .visible_tip_height
        .value()
        .saturating_sub(reorg_window_blocks.saturating_sub(1));

    BlockHeight::new(safe_tip_floor.max(window_floor))
}

fn validate_replacement_artifact_coverage(
    artifacts: &ChainEpochArtifacts,
    from_height: BlockHeight,
) -> Result<(), StoreError> {
    validate_required_block_coverage(
        artifacts,
        BlockHeightRange::inclusive(from_height, artifacts.chain_epoch.visible_tip_height),
        "replacement commits must include every replaced block and compact block",
    )
}

fn validate_replacement_preserves_safe_tip_anchor(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: ChainEpoch,
) -> Result<(), StoreError> {
    if artifacts.chain_epoch.settled_tip_height < current_chain_epoch.settled_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "replacement commit cannot lower safe_tip_height",
        });
    }

    if artifacts.chain_epoch.settled_tip_height == current_chain_epoch.settled_tip_height
        && artifacts.chain_epoch.settled_tip_hash != current_chain_epoch.settled_tip_hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "replacement commit cannot change the current safe_tip_hash",
        });
    }

    Ok(())
}

fn validate_required_block_coverage(
    artifacts: &ChainEpochArtifacts,
    required_range: BlockHeightRange,
    reason: &'static str,
) -> Result<(), StoreError> {
    let block_heights: HashSet<BlockHeight> = artifacts
        .block_headers
        .iter()
        .map(|block| block.height)
        .collect();
    let compact_block_heights: HashSet<BlockHeight> = artifacts
        .compact_blocks
        .iter()
        .map(|compact_block| compact_block.height)
        .collect();

    for height in required_range {
        if !block_heights.contains(&height) || !compact_block_heights.contains(&height) {
            return Err(StoreError::InvalidChainEpochArtifacts { reason });
        }
    }

    Ok(())
}

fn validate_settled_tip_height(chain_epoch: ChainEpoch) -> Result<(), StoreError> {
    if chain_epoch.settled_tip_height > chain_epoch.visible_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "safe_tip_height cannot exceed tip height",
        });
    }

    Ok(())
}

fn block_hash_by_height(
    block_headers: &[BlockHeaderArtifact],
) -> Result<HashMap<BlockHeight, BlockHash>, StoreError> {
    let mut block_hash_by_height = HashMap::new();
    for block in block_headers {
        if let Some(existing_hash) = block_hash_by_height.insert(block.height, block.block_hash)
            && existing_hash != block.block_hash
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block artifacts cannot contain conflicting hashes at the same height",
            });
        }
    }

    Ok(block_hash_by_height)
}

fn block_header_by_height(
    block_headers: &[BlockHeaderArtifact],
) -> Result<HashMap<BlockHeight, &BlockHeaderArtifact>, StoreError> {
    let mut block_header_by_height = HashMap::new();
    for block in block_headers {
        if let Some(existing_block) = block_header_by_height.insert(block.height, block)
            && (existing_block.block_hash != block.block_hash
                || existing_block.parent_hash != block.parent_hash)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block artifacts cannot contain conflicting metadata at the same height",
            });
        }
    }

    Ok(block_header_by_height)
}

fn validate_block_header_artifacts(
    block_headers: &[BlockHeaderArtifact],
    tip_height: BlockHeight,
) -> Result<(), StoreError> {
    for block in block_headers {
        if block.height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block artifact height cannot exceed tip height",
            });
        }
    }

    Ok(())
}

fn validate_block_replay_envelopes(
    artifacts: &ChainEpochArtifacts,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<ValidatedBlockReplayOrder, StoreError> {
    if artifacts.block_replay_envelopes.len() != artifacts.block_headers.len() {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "every committed block header must have exactly one block replay envelope",
        });
    }

    let artifact_index = BlockReplayArtifactIndex::new(artifacts)?;

    let mut decoded_block_ids = HashSet::new();
    let mut ordered_block_heights = Vec::with_capacity(artifacts.block_replay_envelopes.len());
    let mut decoded_transaction_ids = HashSet::new();
    let mut decoded_transparent_index = DecodedTransparentIndex::default();
    for replay_envelope in &artifacts.block_replay_envelopes {
        let replay = decode_canonical_block_replay(replay_envelope.as_bytes()).map_err(|_| {
            StoreError::InvalidChainEpochArtifacts {
                reason: "block replay envelope failed semantic validation",
            }
        })?;
        let decoded_facts = replay.facts();
        let block_id = validate_replay_block_header(
            decoded_facts,
            block_hash_by_height,
            &artifact_index.block_headers_by_id,
        )?;
        if !decoded_block_ids.insert(block_id) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block replay cannot repeat a block identity",
            });
        }
        validate_replay_transactions(
            decoded_facts,
            block_id,
            &artifact_index,
            &mut decoded_transaction_ids,
            &mut decoded_transparent_index,
        )?;
        validate_replay_block_blob(decoded_facts, block_id, &artifact_index)?;
        ordered_block_heights.push(block_id.height);
    }

    validate_replay_block_order(artifacts, &ordered_block_heights)?;
    validate_replay_artifact_membership(
        artifacts,
        &artifact_index,
        &decoded_block_ids,
        &decoded_transaction_ids,
    )?;
    validate_replay_transparent_artifacts(artifacts, &decoded_transparent_index)?;

    Ok(ValidatedBlockReplayOrder {
        block_heights: ordered_block_heights,
    })
}

fn validate_replay_block_order(
    artifacts: &ChainEpochArtifacts,
    ordered_replay_heights: &[BlockHeight],
) -> Result<(), StoreError> {
    if !artifacts
        .block_headers
        .iter()
        .map(|header| header.height)
        .eq(ordered_replay_heights.iter().copied())
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block replay must follow committed block header order",
        });
    }
    Ok(())
}

struct BlockReplayArtifactIndex<'a> {
    block_headers_by_id: HashMap<BlockId, &'a BlockHeaderArtifact>,
    transaction_index_by_block: HashMap<BlockId, Vec<&'a BlockTransactionIndexArtifact>>,
    transaction_facts_by_block: HashMap<BlockId, Vec<&'a TransactionFactsArtifact>>,
    transaction_locations_by_id: HashMap<TransactionId, &'a TransactionLocation>,
    intrinsic_balances_by_id: HashMap<TransactionId, &'a TransactionIntrinsicValueBalancesArtifact>,
    transaction_blobs_by_id: HashMap<TransactionId, &'a TransactionBlobArtifact>,
    block_blobs_by_id: HashMap<BlockId, &'a BlockBlobArtifact>,
}

impl<'a> BlockReplayArtifactIndex<'a> {
    fn new(artifacts: &'a ChainEpochArtifacts) -> Result<Self, StoreError> {
        Ok(Self {
            block_headers_by_id: index_unique_by_key(
                &artifacts.block_headers,
                |header| BlockId::new(header.height, header.block_hash),
                "block headers cannot repeat a block identity",
            )?,
            transaction_index_by_block: transaction_index_by_block(artifacts),
            transaction_facts_by_block: transaction_facts_by_block(artifacts),
            transaction_locations_by_id: index_unique_by_key(
                &artifacts.transaction_locations,
                |location| location.transaction_id,
                "transaction locations cannot repeat a transaction id",
            )?,
            intrinsic_balances_by_id: index_unique_by_key(
                &artifacts.transaction_intrinsic_value_balances,
                |balances| balances.location.transaction_id,
                "transaction intrinsic balances cannot repeat a transaction id",
            )?,
            transaction_blobs_by_id: index_unique_by_key(
                &artifacts.transaction_blobs,
                |blob| blob.location.transaction_id,
                "transaction blobs cannot repeat a transaction id",
            )?,
            block_blobs_by_id: index_unique_by_key(
                &artifacts.block_blobs,
                |blob| BlockId::new(blob.height, blob.block_hash),
                "block blobs cannot repeat a block identity",
            )?,
        })
    }
}

fn index_unique_by_key<'a, Key, Row>(
    rows: &'a [Row],
    key_for: impl Fn(&Row) -> Key,
    duplicate_reason: &'static str,
) -> Result<HashMap<Key, &'a Row>, StoreError>
where
    Key: Eq + Hash,
{
    let mut rows_by_key = HashMap::with_capacity(rows.len());
    for row in rows {
        if rows_by_key.insert(key_for(row), row).is_some() {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: duplicate_reason,
            });
        }
    }
    Ok(rows_by_key)
}

fn transaction_index_by_block(
    artifacts: &ChainEpochArtifacts,
) -> HashMap<BlockId, Vec<&BlockTransactionIndexArtifact>> {
    let mut rows_by_block = HashMap::<BlockId, Vec<_>>::new();
    for row in &artifacts.block_transaction_index {
        rows_by_block
            .entry(BlockId::new(row.block_height, row.block_hash))
            .or_default()
            .push(row);
    }
    for rows in rows_by_block.values_mut() {
        rows.sort_by_key(|row| row.tx_index_in_block);
    }
    rows_by_block
}

fn transaction_facts_by_block(
    artifacts: &ChainEpochArtifacts,
) -> HashMap<BlockId, Vec<&TransactionFactsArtifact>> {
    let mut transactions_by_block = HashMap::<BlockId, Vec<_>>::new();
    for transaction in &artifacts.transaction_facts {
        transactions_by_block
            .entry(BlockId::new(
                transaction.location.block_height,
                transaction.location.block_hash,
            ))
            .or_default()
            .push(transaction);
    }
    for transactions in transactions_by_block.values_mut() {
        transactions.sort_by_key(|transaction| transaction.location.tx_index_in_block);
    }
    transactions_by_block
}

fn validate_replay_block_header(
    decoded_facts: &CanonicalBlockFacts,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
    block_headers_by_id: &HashMap<BlockId, &BlockHeaderArtifact>,
) -> Result<BlockId, StoreError> {
    let replay_header = &decoded_facts.block_header;
    let block_id = BlockId::new(replay_header.height, replay_header.block_hash);
    if block_hash_by_height.get(&replay_header.height) != Some(&replay_header.block_hash)
        || block_headers_by_id.get(&block_id).copied() != Some(replay_header)
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block replay header must match the committed block header",
        });
    }
    Ok(block_id)
}

fn validate_replay_transactions(
    decoded_facts: &CanonicalBlockFacts,
    block_id: BlockId,
    artifact_index: &BlockReplayArtifactIndex<'_>,
    decoded_transaction_ids: &mut HashSet<TransactionId>,
    decoded_transparent_index: &mut DecodedTransparentIndex,
) -> Result<(), StoreError> {
    let transaction_index = artifact_index
        .transaction_index_by_block
        .get(&block_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let transaction_facts = artifact_index
        .transaction_facts_by_block
        .get(&block_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if transaction_index.len() != decoded_facts.transactions.len()
        || transaction_facts.len() != decoded_facts.transactions.len()
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block replay transaction count must match block index and transaction facts",
        });
    }

    for (position, replay_transaction) in decoded_facts.transactions.iter().enumerate() {
        let tx_index_in_block =
            u32::try_from(position).map_err(|_| StoreError::InvalidChainEpochArtifacts {
                reason: "block replay transaction count exceeds the supported index range",
            })?;
        validate_replay_transaction(
            ReplayTransactionRows {
                decoded_facts: replay_transaction,
                block_index: transaction_index[position],
                stored_facts: transaction_facts[position],
                index_in_block: tx_index_in_block,
            },
            artifact_index,
            decoded_transaction_ids,
            decoded_transparent_index,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ReplayTransactionRows<'a> {
    decoded_facts: &'a CanonicalTransactionFacts,
    block_index: &'a BlockTransactionIndexArtifact,
    stored_facts: &'a TransactionFactsArtifact,
    index_in_block: u32,
}

fn validate_replay_transaction(
    rows: ReplayTransactionRows<'_>,
    artifact_index: &BlockReplayArtifactIndex<'_>,
    decoded_transaction_ids: &mut HashSet<TransactionId>,
    decoded_transparent_index: &mut DecodedTransparentIndex,
) -> Result<(), StoreError> {
    let transaction_id = rows.decoded_facts.public_facts.transaction_id;
    if rows.block_index.tx_index_in_block != rows.index_in_block
        || rows.block_index.transaction_id != transaction_id
        || rows.stored_facts.location.tx_index_in_block != rows.index_in_block
        || rows.stored_facts.location.transaction_id != transaction_id
        || rows.stored_facts.public_facts != rows.decoded_facts.public_facts
        || rows.stored_facts.transparent_inputs != rows.decoded_facts.transparent_inputs
        || rows.stored_facts.transparent_outputs != rows.decoded_facts.transparent_outputs
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "ordered block replay transactions must match index and transaction facts",
        });
    }
    if !decoded_transaction_ids.insert(transaction_id) {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block replay cannot repeat a transaction id",
        });
    }

    let Some(location) = artifact_index
        .transaction_locations_by_id
        .get(&transaction_id)
    else {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "every block replay transaction must have a transaction location",
        });
    };
    if **location != rows.stored_facts.location {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction location must match block replay",
        });
    }
    let Some(balances) = artifact_index.intrinsic_balances_by_id.get(&transaction_id) else {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "every block replay transaction must have intrinsic balances",
        });
    };
    if balances.location != rows.stored_facts.location
        || balances.value_balances != rows.decoded_facts.intrinsic_value_balances
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction intrinsic balances must match block replay",
        });
    }
    if let Some(transaction_blob) = artifact_index.transaction_blobs_by_id.get(&transaction_id) {
        validate_replay_transaction_blob(rows, transaction_blob)?;
    }
    decoded_transparent_index.record_transaction(rows)?;
    Ok(())
}

fn validate_replay_transaction_blob(
    rows: ReplayTransactionRows<'_>,
    transaction_blob: &TransactionBlobArtifact,
) -> Result<(), StoreError> {
    if transaction_blob.location != rows.stored_facts.location {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction blob location must match block replay",
        });
    }
    if u32::try_from(transaction_blob.raw_transaction_bytes.len())
        != Ok(rows.decoded_facts.public_facts.size_bytes)
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction blob size must match block replay",
        });
    }
    if SerializedBytesDigest::from_serialized_bytes(&transaction_blob.raw_transaction_bytes)
        != rows.decoded_facts.serialized_bytes_digest
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transaction blob bytes must match block replay digest",
        });
    }
    Ok(())
}

#[derive(Default)]
struct DecodedTransparentIndex {
    outputs_by_outpoint: HashMap<TransparentOutPoint, TransparentOutputArtifact>,
    input_identities: HashSet<ReplayTransparentInputIdentity>,
}

impl DecodedTransparentIndex {
    fn record_transaction(&mut self, rows: ReplayTransactionRows<'_>) -> Result<(), StoreError> {
        let transaction_id = rows.decoded_facts.public_facts.transaction_id;
        let location = rows.stored_facts.location;
        for output in &rows.decoded_facts.transparent_outputs {
            let outpoint = TransparentOutPoint::new(transaction_id, output.output_index);
            let artifact = TransparentOutputArtifact::new(
                outpoint,
                output.value_zat,
                output.script_pub_key.clone(),
                output.address_script_hash,
                location.block_height,
                location.block_hash,
            );
            if self
                .outputs_by_outpoint
                .insert(outpoint, artifact)
                .is_some()
            {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "block replay cannot repeat a transparent output outpoint",
                });
            }
        }

        for input in &rows.decoded_facts.transparent_inputs {
            if input.spent_outpoint.is_coinbase_sentinel() {
                continue;
            }
            let identity = ReplayTransparentInputIdentity {
                block_id: BlockId::new(location.block_height, location.block_hash),
                transaction_id,
                tx_index_in_block: rows.index_in_block,
                input_index: input.input_index,
                spent_outpoint: input.spent_outpoint,
            };
            if !self.input_identities.insert(identity) {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "block replay cannot repeat a transparent input identity",
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ReplayTransparentInputIdentity {
    block_id: BlockId,
    transaction_id: TransactionId,
    tx_index_in_block: u32,
    input_index: u32,
    spent_outpoint: TransparentOutPoint,
}

fn validate_replay_transparent_artifacts(
    artifacts: &ChainEpochArtifacts,
    decoded_index: &DecodedTransparentIndex,
) -> Result<(), StoreError> {
    let transparent_outputs_match = artifacts.transparent_outputs_by_outpoint.len()
        == decoded_index.outputs_by_outpoint.len()
        && artifacts
            .transparent_outputs_by_outpoint
            .iter()
            .all(|artifact| {
                decoded_index.outputs_by_outpoint.get(&artifact.outpoint) == Some(artifact)
            });
    if !transparent_outputs_match {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transparent output artifacts must exactly match block replay",
        });
    }

    let every_spend_has_replay_input = artifacts.transparent_spend_facts.iter().all(|spend| {
        decoded_index
            .input_identities
            .contains(&ReplayTransparentInputIdentity {
                block_id: BlockId::new(spend.block_height, spend.block_hash),
                transaction_id: spend.spending_transaction_id,
                tx_index_in_block: spend.tx_index_in_block,
                input_index: spend.input_index,
                spent_outpoint: spend.spent_outpoint,
            })
    });
    if !every_spend_has_replay_input {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "every transparent spend fact must identify an input in block replay",
        });
    }
    Ok(())
}

fn validate_replay_block_blob(
    decoded_facts: &CanonicalBlockFacts,
    block_id: BlockId,
    artifact_index: &BlockReplayArtifactIndex<'_>,
) -> Result<(), StoreError> {
    let block_blob = artifact_index.block_blobs_by_id.get(&block_id);
    if let Some(block_blob) = block_blob {
        if block_blob.parent_hash != decoded_facts.block_header.parent_hash {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block blob identity must match block replay",
            });
        }
        if u64::try_from(block_blob.raw_block_bytes.len())
            != Ok(decoded_facts.block_header.block_size_bytes)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block blob size must match block replay",
            });
        }
        if SerializedBytesDigest::from_serialized_bytes(&block_blob.raw_block_bytes)
            != decoded_facts.serialized_bytes_digest
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block blob bytes must match block replay digest",
            });
        }
    }
    Ok(())
}

fn validate_replay_artifact_membership(
    artifacts: &ChainEpochArtifacts,
    artifact_index: &BlockReplayArtifactIndex<'_>,
    decoded_block_ids: &HashSet<BlockId>,
    decoded_transaction_ids: &HashSet<TransactionId>,
) -> Result<(), StoreError> {
    let contains_unknown_block =
        decoded_block_ids.len() != artifact_index.block_headers_by_id.len()
            || artifacts.block_blobs.iter().any(|blob| {
                !decoded_block_ids.contains(&BlockId::new(blob.height, blob.block_hash))
            })
            || artifacts.block_transaction_index.iter().any(|row| {
                !decoded_block_ids.contains(&BlockId::new(row.block_height, row.block_hash))
            });
    let contains_unknown_transaction = artifacts
        .transaction_facts
        .iter()
        .any(|transaction| !decoded_transaction_ids.contains(&transaction.location.transaction_id))
        || artifacts
            .transaction_locations
            .iter()
            .any(|location| !decoded_transaction_ids.contains(&location.transaction_id))
        || artifacts
            .transaction_blobs
            .iter()
            .any(|blob| !decoded_transaction_ids.contains(&blob.location.transaction_id))
        || artifacts
            .transaction_intrinsic_value_balances
            .iter()
            .any(|balances| !decoded_transaction_ids.contains(&balances.location.transaction_id));
    if contains_unknown_block || contains_unknown_transaction {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "committed artifacts must belong to the supplied block replay",
        });
    }
    Ok(())
}

fn validate_compact_block_artifacts(
    compact_blocks: &[CompactBlockArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    for compact_block in compact_blocks {
        if compact_block.height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "compact block artifact height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&compact_block.height) != Some(&compact_block.block_hash) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "compact block artifact must match a block artifact at the same height",
            });
        }
    }

    Ok(())
}

fn validate_transaction_facts_artifacts(
    transaction_facts: &[TransactionFactsArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    for transaction in transaction_facts {
        if transaction.location.block_height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transaction artifact height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&transaction.location.block_height)
            != Some(&transaction.location.block_hash)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transaction artifact must match a block artifact at the same height",
            });
        }
    }

    Ok(())
}

fn validate_transaction_intrinsic_value_balances(
    intrinsic_balances: &[zinder_core::TransactionIntrinsicValueBalancesArtifact],
    transaction_facts: &[TransactionFactsArtifact],
) -> Result<(), StoreError> {
    let locations_by_transaction_id = transaction_facts
        .iter()
        .map(|facts| (facts.location.transaction_id, facts.location))
        .collect::<HashMap<_, _>>();
    let mut transaction_ids = HashSet::new();
    for artifact in intrinsic_balances {
        if !transaction_ids.insert(artifact.location.transaction_id) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transaction intrinsic value balances cannot repeat a transaction id",
            });
        }
        if locations_by_transaction_id.get(&artifact.location.transaction_id)
            != Some(&artifact.location)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transaction intrinsic value balances must match transaction facts in the same commit",
            });
        }
    }

    Ok(())
}

fn validate_tree_state_artifacts(
    tree_states: &[TreeStateArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    for tree_state in tree_states {
        if tree_state.height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "tree-state artifact height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&tree_state.height) != Some(&tree_state.block_hash) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "tree-state artifact must match a block artifact at the same height",
            });
        }
    }

    Ok(())
}

fn validate_final_note_commitment_roots(
    roots_by_block: &[BlockFinalNoteCommitmentRoots],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut heights = HashSet::new();
    for roots in roots_by_block {
        if roots.height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "final note-commitment roots height cannot exceed tip height",
            });
        }
        if block_hash_by_height.get(&roots.height) != Some(&roots.block_hash) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "final note-commitment roots must match a block artifact at the same height",
            });
        }
        if !heights.insert(roots.height) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "final note-commitment roots cannot repeat a block height",
            });
        }
    }

    Ok(())
}

fn validate_block_value_pool_balances(
    balances_by_block: &[BlockValuePoolBalances],
    tip_height: BlockHeight,
    block_headers: &[BlockHeaderArtifact],
) -> Result<(), StoreError> {
    let block_headers_by_height = block_headers
        .iter()
        .map(|block| (block.height, block))
        .collect::<HashMap<_, _>>();
    let mut heights = HashSet::new();
    for balances in balances_by_block {
        if balances.block_id.height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances height cannot exceed tip height",
            });
        }
        let Some(block) = block_headers_by_height.get(&balances.block_id.height) else {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances must match a block artifact at the same height",
            });
        };
        if block.block_hash != balances.block_id.hash {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances hash must match the block artifact",
            });
        }
        if block.block_time != balances.block_time_seconds {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances time must match the block artifact",
            });
        }
        if !heights.insert(balances.block_id.height) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances cannot repeat a block height",
            });
        }
        validate_value_pool_entries(balances)?;
    }

    Ok(())
}

pub(super) fn validate_value_pool_entries(
    balances: &BlockValuePoolBalances,
) -> Result<(), StoreError> {
    if balances.pools.is_empty() {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "block value-pool balances must contain at least one pool",
        });
    }

    let mut pool_ids = HashSet::with_capacity(balances.pools.len());
    for pool in &balances.pools {
        if pool.id.is_empty() {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances cannot contain an empty pool id",
            });
        }
        if !pool_ids.insert(pool.id.as_str()) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "block value-pool balances cannot contain duplicate pool ids",
            });
        }
    }

    Ok(())
}

fn validate_subtree_root_artifacts(
    subtree_roots: &[SubtreeRootArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut root_index = HashSet::new();
    for subtree_root in subtree_roots {
        if subtree_root.completing_block_height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "subtree-root completing height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&subtree_root.completing_block_height)
            != Some(&subtree_root.completing_block_hash)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "subtree-root artifact must match a block artifact at the completing height",
            });
        }

        if !root_index.insert((subtree_root.protocol, subtree_root.subtree_index)) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "subtree-root artifacts cannot repeat a protocol and index",
            });
        }
    }

    Ok(())
}

fn validate_transparent_output_artifacts(
    transparent_outputs_by_outpoint: &[TransparentOutputArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut outpoints = HashSet::<TransparentOutPoint>::new();
    for prevout in transparent_outputs_by_outpoint {
        if prevout.block_height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent output height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&prevout.block_height) != Some(&prevout.block_hash) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent output artifact must match a block artifact at the same height",
            });
        }

        if !outpoints.insert(prevout.outpoint) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent output artifacts cannot repeat an outpoint",
            });
        }
    }

    Ok(())
}

fn validate_transparent_spend_facts(
    transparent_spend_facts: &[TransparentSpendFact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut spent_outpoints = HashSet::<TransparentOutPoint>::new();
    for spend in transparent_spend_facts {
        if spend.block_height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent spend fact height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&spend.block_height) != Some(&spend.block_hash) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent spend fact must match a block artifact at the same height",
            });
        }

        if spend.spent_block_height > spend.block_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent spend fact cannot spend an output mined after the spending block",
            });
        }

        if let Some(committed_spent_block_hash) =
            block_hash_by_height.get(&spend.spent_block_height)
            && committed_spent_block_hash != &spend.spent_block_hash
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent spend fact spent-output block hash must match the committed block at the same height",
            });
        }

        if !spent_outpoints.insert(spend.spent_outpoint) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent spend facts cannot repeat a spent outpoint",
            });
        }
    }

    Ok(())
}

fn first_committed_block_height(artifacts: &ChainEpochArtifacts) -> BlockHeight {
    artifacts
        .block_headers
        .iter()
        .map(|block| block.height)
        .min()
        .map_or(BlockHeight::new(1), |height| height)
}

fn validate_committed_boundary_hash_if_present(
    height: BlockHeight,
    hash: BlockHash,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
    reason: &'static str,
) -> Result<(), StoreError> {
    if let Some(committed_hash) = block_hash_by_height.get(&height)
        && *committed_hash != hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts { reason });
    }

    Ok(())
}
