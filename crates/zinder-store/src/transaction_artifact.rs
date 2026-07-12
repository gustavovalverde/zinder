//! Transaction fact/blob read traits.

use std::collections::{HashMap, HashSet};

use zinder_core::{
    BlockHeaderArtifact, BlockHeight, ChainEpoch, TransactionBlobArtifact,
    TransactionFactsArtifact, TransactionId, TransactionIntrinsicValueBalancesArtifact,
    TransactionLocation,
};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::decode_visible_source_epoch,
    block_artifact::read_block_header_artifact,
    format::{
        StoreKey, decode_transaction_blob_artifact, decode_transaction_facts_artifact,
        decode_transaction_intrinsic_value_balances, decode_transaction_location_artifact,
    },
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for canonical transaction facts.
pub trait TransactionFactsStore {
    /// Reads the transaction facts for `transaction_id` in the reader's chain epoch.
    fn transaction_facts_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionFactsArtifact>, StoreError>;

    /// Reads transaction facts for many `transaction_ids` in one batched store read.
    ///
    /// Equivalent to calling [`transaction_facts_by_id`](Self::transaction_facts_by_id)
    /// for each input, but collapses the facts reads into a single `multi_get` and
    /// the per-transaction block-header cross-check into one lookup per unique
    /// `block_height` instead of one per transaction.
    fn transaction_facts_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, StoreError>;
}

/// Read boundary for canonical transaction locations.
pub trait TransactionLocationStore {
    /// Reads the transaction location for `transaction_id` in the reader's chain epoch.
    fn transaction_location_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionLocation>, StoreError>;
}

/// Read boundary for optional raw transaction blobs.
pub trait TransactionBlobStore {
    /// Reads the raw transaction blob for `transaction_id` in the reader's chain epoch.
    fn transaction_blob_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionBlobArtifact>, StoreError>;
}

/// Read boundary for optional transaction-intrinsic shielded value balances.
pub trait TransactionIntrinsicValueBalancesStore {
    /// Reads intrinsic value balances for `transaction_id` in the reader's chain epoch.
    fn transaction_intrinsic_value_balances_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionIntrinsicValueBalancesArtifact>, StoreError>;

    /// Reads intrinsic value balances for many transaction ids in one batched store read.
    fn transaction_intrinsic_value_balances_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionIntrinsicValueBalancesArtifact>>, StoreError>;
}

pub(crate) fn read_transaction_facts_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionFactsArtifact>, StoreError> {
    let mut batch = read_transaction_facts_artifacts_batch(inner, chain_epoch, &[transaction_id])?;
    Ok(batch.remove(&transaction_id).flatten())
}

/// Reads transaction facts for many ids in one batched store read.
///
/// Returned map always contains an entry for every input id (mapping to
/// `None` when the id is unknown or its visibility row points at a reverted
/// block). Reorg safety stays equivalent to [`read_transaction_facts_artifact`]:
/// every returned artifact is gated on
/// `block.block_hash == transaction.block_hash`, where the canonical block
/// at `transaction.block_height` is fetched once per unique height instead
/// of once per transaction.
pub(crate) fn read_transaction_facts_artifacts_batch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_ids: &[TransactionId],
) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, StoreError> {
    read_transaction_facts_artifacts_batch_with_known_headers(
        inner,
        chain_epoch,
        transaction_ids,
        &HashMap::new(),
    )
}

/// Keys to `multi_get` from the transaction-facts seek phase, alongside
/// every input id's seek outcome (`None` when the id has no visible row).
type TransactionFactsSeekResult = (Vec<StoreKey>, Vec<(TransactionId, Option<StoreKey>)>);

/// Seek phase shared by the transaction-facts batch readers.
///
/// Per unique id, locates the visibility row and derives the
/// Transaction-CF key. The visibility lookup is per-id because the seek
/// prefix differs; `RocksDB` has no batched `seek_for_prev`.
///
/// Returns the keys to `multi_get` (in visibility-hit order) alongside every
/// input id's seek outcome (`None` when the id has no visible row), so the
/// caller can zip the `multi_get` results back onto their originating id.
fn seek_transaction_facts_keys(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_ids: &[TransactionId],
) -> Result<TransactionFactsSeekResult, StoreError> {
    // Seek phase: per unique id, locate the visibility row and derive the
    // Transaction-CF key. The visibility lookup is per-id because the seek
    // prefix differs; RocksDB has no batched `seek_for_prev`.
    let unique_ids = unique_transaction_ids(transaction_ids);

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
            ArtifactFamily::TransactionFacts,
            &seek_key,
            &source_epoch_bytes,
        )?;
        let key = StoreKey::transaction_facts(chain_epoch.network, source_epoch, *transaction_id);
        transaction_keys.push(key.clone());
        seek_outcomes.push((*transaction_id, Some(key)));
    }
    Ok((transaction_keys, seek_outcomes))
}

/// Reads transaction facts for many ids in one batched store read, reusing
/// block headers the caller already fetched for the reorg-safety
/// cross-check instead of re-reading them.
///
/// Equivalent to [`read_transaction_facts_artifacts_batch`], but the block
/// dedup phase looks a needed height up in `known_block_headers` first and
/// only falls back to a fresh store read for heights not present there.
/// Callers that stage every needed height's header up front (as derive
/// replay does) pay zero extra block reads in the common case.
pub(crate) fn read_transaction_facts_artifacts_batch_with_known_headers(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_ids: &[TransactionId],
    known_block_headers: &HashMap<BlockHeight, BlockHeaderArtifact>,
) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, StoreError> {
    let (transaction_keys, seek_outcomes) =
        seek_transaction_facts_keys(inner, chain_epoch, transaction_ids)?;

    // Transaction batch read: one `multi_get` for every visibility hit.
    let mut transaction_values = inner
        .multi_get(StorageTable::TransactionFacts, &transaction_keys)?
        .into_iter();

    // Decode phase: pull each envelope back into a typed artifact. Track the
    // set of distinct block heights the artifacts reference so the dedup
    // phase can issue one block lookup per height.
    let mut decoded: Vec<(TransactionId, Option<TransactionFactsArtifact>)> =
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
                family: ArtifactFamily::TransactionFacts,
                key: key.clone().into(),
            })?;
        let Some(envelope_bytes) = envelope_value else {
            return Err(StoreError::ArtifactMissing {
                family: ArtifactFamily::TransactionFacts,
                key: key.into(),
            });
        };
        let transaction = decode_transaction_facts_artifact(&key, &envelope_bytes)?;
        needed_heights.insert(transaction.location.block_height);
        decoded.push((transaction_id, Some(transaction)));
    }

    // Block dedup phase: one canonical-block read per unique height not
    // already supplied by the caller. The reorg-safety invariant from
    // `read_transaction_artifact` is preserved below by comparing each
    // transaction's recorded `block_hash` against the canonical block
    // resolved here.
    let mut canonical_blocks_by_height = HashMap::with_capacity(needed_heights.len());
    for height in needed_heights {
        if let Some(block) = known_block_headers.get(&height) {
            canonical_blocks_by_height.insert(height, block.clone());
            continue;
        }
        if let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? {
            canonical_blocks_by_height.insert(height, block);
        }
    }

    // Filter phase: drop any transaction whose block hash does not match
    // the canonical block at its height (its visibility row points at a
    // reverted branch that hasn't been cleaned yet).
    let mut artifacts_by_id: HashMap<TransactionId, Option<TransactionFactsArtifact>> =
        HashMap::with_capacity(transaction_ids.len());
    for (transaction_id, artifact) in decoded {
        let retained = artifact.and_then(|artifact| {
            canonical_blocks_by_height
                .get(&artifact.location.block_height)
                .filter(|block| block.block_hash == artifact.location.block_hash)
                .map(|_| artifact)
        });
        artifacts_by_id.insert(transaction_id, retained);
    }

    Ok(artifacts_by_id)
}

pub(crate) fn read_transaction_location(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionLocation>, StoreError> {
    let Some((source_epoch, seek_key)) =
        visible_transaction_source_epoch(inner, chain_epoch, transaction_id)?
    else {
        return Ok(None);
    };
    let key = StoreKey::transaction_location(chain_epoch.network, source_epoch, transaction_id);
    let Some(envelope_bytes) = inner.get(StorageTable::TransactionLocation, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::TransactionLocation,
            key: key.into(),
        });
    };
    let location = decode_transaction_location_artifact(&key, &envelope_bytes)?;
    if let Some(block) = read_block_header_artifact(inner, chain_epoch, location.block_height)?
        && block.block_hash == location.block_hash
    {
        return Ok(Some(location));
    }
    let _ = seek_key;
    Ok(None)
}

pub(crate) fn read_transaction_blob_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionBlobArtifact>, StoreError> {
    let Some((source_epoch, _seek_key)) =
        visible_transaction_source_epoch(inner, chain_epoch, transaction_id)?
    else {
        return Ok(None);
    };
    let key = StoreKey::transaction_blob(chain_epoch.network, source_epoch, transaction_id);
    let Some(envelope_bytes) = inner.get(StorageTable::TransactionBlob, &key)? else {
        return Ok(None);
    };
    let blob = decode_transaction_blob_artifact(&key, &envelope_bytes)?;
    if let Some(block) = read_block_header_artifact(inner, chain_epoch, blob.location.block_height)?
        && block.block_hash == blob.location.block_hash
    {
        return Ok(Some(blob));
    }
    Ok(None)
}

pub(crate) fn read_transaction_intrinsic_value_balances(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionIntrinsicValueBalancesArtifact>, StoreError> {
    let mut batch =
        read_transaction_intrinsic_value_balances_batch(inner, chain_epoch, &[transaction_id])?;
    Ok(batch.remove(&transaction_id).flatten())
}

/// Reads optional intrinsic value balances for many ids in one batched store read.
///
/// The returned map contains every unique input id. Unknown ids, ids without an
/// additive intrinsic-balance artifact, and artifacts on reverted branches map
/// to `None`.
pub(crate) fn read_transaction_intrinsic_value_balances_batch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_ids: &[TransactionId],
) -> Result<HashMap<TransactionId, Option<TransactionIntrinsicValueBalancesArtifact>>, StoreError> {
    let unique_ids = unique_transaction_ids(transaction_ids);

    let mut artifact_keys = Vec::with_capacity(unique_ids.len());
    let mut seek_outcomes = Vec::with_capacity(unique_ids.len());
    for transaction_id in unique_ids {
        let prefix =
            StoreKey::visible_transaction_epoch_prefix(chain_epoch.network, transaction_id);
        let seek_key = StoreKey::visible_transaction_epoch(
            chain_epoch.network,
            transaction_id,
            chain_epoch.id,
        );
        let Some(source_epoch_bytes) =
            inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
        else {
            seek_outcomes.push((transaction_id, None));
            continue;
        };
        let source_epoch = decode_visible_source_epoch(
            ArtifactFamily::TransactionIntrinsicValueBalances,
            &seek_key,
            &source_epoch_bytes,
        )?;
        let artifact_key = StoreKey::transaction_intrinsic_value_balances(
            chain_epoch.network,
            source_epoch,
            transaction_id,
        );
        artifact_keys.push(artifact_key.clone());
        seek_outcomes.push((transaction_id, Some(artifact_key)));
    }

    let mut artifact_values = inner
        .multi_get(
            StorageTable::TransactionIntrinsicValueBalances,
            &artifact_keys,
        )?
        .into_iter();
    let mut decoded = Vec::with_capacity(seek_outcomes.len());
    let mut needed_heights = HashSet::new();
    for (transaction_id, key_option) in seek_outcomes {
        let Some(key) = key_option else {
            decoded.push((transaction_id, None));
            continue;
        };
        let envelope_value = artifact_values.next().ok_or(StoreError::ArtifactMissing {
            family: ArtifactFamily::TransactionIntrinsicValueBalances,
            key: key.clone().into(),
        })?;
        let Some(envelope_bytes) = envelope_value else {
            decoded.push((transaction_id, None));
            continue;
        };
        let artifact = decode_transaction_intrinsic_value_balances(&key, &envelope_bytes)?;
        if artifact.location.transaction_id != transaction_id {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::TransactionIntrinsicValueBalances,
                key: key.into(),
                reason: "transaction intrinsic value-balances id does not match its key",
            });
        }
        needed_heights.insert(artifact.location.block_height);
        decoded.push((transaction_id, Some(artifact)));
    }

    let mut canonical_blocks_by_height = HashMap::with_capacity(needed_heights.len());
    for height in needed_heights {
        if let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? {
            canonical_blocks_by_height.insert(height, block);
        }
    }

    let mut artifacts_by_id = HashMap::with_capacity(decoded.len());
    for (transaction_id, artifact) in decoded {
        let retained = artifact.and_then(|artifact| {
            canonical_blocks_by_height
                .get(&artifact.location.block_height)
                .filter(|block| block.block_hash == artifact.location.block_hash)
                .map(|_| artifact)
        });
        artifacts_by_id.insert(transaction_id, retained);
    }
    Ok(artifacts_by_id)
}

fn unique_transaction_ids(transaction_ids: &[TransactionId]) -> Vec<TransactionId> {
    let mut unique_ids = Vec::with_capacity(transaction_ids.len());
    let mut seen = HashSet::with_capacity(transaction_ids.len());
    for &transaction_id in transaction_ids {
        if seen.insert(transaction_id) {
            unique_ids.push(transaction_id);
        }
    }
    unique_ids
}

pub(crate) fn visible_transaction_source_epoch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<(zinder_core::ChainEpochId, StoreKey)>, StoreError> {
    let prefix = StoreKey::visible_transaction_epoch_prefix(chain_epoch.network, transaction_id);
    let seek_key =
        StoreKey::visible_transaction_epoch(chain_epoch.network, transaction_id, chain_epoch.id);
    let Some(source_epoch_bytes) =
        inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
    else {
        return Ok(None);
    };
    let source_epoch = decode_visible_source_epoch(
        ArtifactFamily::TransactionLocation,
        &seek_key,
        &source_epoch_bytes,
    )?;
    Ok(Some((source_epoch, seek_key)))
}
