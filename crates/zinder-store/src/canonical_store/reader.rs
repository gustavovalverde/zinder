//! Typed serving reads for an admitted version-1 canonical store.

use crate::BoundedRocksDbOpen;
use rust_rocksdb::{Direction, IteratorMode};
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, ChainEpoch,
    ChainTipMetadata, CommitmentTreeCheckpoint, CompactBlockArtifact, Network, SubtreeRootArtifact,
    SubtreeRootRange, TransactionBlobArtifact, TransactionId, TransactionLocation,
    UnixTimestampMillis, wire::encode_internal_transaction_id,
};

use super::{
    CANONICAL_STORE_SCHEMA_VERSION, CanonicalStoreError, RocksDbCanonicalSecondary,
    RocksDbCanonicalStore,
    block_load::{
        BLOCK_HEADER_VALUE_LEN, decode_tree_state_checkpoint, encode_block_position,
        encode_transaction_position,
    },
    publication::column_family,
    rocksdb::{
        BLOCK_HEADER_COLUMN_FAMILY, CHAIN_EPOCH_COLUMN_FAMILY, COMPACT_BLOCK_COLUMN_FAMILY,
        SUBTREE_ROOT_COLUMN_FAMILY, TRANSACTION_BLOB_COLUMN_FAMILY,
        TRANSACTION_LOCATION_COLUMN_FAMILY, TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
    },
    subtree_load::{decode_subtree_root, encode_subtree_root_key},
};

const CHAIN_EPOCH_VALUE_BYTES: usize = 93;
const TRANSACTION_LOCATION_VALUE_BYTES: usize = 40;

trait CanonicalServingRead {
    fn serving_open(&self) -> &BoundedRocksDbOpen;
    fn serving_network(&self) -> Network;
    fn serving_build_plan(&self) -> &super::CanonicalStoreBuildPlan;
    fn serving_ready_evidence(&self) -> super::CanonicalStoreReadyEvidence;
}

impl CanonicalServingRead for RocksDbCanonicalStore {
    fn serving_open(&self) -> &BoundedRocksDbOpen {
        &self.bounded_open
    }

    fn serving_network(&self) -> Network {
        self.network()
    }

    fn serving_build_plan(&self) -> &super::CanonicalStoreBuildPlan {
        self.build_plan()
    }

    fn serving_ready_evidence(&self) -> super::CanonicalStoreReadyEvidence {
        self.ready_evidence()
    }
}

impl CanonicalServingRead for RocksDbCanonicalSecondary {
    fn serving_open(&self) -> &BoundedRocksDbOpen {
        &self.bounded_open
    }

    fn serving_network(&self) -> Network {
        self.network()
    }

    fn serving_build_plan(&self) -> &super::CanonicalStoreBuildPlan {
        self.build_plan()
    }

    fn serving_ready_evidence(&self) -> super::CanonicalStoreReadyEvidence {
        self.ready_evidence()
    }
}

macro_rules! impl_canonical_typed_reads {
    ($store:ty) => {
        impl $store {
            /// Reads the exact visible epoch admitted by this READY store.
            pub fn chain_epoch(&self) -> Result<ChainEpoch, CanonicalStoreError> {
                read_chain_epoch(self)
            }

            /// Reads one immutable canonical epoch retained by this admitted store.
            ///
            /// Retained transition replay uses this to reconstruct the exact
            /// `ChainView` that resulted from an event, rather than projecting a
            /// later visible epoch onto historical events. The method does not
            /// open another store or mutate the primary.
            pub fn chain_epoch_at(
                &self,
                epoch_id: zinder_core::ChainEpochId,
            ) -> Result<ChainEpoch, CanonicalStoreError> {
                read_chain_epoch_at(self, epoch_id)
            }

            /// Reads one canonical block header by height.
            pub fn block_header_at(
                &self,
                height: BlockHeight,
            ) -> Result<Option<BlockHeaderArtifact>, CanonicalStoreError> {
                read_block_header_at(self, height)
            }

            /// Reads one compact block by height.
            pub fn compact_block_at(
                &self,
                height: BlockHeight,
            ) -> Result<Option<CompactBlockArtifact>, CanonicalStoreError> {
                read_compact_block_at(self, height)
            }

            /// Reads an inclusive compact-block range in ascending height order.
            pub fn compact_blocks_in_range(
                &self,
                range: zinder_core::BlockHeightRange,
            ) -> Result<Vec<CompactBlockArtifact>, CanonicalStoreError> {
                read_compact_blocks_in_range(self, range)
            }

            /// Reads the canonical location of one transaction.
            pub fn transaction_location(
                &self,
                transaction_id: TransactionId,
            ) -> Result<Option<TransactionLocation>, CanonicalStoreError> {
                read_transaction_location(self, transaction_id)
            }

            /// Reads one retained raw transaction at its authenticated location.
            pub fn transaction_blob(
                &self,
                location: TransactionLocation,
            ) -> Result<Option<TransactionBlobArtifact>, CanonicalStoreError> {
                read_transaction_blob(self, location)
            }

            /// Reads the newest commitment-tree checkpoint at or below `height`.
            pub fn tree_state_checkpoint_at_or_before(
                &self,
                height: BlockHeight,
            ) -> Result<Option<CommitmentTreeCheckpoint>, CanonicalStoreError> {
                read_tree_state_checkpoint_at_or_before(self, height)
            }

            /// Reads the contiguous subtree-root rows requested by `range`.
            pub fn subtree_roots(
                &self,
                range: SubtreeRootRange,
            ) -> Result<Vec<SubtreeRootArtifact>, CanonicalStoreError> {
                read_subtree_roots(self, range)
            }
        }
    };
}

impl_canonical_typed_reads!(RocksDbCanonicalStore);
impl_canonical_typed_reads!(RocksDbCanonicalSecondary);

fn read_chain_epoch(store: &impl CanonicalServingRead) -> Result<ChainEpoch, CanonicalStoreError> {
    read_chain_epoch_at(store, store.serving_ready_evidence().visible_epoch)
}

fn read_chain_epoch_at(
    store: &impl CanonicalServingRead,
    epoch_id: zinder_core::ChainEpochId,
) -> Result<ChainEpoch, CanonicalStoreError> {
    let encoded = read_optional(
        store,
        CHAIN_EPOCH_COLUMN_FAMILY,
        &epoch_id.value().to_be_bytes(),
        "chain epoch read",
    )?
    .ok_or_else(|| CanonicalStoreError::CanonicalEpochNotRetained {
        epoch_id: epoch_id.value(),
    })?;
    decode_chain_epoch(store.serving_network(), epoch_id, &encoded)
}

fn read_block_header_at(
    store: &impl CanonicalServingRead,
    height: BlockHeight,
) -> Result<Option<BlockHeaderArtifact>, CanonicalStoreError> {
    read_optional(
        store,
        BLOCK_HEADER_COLUMN_FAMILY,
        &encode_block_position(height),
        "block header read",
    )?
    .map(|encoded| decode_block_header(height, &encoded))
    .transpose()
}

fn read_compact_block_at(
    store: &impl CanonicalServingRead,
    height: BlockHeight,
) -> Result<Option<CompactBlockArtifact>, CanonicalStoreError> {
    let Some(payload_bytes) = read_optional(
        store,
        COMPACT_BLOCK_COLUMN_FAMILY,
        &encode_block_position(height),
        "compact block read",
    )?
    else {
        return Ok(None);
    };
    let header = read_block_header_at(store, height)?
        .ok_or_else(|| CanonicalStoreError::publication("compact block header is absent"))?;
    Ok(Some(CompactBlockArtifact::new(
        height,
        header.block_hash,
        payload_bytes,
    )))
}

fn read_compact_blocks_in_range(
    store: &impl CanonicalServingRead,
    range: zinder_core::BlockHeightRange,
) -> Result<Vec<CompactBlockArtifact>, CanonicalStoreError> {
    range
        .into_iter()
        .map(|height| {
            read_compact_block_at(store, height)?.ok_or_else(|| {
                CanonicalStoreError::publication(format!(
                    "compact block at height {} is absent",
                    height.value()
                ))
            })
        })
        .collect()
}

fn read_transaction_location(
    store: &impl CanonicalServingRead,
    transaction_id: TransactionId,
) -> Result<Option<TransactionLocation>, CanonicalStoreError> {
    read_optional(
        store,
        TRANSACTION_LOCATION_COLUMN_FAMILY,
        &encode_internal_transaction_id(transaction_id),
        "transaction location read",
    )?
    .map(|encoded| decode_transaction_location(transaction_id, &encoded))
    .transpose()
}

fn read_transaction_blob(
    store: &impl CanonicalServingRead,
    location: TransactionLocation,
) -> Result<Option<TransactionBlobArtifact>, CanonicalStoreError> {
    if read_transaction_location(store, location.transaction_id)? != Some(location) {
        return Ok(None);
    }
    Ok(read_optional(
        store,
        TRANSACTION_BLOB_COLUMN_FAMILY,
        &encode_transaction_position(location.block_height, location.tx_index_in_block),
        "transaction blob read",
    )?
    .map(|raw_transaction_bytes| TransactionBlobArtifact::new(location, raw_transaction_bytes)))
}

fn read_tree_state_checkpoint_at_or_before(
    store: &impl CanonicalServingRead,
    height: BlockHeight,
) -> Result<Option<CommitmentTreeCheckpoint>, CanonicalStoreError> {
    let family = column_family(
        &store.serving_open().db,
        TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
    )?;
    let start = encode_block_position(height);
    let mut rows = store
        .serving_open()
        .db
        .iterator_cf(&family, IteratorMode::From(&start, Direction::Reverse));
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let (key, encoded) = row.map_err(|source| CanonicalStoreError::RocksDbOperation {
        operation: "tree-state checkpoint read",
        source,
    })?;
    let checkpoint_height = decode_height_key(&key)?;
    let (block_time_seconds, frontiers) = decode_tree_state_checkpoint(&encoded)
        .map_err(|source| CanonicalStoreError::publication(source.to_string()))?;
    let history_predecessor = store.serving_build_plan().history_predecessor();
    let block_id = if checkpoint_height == history_predecessor.block_id.height {
        if block_time_seconds != history_predecessor.block_time_seconds
            || frontiers != history_predecessor.frontiers
        {
            return Err(CanonicalStoreError::publication(
                "tree-state history predecessor does not match the admitted build plan",
            ));
        }
        history_predecessor.block_id
    } else {
        let header = read_block_header_at(store, checkpoint_height)?.ok_or_else(|| {
            CanonicalStoreError::publication("tree-state checkpoint header is absent")
        })?;
        BlockId::new(checkpoint_height, header.block_hash)
    };
    Ok(Some(CommitmentTreeCheckpoint::new(
        block_id,
        block_time_seconds,
        frontiers,
    )))
}

fn read_subtree_roots(
    store: &impl CanonicalServingRead,
    range: SubtreeRootRange,
) -> Result<Vec<SubtreeRootArtifact>, CanonicalStoreError> {
    let family = column_family(&store.serving_open().db, SUBTREE_ROOT_COLUMN_FAMILY)?;
    range
        .into_iter()
        .map(|subtree_index| {
            let probe = SubtreeRootArtifact::new(
                range.protocol,
                subtree_index,
                zinder_core::SubtreeRootHash::from_bytes([0; 32]),
                BlockHeight::new(0),
                BlockHash::from_bytes([0; 32]),
            );
            let key = encode_subtree_root_key(&probe);
            store
                .serving_open()
                .db
                .get_cf(&family, key)
                .map_err(|source| CanonicalStoreError::RocksDbOperation {
                    operation: "subtree-root read",
                    source,
                })?
                .map(|encoded| decode_subtree_root(&key, &encoded))
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|roots| roots.into_iter().flatten().collect())
}

fn read_optional(
    store: &impl CanonicalServingRead,
    family_name: &'static str,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<Vec<u8>>, CanonicalStoreError> {
    let family = column_family(&store.serving_open().db, family_name)?;
    store
        .serving_open()
        .db
        .get_cf(&family, key)
        .map_err(|source| CanonicalStoreError::RocksDbOperation { operation, source })
}

fn decode_chain_epoch(
    network: Network,
    epoch_id: zinder_core::ChainEpochId,
    encoded: &[u8],
) -> Result<ChainEpoch, CanonicalStoreError> {
    if encoded.len() != CHAIN_EPOCH_VALUE_BYTES || encoded.first() != Some(&1) {
        return Err(CanonicalStoreError::publication(
            "chain epoch is not the exact version-1 value",
        ));
    }
    Ok(ChainEpoch {
        id: epoch_id,
        network,
        visible_tip_height: BlockHeight::new(read_u32_le(encoded, 1)?),
        visible_tip_hash: BlockHash::from_bytes(read_array(encoded, 5)?),
        settled_tip_height: BlockHeight::new(read_u32_le(encoded, 37)?),
        settled_tip_hash: BlockHash::from_bytes(read_array(encoded, 41)?),
        artifact_schema_version: ArtifactSchemaVersion::new(CANONICAL_STORE_SCHEMA_VERSION),
        tip_metadata: ChainTipMetadata::new(
            read_u32_le(encoded, 73)?,
            read_u32_le(encoded, 77)?,
            read_u32_le(encoded, 81)?,
        ),
        created_at: UnixTimestampMillis::new(read_u64_le(encoded, 85)?),
    })
}

fn decode_block_header(
    height: BlockHeight,
    encoded: &[u8],
) -> Result<BlockHeaderArtifact, CanonicalStoreError> {
    if encoded.len() != BLOCK_HEADER_VALUE_LEN {
        return Err(CanonicalStoreError::publication(
            "block header is not the exact version-1 value",
        ));
    }
    Ok(BlockHeaderArtifact::new(
        height,
        BlockHash::from_bytes(read_array(encoded, 0)?),
        BlockHash::from_bytes(read_array(encoded, 32)?),
        read_array(encoded, 64)?,
        read_array(encoded, 96)?,
        read_i64_le(encoded, 128)?,
        read_u32_le(encoded, 136)?,
        read_array(encoded, 140)?,
        read_u32_le(encoded, 172)?,
        read_u64_le(encoded, 176)?,
    ))
}

fn decode_transaction_location(
    transaction_id: TransactionId,
    encoded: &[u8],
) -> Result<TransactionLocation, CanonicalStoreError> {
    if encoded.len() != TRANSACTION_LOCATION_VALUE_BYTES {
        return Err(CanonicalStoreError::publication(
            "transaction location is not the exact version-1 value",
        ));
    }
    Ok(TransactionLocation::new(
        transaction_id,
        BlockHeight::new(read_u32_be(encoded, 0)?),
        BlockHash::from_bytes(read_array(encoded, 4)?),
        read_u32_be(encoded, 36)?,
    ))
}

fn decode_height_key(encoded: &[u8]) -> Result<BlockHeight, CanonicalStoreError> {
    if encoded.len() != 4 {
        return Err(CanonicalStoreError::publication(
            "block-height key is not the exact version-1 value",
        ));
    }
    Ok(BlockHeight::new(u32::from_be_bytes(read_array(
        encoded, 0,
    )?)))
}

fn read_i64_le(encoded: &[u8], offset: usize) -> Result<i64, CanonicalStoreError> {
    Ok(i64::from_le_bytes(read_array(encoded, offset)?))
}

fn read_u32_le(encoded: &[u8], offset: usize) -> Result<u32, CanonicalStoreError> {
    Ok(u32::from_le_bytes(read_array(encoded, offset)?))
}

fn read_u32_be(encoded: &[u8], offset: usize) -> Result<u32, CanonicalStoreError> {
    Ok(u32::from_be_bytes(read_array(encoded, offset)?))
}

fn read_u64_le(encoded: &[u8], offset: usize) -> Result<u64, CanonicalStoreError> {
    Ok(u64::from_le_bytes(read_array(encoded, offset)?))
}

fn read_array<const LENGTH: usize>(
    encoded: &[u8],
    offset: usize,
) -> Result<[u8; LENGTH], CanonicalStoreError> {
    encoded
        .get(offset..offset.saturating_add(LENGTH))
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| CanonicalStoreError::publication("version-1 serving value is truncated"))
}
