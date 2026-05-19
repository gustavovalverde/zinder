//! Transaction artifact read traits.

use std::collections::{HashMap, HashSet};

use zinder_core::{BlockHeight, ChainEpoch, TransactionArtifact, TransactionId};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::decode_visible_source_epoch,
    block_artifact::read_block_artifact,
    format::{StoreKey, decode_transaction_artifact},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for transaction artifacts.
pub trait TransactionArtifactStore {
    /// Reads the transaction artifact for `transaction_id` in the reader's chain epoch.
    fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionArtifact>, StoreError>;

    /// Reads transaction artifacts for many `transaction_ids` in one batched store read.
    ///
    /// Equivalent to calling [`transaction_by_id`](Self::transaction_by_id) for each
    /// input, but collapses the Transaction CF reads into a single `multi_get` and
    /// the per-transaction block-artifact cross-check into one lookup per unique
    /// `block_height` instead of one per transaction.
    fn transactions_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionArtifact>>, StoreError>;
}

pub(crate) fn read_transaction_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionArtifact>, StoreError> {
    let mut batch = read_transaction_artifacts_batch(inner, chain_epoch, &[transaction_id])?;
    Ok(batch.remove(&transaction_id).flatten())
}

/// Reads transaction artifacts for many ids in one batched store read.
///
/// Returned map always contains an entry for every input id (mapping to
/// `None` when the id is unknown or its visibility row points at a reverted
/// block). Reorg safety stays equivalent to [`read_transaction_artifact`]:
/// every returned artifact is gated on
/// `block.block_hash == transaction.block_hash`, where the canonical block
/// at `transaction.block_height` is fetched once per unique height instead
/// of once per transaction.
pub(crate) fn read_transaction_artifacts_batch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_ids: &[TransactionId],
) -> Result<HashMap<TransactionId, Option<TransactionArtifact>>, StoreError> {
    // Seek phase: per unique id, locate the visibility row and derive the
    // Transaction-CF key. The visibility lookup is per-id because the seek
    // prefix differs; RocksDB has no batched `seek_for_prev`.
    let mut unique_ids: Vec<TransactionId> = Vec::with_capacity(transaction_ids.len());
    let mut seen: HashSet<TransactionId> = HashSet::with_capacity(transaction_ids.len());
    for &transaction_id in transaction_ids {
        if seen.insert(transaction_id) {
            unique_ids.push(transaction_id);
        }
    }

    let mut transaction_keys: Vec<StoreKey> = Vec::with_capacity(unique_ids.len());
    let mut seek_outcomes: Vec<(TransactionId, Option<StoreKey>)> =
        Vec::with_capacity(unique_ids.len());
    for transaction_id in &unique_ids {
        let prefix =
            StoreKey::visible_transaction_epoch_prefix(chain_epoch.network, *transaction_id);
        let seek_key = StoreKey::visible_transaction_epoch(
            chain_epoch.network,
            *transaction_id,
            chain_epoch.id,
        );
        let Some(source_epoch_bytes) =
            inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
        else {
            seek_outcomes.push((*transaction_id, None));
            continue;
        };
        let source_epoch = decode_visible_source_epoch(
            ArtifactFamily::Transaction,
            &seek_key,
            &source_epoch_bytes,
        )?;
        let key = StoreKey::transaction(chain_epoch.network, source_epoch, *transaction_id);
        transaction_keys.push(key.clone());
        seek_outcomes.push((*transaction_id, Some(key)));
    }

    // Transaction batch read: one `multi_get` for every visibility hit.
    let mut transaction_values = inner
        .multi_get(StorageTable::Transaction, &transaction_keys)?
        .into_iter();

    // Decode phase: pull each envelope back into a typed artifact. Track the
    // set of distinct block heights the artifacts reference so the dedup
    // phase can issue one block lookup per height.
    let mut decoded: Vec<(TransactionId, Option<TransactionArtifact>)> =
        Vec::with_capacity(seek_outcomes.len());
    let mut needed_heights: HashSet<BlockHeight> = HashSet::new();
    for (transaction_id, key_option) in seek_outcomes {
        let Some(key) = key_option else {
            decoded.push((transaction_id, None));
            continue;
        };
        let envelope_value = transaction_values
            .next()
            .ok_or(StoreError::ArtifactMissing {
                family: ArtifactFamily::Transaction,
                key: key.clone().into(),
            })?;
        let Some(envelope_bytes) = envelope_value else {
            return Err(StoreError::ArtifactMissing {
                family: ArtifactFamily::Transaction,
                key: key.into(),
            });
        };
        let transaction = decode_transaction_artifact(&key, &envelope_bytes)?;
        needed_heights.insert(transaction.block_height);
        decoded.push((transaction_id, Some(transaction)));
    }

    // Block dedup phase: one canonical-block read per unique height. The
    // reorg-safety invariant from `read_transaction_artifact` is preserved
    // below by comparing each transaction's recorded `block_hash` against
    // the canonical block fetched here.
    let mut canonical_blocks_by_height = HashMap::with_capacity(needed_heights.len());
    for height in needed_heights {
        if let Some(block) = read_block_artifact(inner, chain_epoch, height)? {
            canonical_blocks_by_height.insert(height, block);
        }
    }

    // Filter phase: drop any transaction whose block hash does not match
    // the canonical block at its height (its visibility row points at a
    // reverted branch that hasn't been cleaned yet).
    let mut artifacts_by_id: HashMap<TransactionId, Option<TransactionArtifact>> =
        HashMap::with_capacity(transaction_ids.len());
    for (transaction_id, artifact) in decoded {
        let retained = artifact.and_then(|artifact| {
            canonical_blocks_by_height
                .get(&artifact.block_height)
                .filter(|block| block.block_hash == artifact.block_hash)
                .map(|_| artifact)
        });
        artifacts_by_id.insert(transaction_id, retained);
    }

    Ok(artifacts_by_id)
}
