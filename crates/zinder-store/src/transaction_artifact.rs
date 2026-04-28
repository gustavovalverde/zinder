//! Transaction artifact read traits.

use zinder_core::{ChainEpoch, TransactionArtifact, TransactionId};

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
}

pub(crate) fn read_transaction_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    transaction_id: TransactionId,
) -> Result<Option<TransactionArtifact>, StoreError> {
    let prefix = StoreKey::visible_transaction_epoch_prefix(chain_epoch.network, transaction_id);
    let seek_key =
        StoreKey::visible_transaction_epoch(chain_epoch.network, transaction_id, chain_epoch.id);
    let Some(source_epoch_bytes) =
        inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
    else {
        return Ok(None);
    };

    let source_epoch =
        decode_visible_source_epoch(ArtifactFamily::Transaction, &seek_key, &source_epoch_bytes)?;
    let key = StoreKey::transaction(chain_epoch.network, source_epoch, transaction_id);
    let Some(envelope_bytes) = inner.get(StorageTable::Transaction, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::Transaction,
            key: key.into(),
        });
    };

    let transaction = decode_transaction_artifact(&key, &envelope_bytes)?;
    // Re-read the visible block before returning so reverted-branch transactions
    // cannot leak through visibility rows that predate replacement cleanup.
    let Some(block) = read_block_artifact(inner, chain_epoch, transaction.block_height)? else {
        return Ok(None);
    };

    if block.block_hash == transaction.block_hash {
        return Ok(Some(transaction));
    }

    Ok(None)
}
