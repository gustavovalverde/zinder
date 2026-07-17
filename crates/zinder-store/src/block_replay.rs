//! Canonical block semantic replay reads.

use std::{collections::BTreeMap, num::NonZeroU32};

use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, ValidatedCanonicalBlockReplay,
    decode_canonical_block_replay,
};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::decode_visible_source_epoch,
    format::StoreKey,
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Maximum number of block replay rows returned by one batch read.
pub const MAX_BLOCK_REPLAY_BATCH_BLOCKS: NonZeroU32 = NonZeroU32::MIN.saturating_add(255);

/// Bounded, forward block replay read request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockReplayBatchRequest {
    /// First canonical height to return.
    pub start_height: BlockHeight,
    /// Maximum number of consecutive replay rows to return.
    pub max_blocks: NonZeroU32,
}

impl BlockReplayBatchRequest {
    /// Creates a bounded block replay read request.
    #[must_use]
    pub const fn new(start_height: BlockHeight, max_blocks: NonZeroU32) -> Self {
        Self {
            start_height,
            max_blocks,
        }
    }

    pub(crate) fn ensure_within_limit(self) -> Result<(), StoreError> {
        if self.max_blocks <= MAX_BLOCK_REPLAY_BATCH_BLOCKS {
            return Ok(());
        }

        Err(StoreError::ArtifactRangeTooLarge {
            family: ArtifactFamily::BlockReplay,
            requested_block_count: self.max_blocks,
            maximum_block_count: MAX_BLOCK_REPLAY_BATCH_BLOCKS,
        })
    }
}

/// Epoch-bound read boundary for durable canonical block replay.
pub trait BlockReplayStore {
    /// Reads and semantically validates one replay envelope at one canonical height.
    fn block_replay_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<ValidatedCanonicalBlockReplay>, StoreError>;

    /// Reads a bounded, ascending batch of semantically validated replay envelopes.
    ///
    /// The store rejects requests above
    /// [`MAX_BLOCK_REPLAY_BATCH_BLOCKS`]. A request beginning after the
    /// pinned visible tip returns an empty batch; a request crossing the tip is
    /// clipped to it. Missing or corrupt rows fail the whole read.
    ///
    /// The batch resolves all source epochs with one ordered visibility-index
    /// scan, then fetches all replay payloads with one `multi_get`.
    fn block_replay_batch(
        &self,
        request: BlockReplayBatchRequest,
    ) -> Result<Vec<ValidatedCanonicalBlockReplay>, StoreError>;
}

pub(crate) fn read_block_replay(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<ValidatedCanonicalBlockReplay>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    metrics::counter!(
        "zinder_store_visibility_seek_total",
        "artifact_family" => ArtifactFamily::BlockReplay.wire_label()
    )
    .increment(1);
    let visibility_prefix = StoreKey::visible_block_epoch_prefix(chain_epoch.network, height);
    let visibility_seek_key =
        StoreKey::visible_block_epoch(chain_epoch.network, height, chain_epoch.id);
    let Some((visibility_key, source_epoch_bytes)) = inner.get_previous_by_prefix(
        StorageTable::ReorgWindow,
        &visibility_prefix,
        &visibility_seek_key,
    )?
    else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::BlockReplay,
            key: visibility_seek_key.into(),
        });
    };
    let replay_key = block_replay_key_from_visibility_row(
        chain_epoch,
        height,
        &visibility_key,
        &source_epoch_bytes,
    )?;
    let replay_envelope_bytes = inner
        .get(StorageTable::BlockReplay, &replay_key)?
        .ok_or_else(|| StoreError::ArtifactMissing {
            family: ArtifactFamily::BlockReplay,
            key: replay_key.clone().into(),
        })?;

    decode_and_validate_block_replay(&replay_key, &replay_envelope_bytes, height).map(Some)
}

pub(crate) fn read_block_replay_batch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    request: BlockReplayBatchRequest,
) -> Result<Vec<ValidatedCanonicalBlockReplay>, StoreError> {
    request.ensure_within_limit()?;
    if request.start_height > chain_epoch.visible_tip_height {
        return Ok(Vec::new());
    }

    let requested_end_height = BlockHeight::new(
        request
            .start_height
            .value()
            .saturating_add(request.max_blocks.get().saturating_sub(1)),
    );
    let end_height = requested_end_height.min(chain_epoch.visible_tip_height);
    let replay_keys = read_block_replay_keys(
        inner,
        chain_epoch,
        BlockHeightRange::inclusive(request.start_height, end_height),
    )?;
    let store_keys = replay_keys
        .iter()
        .map(|(_, replay_key)| replay_key.clone())
        .collect::<Vec<_>>();
    let replay_values = inner.multi_get(StorageTable::BlockReplay, &store_keys)?;

    let mut replays = Vec::with_capacity(replay_keys.len());
    for (index, (height, replay_key)) in replay_keys.into_iter().enumerate() {
        let replay_envelope_bytes = replay_values
            .get(index)
            .and_then(Option::as_deref)
            .ok_or_else(|| StoreError::ArtifactMissing {
                family: ArtifactFamily::BlockReplay,
                key: replay_key.clone().into(),
            })?;
        replays.push(decode_and_validate_block_replay(
            &replay_key,
            replay_envelope_bytes,
            height,
        )?);
    }

    Ok(replays)
}

fn read_block_replay_keys(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<(BlockHeight, StoreKey)>, StoreError> {
    metrics::counter!(
        "zinder_store_visibility_scan_total",
        "artifact_family" => ArtifactFamily::BlockReplay.wire_label()
    )
    .increment(1);

    let start_prefix = StoreKey::visible_block_epoch_prefix(chain_epoch.network, block_range.start);
    let end_exclusive = StoreKey::visible_block_epoch_range_end_exclusive(
        chain_epoch.network,
        block_range.end,
        chain_epoch.id,
    );
    let mut selected_visibility_rows = BTreeMap::new();
    inner.scan_forward_range(
        StorageTable::ReorgWindow,
        &(&start_prefix..&end_exclusive),
        &mut |key_bytes, source_epoch_bytes| {
            let key = StoreKey::from_raw_bytes(key_bytes);
            let Some((network, height, publication_epoch)) =
                StoreKey::visible_block_epoch_key_parts(key_bytes)
            else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::BlockReplay,
                    key: key.into(),
                    reason: "visible block epoch key is malformed",
                });
            };
            if network != chain_epoch.network
                || height < block_range.start
                || height > block_range.end
            {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::BlockReplay,
                    key: key.into(),
                    reason: "visible block epoch key is outside the requested range",
                });
            }
            if publication_epoch <= chain_epoch.id {
                selected_visibility_rows.insert(height, (key, source_epoch_bytes.to_vec()));
            }
            Ok(PrefixScanControl::Continue)
        },
    )?;

    let mut replay_keys = Vec::with_capacity(block_range.into_iter().len());
    for height in block_range {
        let seek_key = StoreKey::visible_block_epoch(chain_epoch.network, height, chain_epoch.id);
        let Some((visibility_key, source_epoch_bytes)) = selected_visibility_rows.remove(&height)
        else {
            return Err(StoreError::ArtifactMissing {
                family: ArtifactFamily::BlockReplay,
                key: seek_key.into(),
            });
        };
        replay_keys.push((
            height,
            block_replay_key_from_visibility_row(
                chain_epoch,
                height,
                &visibility_key,
                &source_epoch_bytes,
            )?,
        ));
    }

    Ok(replay_keys)
}

fn block_replay_key_from_visibility_row(
    chain_epoch: ChainEpoch,
    expected_height: BlockHeight,
    visibility_key: &StoreKey,
    source_epoch_bytes: &[u8],
) -> Result<StoreKey, StoreError> {
    let Some((network, height, publication_epoch)) =
        StoreKey::visible_block_epoch_key_parts(visibility_key.as_bytes())
    else {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockReplay,
            key: visibility_key.clone().into(),
            reason: "visible block epoch key is malformed",
        });
    };
    if network != chain_epoch.network
        || height != expected_height
        || publication_epoch > chain_epoch.id
    {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockReplay,
            key: visibility_key.clone().into(),
            reason: "visible block epoch key does not match the requested snapshot position",
        });
    }
    let source_epoch = decode_visible_source_epoch(
        ArtifactFamily::BlockReplay,
        visibility_key,
        source_epoch_bytes,
    )?;

    Ok(StoreKey::block_replay(
        chain_epoch.network,
        source_epoch,
        expected_height,
    ))
}

fn decode_and_validate_block_replay(
    key: &StoreKey,
    replay_envelope_bytes: &[u8],
    requested_height: BlockHeight,
) -> Result<ValidatedCanonicalBlockReplay, StoreError> {
    let replay = decode_canonical_block_replay(replay_envelope_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockReplay,
            key: key.clone().into(),
            reason: "block replay envelope failed semantic validation",
        }
    })?;
    if replay.facts().block_header.height != requested_height {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockReplay,
            key: key.clone().into(),
            reason: "block replay header height does not match the requested height",
        });
    }

    Ok(replay)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap};

    use zinder_core::{
        BlockHash, BlockHeaderArtifact, CanonicalBlockFacts, CanonicalBlockFactsDigestVersion,
        CanonicalBlockReplayFormatVersion, ChainEpochId, ChainTipMetadata, Network,
        SerializedBytesDigest, UnixTimestampMillis, encode_canonical_block_replay,
    };

    use super::*;
    use crate::CURRENT_ARTIFACT_SCHEMA_VERSION;

    struct ReplayBatchReadProbe {
        visibility_rows: Vec<(Vec<u8>, Vec<u8>)>,
        replay_rows: BTreeMap<Vec<u8>, Vec<u8>>,
        point_get_count: Cell<u32>,
        multi_get_count: Cell<u32>,
        sorted_multi_get_count: Cell<u32>,
        predecessor_seek_count: Cell<u32>,
        range_scan_count: Cell<u32>,
    }

    impl ReplayBatchReadProbe {
        fn new(chain_epoch: ChainEpoch) -> Self {
            let first_height = BlockHeight::new(1);
            let second_height = BlockHeight::new(2);
            let first_epoch = ChainEpochId::new(1);
            let second_epoch = ChainEpochId::new(2);
            let visibility_rows = vec![
                (
                    StoreKey::visible_block_epoch(chain_epoch.network, first_height, first_epoch)
                        .into_bytes(),
                    first_epoch.value().to_be_bytes().to_vec(),
                ),
                (
                    StoreKey::visible_block_epoch(chain_epoch.network, second_height, first_epoch)
                        .into_bytes(),
                    first_epoch.value().to_be_bytes().to_vec(),
                ),
                (
                    StoreKey::visible_block_epoch(chain_epoch.network, second_height, second_epoch)
                        .into_bytes(),
                    second_epoch.value().to_be_bytes().to_vec(),
                ),
            ];
            let replay_rows = [
                (first_height, first_epoch, BlockHash::from_bytes([1; 32])),
                (second_height, second_epoch, BlockHash::from_bytes([20; 32])),
            ]
            .into_iter()
            .map(|(height, source_epoch, block_hash)| {
                let header = replay_header(height, block_hash);
                (
                    StoreKey::block_replay(chain_epoch.network, source_epoch, height).into_bytes(),
                    encode_canonical_block_replay(
                        &CanonicalBlockFacts {
                            block_header: header,
                            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
                                &[],
                            ),
                            transactions: Vec::new(),
                        },
                        CanonicalBlockReplayFormatVersion::CURRENT,
                        CanonicalBlockFactsDigestVersion::CURRENT,
                    )
                    .as_bytes()
                    .to_vec(),
                )
            })
            .collect();

            Self {
                visibility_rows,
                replay_rows,
                point_get_count: Cell::new(0),
                multi_get_count: Cell::new(0),
                sorted_multi_get_count: Cell::new(0),
                predecessor_seek_count: Cell::new(0),
                range_scan_count: Cell::new(0),
            }
        }

        fn unexpected_read<T>() -> Result<T, StoreError> {
            Err(StoreError::InvalidChainEpochArtifacts {
                reason: "unexpected replay batch test read primitive",
            })
        }
    }

    impl RocksChainStoreRead for ReplayBatchReadProbe {
        fn get(
            &self,
            _table: StorageTable,
            _key: &StoreKey,
        ) -> Result<Option<Vec<u8>>, StoreError> {
            self.point_get_count
                .set(self.point_get_count.get().saturating_add(1));
            Self::unexpected_read()
        }

        fn multi_get(
            &self,
            _table: StorageTable,
            keys: &[StoreKey],
        ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
            self.multi_get_count
                .set(self.multi_get_count.get().saturating_add(1));
            Ok(keys
                .iter()
                .map(|key| self.replay_rows.get(key.as_bytes()).cloned())
                .collect())
        }

        fn sorted_multi_get(
            &self,
            _table: StorageTable,
            _keys: &[StoreKey],
        ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
            self.sorted_multi_get_count
                .set(self.sorted_multi_get_count.get().saturating_add(1));
            Self::unexpected_read()
        }

        fn get_previous_by_prefix(
            &self,
            _table: StorageTable,
            _prefix: &StoreKey,
            _seek_key: &StoreKey,
        ) -> Result<Option<(StoreKey, Vec<u8>)>, StoreError> {
            self.predecessor_seek_count
                .set(self.predecessor_seek_count.get().saturating_add(1));
            Self::unexpected_read()
        }

        fn scan_prefix(
            &self,
            _table: StorageTable,
            _prefix: &StoreKey,
            _visit: &mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>,
        ) -> Result<(), StoreError> {
            Self::unexpected_read()
        }

        fn scan_prefix_reverse(
            &self,
            _table: StorageTable,
            _prefix: &StoreKey,
            _visit: &mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>,
        ) -> Result<(), StoreError> {
            Self::unexpected_read()
        }

        fn scan_prefix_reverse_before(
            &self,
            _table: StorageTable,
            _prefix: &StoreKey,
            _before: &StoreKey,
            _visit: &mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>,
        ) -> Result<(), StoreError> {
            Self::unexpected_read()
        }

        fn scan_forward(
            &self,
            _table: StorageTable,
            _start_key: &StoreKey,
            _visit: &mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>,
        ) -> Result<(), StoreError> {
            Self::unexpected_read()
        }

        fn scan_forward_range(
            &self,
            _table: StorageTable,
            key_range: &std::ops::Range<&StoreKey>,
            visit: &mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>,
        ) -> Result<(), StoreError> {
            let start_inclusive = key_range.start;
            let end_exclusive = key_range.end;
            self.range_scan_count
                .set(self.range_scan_count.get().saturating_add(1));
            for (key, value) in &self.visibility_rows {
                if key.as_slice() < start_inclusive.as_bytes()
                    || key.as_slice() >= end_exclusive.as_bytes()
                {
                    continue;
                }
                if matches!(visit(key, value)?, PrefixScanControl::Stop) {
                    break;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn batch_uses_one_visibility_scan_and_one_unsorted_payload_fetch() -> Result<(), StoreError> {
        let chain_epoch = replay_chain_epoch();
        let read_probe = ReplayBatchReadProbe::new(chain_epoch);

        let replays = read_block_replay_batch(
            &read_probe,
            chain_epoch,
            BlockReplayBatchRequest::new(BlockHeight::new(1), NonZeroU32::MIN.saturating_add(1)),
        )?;

        assert_eq!(
            replays
                .iter()
                .map(|replay| replay.facts().block_header.block_hash)
                .collect::<Vec<_>>(),
            vec![
                BlockHash::from_bytes([1; 32]),
                BlockHash::from_bytes([20; 32])
            ]
        );
        assert_eq!(read_probe.range_scan_count.get(), 1);
        assert_eq!(read_probe.multi_get_count.get(), 1);
        assert_eq!(read_probe.sorted_multi_get_count.get(), 0);
        assert_eq!(read_probe.predecessor_seek_count.get(), 0);
        assert_eq!(read_probe.point_get_count.get(), 0);
        Ok(())
    }

    fn replay_chain_epoch() -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(2),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(2),
            visible_tip_hash: BlockHash::from_bytes([20; 32]),
            settled_tip_height: BlockHeight::new(1),
            settled_tip_hash: BlockHash::from_bytes([1; 32]),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_000_002),
        }
    }

    fn replay_header(height: BlockHeight, block_hash: BlockHash) -> BlockHeaderArtifact {
        let parent_hash_byte = u8::try_from(height.value().saturating_sub(1)).unwrap_or(u8::MAX);
        BlockHeaderArtifact::new(
            height,
            block_hash,
            BlockHash::from_bytes([parent_hash_byte; 32]),
            [0x01; 32],
            [0x02; 32],
            i64::from(height.value()),
            0x1d00_ffff,
            [0x03; 32],
            4,
            128,
        )
    }
}
