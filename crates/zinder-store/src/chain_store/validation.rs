use std::collections::{HashMap, HashSet};

use zinder_core::{
    AddressOutputIndexArtifact, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange,
    ChainEpoch, CompactBlockArtifact, SubtreeRootArtifact, TransactionFactsArtifact,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, TreeStateArtifact,
};

use crate::{
    ChainEpochArtifacts, ReorgWindowChange, StoreError, block_artifact::read_block_header_artifact,
    kv::RocksChainStore,
};

use super::ChainStoreOptions;

pub(super) fn validate_chain_store_options(options: ChainStoreOptions) -> Result<(), StoreError> {
    if options.reorg_window_blocks == 0 {
        return Err(StoreError::InvalidChainStoreOptions {
            reason: "reorg window blocks must be greater than zero",
        });
    }
    options
        .rocksdb_resource_budget
        .validate()
        .map_err(|reason| StoreError::InvalidChainStoreOptions { reason })?;

    Ok(())
}

pub(super) fn validate_chain_epoch_artifacts(
    artifacts: &ChainEpochArtifacts,
) -> Result<(), StoreError> {
    if artifacts.chain_epoch.id.value() == 0 {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "chain epoch id must be greater than zero",
        });
    }

    validate_artifact_presence(artifacts)?;

    let tip_height = artifacts.chain_epoch.tip_height;
    validate_safe_tip_height(artifacts.chain_epoch)?;
    let block_hash_by_height = block_hash_by_height(&artifacts.block_headers)?;
    validate_committed_boundary_hash_if_present(
        artifacts.chain_epoch.tip_height,
        artifacts.chain_epoch.tip_hash,
        &block_hash_by_height,
        "tip hash must match the committed block at tip height",
    )?;
    validate_committed_boundary_hash_if_present(
        artifacts.chain_epoch.safe_tip_height,
        artifacts.chain_epoch.safe_tip_hash,
        &block_hash_by_height,
        "safe_tip_hash must match the committed block at safe_tip_height",
    )?;
    validate_block_header_artifacts(&artifacts.block_headers, tip_height)?;
    validate_compact_block_artifacts(&artifacts.compact_blocks, tip_height, &block_hash_by_height)?;
    validate_transaction_facts_artifacts(
        &artifacts.transaction_facts,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_tree_state_artifacts(&artifacts.tree_states, tip_height, &block_hash_by_height)?;
    validate_subtree_root_artifacts(&artifacts.subtree_roots, tip_height, &block_hash_by_height)?;
    validate_address_output_index_artifacts(
        &artifacts.address_output_index,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_transparent_output_artifacts(
        &artifacts.transparent_outputs_by_outpoint,
        &artifacts.address_output_index,
        tip_height,
        &block_hash_by_height,
    )?;
    validate_transparent_spend_facts(
        &artifacts.transparent_spend_facts,
        tip_height,
        &block_hash_by_height,
    )
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
            artifacts.chain_epoch.safe_tip_height,
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
        && artifacts.compact_blocks.is_empty()
        && artifacts.transaction_facts.is_empty()
        && artifacts.tree_states.is_empty()
        && artifacts.subtree_roots.is_empty()
        && artifacts.address_output_index.is_empty()
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
                    safe_tip_height: current_chain_epoch.safe_tip_height,
                });
            }

            if from_height > artifacts.chain_epoch.tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement start height cannot exceed tip height",
                });
            }

            if from_height > current_chain_epoch.tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement start height cannot exceed current tip height",
                });
            }

            validate_replacement_preserves_safe_tip_anchor(artifacts, current_chain_epoch)?;
            validate_replacement_artifact_coverage(artifacts, from_height)
        }
        ReorgWindowChange::AdvanceSafeTipTo { height } => {
            if height > artifacts.chain_epoch.safe_tip_height {
                return Err(StoreError::InvalidChainEpochArtifacts {
                    reason: "AdvanceSafeTipTo height cannot exceed epoch safe_tip_height",
                });
            }

            Ok(())
        }
        ReorgWindowChange::Extend { block_range } => {
            if block_range.end > artifacts.chain_epoch.tip_height {
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
            artifacts.chain_epoch.tip_height,
        )),
        ReorgWindowChange::Extend { .. }
        | ReorgWindowChange::AdvanceSafeTipTo { .. }
        | ReorgWindowChange::Unchanged => match current_chain_epoch {
            Some(current_chain_epoch)
                if artifacts.chain_epoch.tip_height > current_chain_epoch.tip_height =>
            {
                let next_height = current_chain_epoch.tip_height.value().saturating_add(1);
                Some(BlockHeightRange::inclusive(
                    BlockHeight::new(next_height),
                    artifacts.chain_epoch.tip_height,
                ))
            }
            Some(_) => None,
            None => Some(BlockHeightRange::inclusive(
                first_committed_block_height(artifacts),
                artifacts.chain_epoch.tip_height,
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
                == current_chain_epoch.tip_height.value() =>
        {
            // First appended block links directly to the visible chain tip
            // recorded on the chain epoch. This path is required when the
            // store was bootstrapped from a checkpoint and has no stored
            // block at the checkpoint height.
            Some(current_chain_epoch.tip_hash)
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
    let safe_tip_height = artifacts.chain_epoch.safe_tip_height;
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
        if *committed_hash != artifacts.chain_epoch.safe_tip_hash {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "safe_tip_hash must match the committed block at safe_tip_height",
            });
        }

        return Ok(());
    }

    if let Some(current_chain_epoch) = current_chain_epoch
        && safe_tip_height <= current_chain_epoch.tip_height
    {
        let safe_tip_hash =
            visible_block_hash_at(inner, Some(current_chain_epoch), safe_tip_height)?;
        if safe_tip_hash == artifacts.chain_epoch.safe_tip_hash {
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
                    if artifacts.chain_epoch.tip_height > current_chain_epoch.tip_height =>
                {
                    let next_height = current_chain_epoch.tip_height.value().saturating_add(1);
                    Some(BlockHeightRange::inclusive(
                        BlockHeight::new(next_height),
                        artifacts.chain_epoch.tip_height,
                    ))
                }
                Some(_) => None,
                None => Some(BlockHeightRange::inclusive(
                    first_committed_block_height(artifacts),
                    artifacts.chain_epoch.tip_height,
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
    if chain_epoch.tip_height < current_chain_epoch.tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot lower tip height",
        });
    }

    if chain_epoch.safe_tip_height < current_chain_epoch.safe_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot lower safe_tip_height",
        });
    }

    if chain_epoch.tip_height == current_chain_epoch.tip_height
        && chain_epoch.tip_hash != current_chain_epoch.tip_hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot change the tip hash at the current tip height",
        });
    }

    if chain_epoch.safe_tip_height == current_chain_epoch.safe_tip_height
        && chain_epoch.safe_tip_hash != current_chain_epoch.safe_tip_hash
    {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "non-replacement commit cannot change safe_tip_hash at the current safe_tip_height",
        });
    }

    Ok(())
}

fn minimum_reorg_height(chain_epoch: ChainEpoch, reorg_window_blocks: u32) -> BlockHeight {
    let safe_tip_floor = chain_epoch.safe_tip_height.value().saturating_add(1);
    let window_floor = chain_epoch
        .tip_height
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
        BlockHeightRange::inclusive(from_height, artifacts.chain_epoch.tip_height),
        "replacement commits must include every replaced block and compact block",
    )
}

fn validate_replacement_preserves_safe_tip_anchor(
    artifacts: &ChainEpochArtifacts,
    current_chain_epoch: ChainEpoch,
) -> Result<(), StoreError> {
    if artifacts.chain_epoch.safe_tip_height < current_chain_epoch.safe_tip_height {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "replacement commit cannot lower safe_tip_height",
        });
    }

    if artifacts.chain_epoch.safe_tip_height == current_chain_epoch.safe_tip_height
        && artifacts.chain_epoch.safe_tip_hash != current_chain_epoch.safe_tip_hash
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

fn validate_safe_tip_height(chain_epoch: ChainEpoch) -> Result<(), StoreError> {
    if chain_epoch.safe_tip_height > chain_epoch.tip_height {
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

fn validate_address_output_index_artifacts(
    address_output_index: &[AddressOutputIndexArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut outpoints = HashSet::<TransparentOutPoint>::new();
    for address_output in address_output_index {
        if address_output.block_height > tip_height {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent address output height cannot exceed tip height",
            });
        }

        if block_hash_by_height.get(&address_output.block_height)
            != Some(&address_output.block_hash)
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent address output artifact must match a block artifact at the same height",
            });
        }

        if !outpoints.insert(address_output.outpoint) {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent address output artifacts cannot repeat an outpoint",
            });
        }
    }

    Ok(())
}

fn validate_transparent_output_artifacts(
    transparent_outputs_by_outpoint: &[TransparentOutputArtifact],
    address_output_index: &[AddressOutputIndexArtifact],
    tip_height: BlockHeight,
    block_hash_by_height: &HashMap<BlockHeight, BlockHash>,
) -> Result<(), StoreError> {
    let mut prevouts_by_outpoint =
        HashMap::<TransparentOutPoint, &TransparentOutputArtifact>::new();
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

        if prevouts_by_outpoint
            .insert(prevout.outpoint, prevout)
            .is_some()
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent output artifacts cannot repeat an outpoint",
            });
        }
    }

    for address_output in address_output_index {
        let Some(prevout) = prevouts_by_outpoint.get(&address_output.outpoint) else {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent address output artifacts require matching transparent output artifacts",
            });
        };
        if prevout.value_zat != address_output.value_zat
            || prevout.script_pub_key != address_output.script_pub_key
            || prevout.address_script_hash != address_output.address_script_hash
            || prevout.block_height != address_output.block_height
            || prevout.block_hash != address_output.block_hash
        {
            return Err(StoreError::InvalidChainEpochArtifacts {
                reason: "transparent output artifact must match its transparent address output artifact",
            });
        }
    }

    if prevouts_by_outpoint.len() != address_output_index.len() {
        return Err(StoreError::InvalidChainEpochArtifacts {
            reason: "transparent output artifacts require matching transparent address output artifacts",
        });
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
