use std::sync::Arc;

use rust_rocksdb::{BoundColumnFamily, DB, DBRawIteratorWithThreadMode, ReadOptions};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigest, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion,
    ValidatedCanonicalBlockReplay, decode_canonical_block_replay,
    wire::{decode_height_key_ascending, encode_height_key_ascending},
};

use super::{CanonicalBlockLoadEvidence, CanonicalStoreError, CanonicalStoreReadyEvidence};

pub(super) const BLOCK_REPLAY_COLUMN_FAMILY: &str = "block_replay";

/// One cache-bypassing, forward scan of the READY canonical replay family.
///
/// Every yielded envelope is decoded and semantically validated. Reaching the
/// end also authenticates the exact first block, tip, row count, parent links,
/// and ordered sequence digest recorded by canonical publication.
pub struct CanonicalReplayScan<'a> {
    iterator: DBRawIteratorWithThreadMode<'a, DB>,
    ready_evidence: CanonicalStoreReadyEvidence,
    previous_block: Option<(BlockHeight, BlockHash)>,
    observed_block_count: u64,
    sequence_digest_builder: Option<CanonicalBlockFactsSequenceDigestBuilder>,
    finished: bool,
}

impl<'a> CanonicalReplayScan<'a> {
    pub(super) fn new(
        db: &'a DB,
        ready_evidence: CanonicalStoreReadyEvidence,
    ) -> Result<Self, CanonicalStoreError> {
        let block_replay = block_replay_column_family(db)?;
        let mut read_options = ReadOptions::default();
        read_options.fill_cache(false);
        read_options.set_readahead_size(2 * 1024 * 1024);
        let mut iterator = db.raw_iterator_cf_opt(&block_replay, read_options);
        iterator.seek(encode_height_key_ascending(
            ready_evidence.first_retained_block.height,
        ));
        Ok(Self {
            iterator,
            ready_evidence,
            previous_block: None,
            observed_block_count: 0,
            sequence_digest_builder: Some(CanonicalBlockFactsSequenceDigestBuilder::new(
                CanonicalBlockFactsSequenceDigestVersion::V1,
            )),
            finished: false,
        })
    }

    fn finish_scan(&mut self) -> Result<(), CanonicalStoreError> {
        self.iterator
            .status()
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical replay scan",
                source,
            })?;
        if self.observed_block_count != self.ready_evidence.baseline_block_count {
            return Err(CanonicalStoreError::block_replay_sequence(format!(
                "READY records {} baseline blocks but replay scan observed {}",
                self.ready_evidence.baseline_block_count, self.observed_block_count
            )));
        }
        let observed_tip = self.previous_block;
        let expected_tip = (
            self.ready_evidence.visible_tip.height,
            self.ready_evidence.visible_tip.hash,
        );
        if observed_tip != Some(expected_tip) {
            return Err(CanonicalStoreError::block_replay_sequence(
                "replay scan tip does not match READY",
            ));
        }
        let digest_builder = self.sequence_digest_builder.take().ok_or_else(|| {
            CanonicalStoreError::block_replay_sequence(
                "canonical replay scan sequence digest was already finalized",
            )
        })?;
        if digest_builder.finish().as_bytes() != self.ready_evidence.baseline_sequence_digest {
            return Err(CanonicalStoreError::block_replay_sequence(
                "replay scan sequence digest does not match READY",
            ));
        }
        Ok(())
    }

    fn validate_replay_position(
        &self,
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
    ) -> Result<(), CanonicalStoreError> {
        match self.previous_block {
            None if height == self.ready_evidence.first_retained_block.height
                && block_hash == self.ready_evidence.first_retained_block.hash =>
            {
                Ok(())
            }
            Some((previous_height, previous_hash))
                if previous_height.next() == Some(height) && parent_hash == previous_hash =>
            {
                Ok(())
            }
            None | Some(_) => Err(CanonicalStoreError::block_replay_sequence(format!(
                "replay scan observed an unexpected block at height {}",
                height.value()
            ))),
        }
    }
}

impl Iterator for CanonicalReplayScan<'_> {
    type Item = Result<ValidatedCanonicalBlockReplay, CanonicalStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.iterator.valid() {
            self.finished = true;
            return self.finish_scan().err().map(Err);
        }
        let decoded_replay = self.iterator.item().map_or_else(
            || {
                Err(CanonicalStoreError::block_replay_sequence(
                    "canonical replay iterator was valid without a row",
                ))
            },
            |(key, encoded_replay)| decode_persisted_replay(key, encoded_replay),
        );
        let replay = (|| {
            let (height, replay) = decoded_replay?;
            let facts = replay.facts();
            self.validate_replay_position(
                height,
                facts.block_header.block_hash,
                facts.block_header.parent_hash,
            )?;
            self.observed_block_count =
                self.observed_block_count.checked_add(1).ok_or_else(|| {
                    CanonicalStoreError::block_replay_sequence("replay scan count exceeds u64::MAX")
                })?;
            if self.observed_block_count > self.ready_evidence.baseline_block_count {
                return Err(CanonicalStoreError::block_replay_sequence(
                    "replay scan contains rows beyond the published baseline",
                ));
            }
            let digest_builder = self.sequence_digest_builder.as_mut().ok_or_else(|| {
                CanonicalStoreError::block_replay_sequence(
                    "canonical replay scan sequence digest was finalized early",
                )
            })?;
            digest_builder
                .try_append(replay.reference_digest())
                .map_err(|source| CanonicalStoreError::block_replay_sequence(source.to_string()))?;
            self.previous_block = Some((height, facts.block_header.block_hash));
            Ok(replay)
        })();
        self.iterator.next();
        if replay.is_err() {
            self.finished = true;
        }
        Some(replay)
    }
}

/// Cache-bypassing semantic proof of the persisted replay family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PersistedBlockReplayEvidence {
    pub(super) first_height: BlockHeight,
    pub(super) first_parent_hash: BlockHash,
    pub(super) first_hash: BlockHash,
    pub(super) tip_height: BlockHeight,
    pub(super) tip_hash: BlockHash,
    pub(super) block_count: u64,
    pub(super) logical_replay_bytes: u64,
    pub(super) replay_format_version: CanonicalBlockReplayFormatVersion,
    pub(super) block_digest_version: CanonicalBlockFactsDigestVersion,
    pub(super) sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion,
    pub(super) sequence_digest: CanonicalBlockFactsSequenceDigest,
}

/// Decodes every persisted replay row without populating the block cache.
pub(super) fn validate_persisted_block_replays(
    db: &DB,
) -> Result<PersistedBlockReplayEvidence, CanonicalStoreError> {
    let block_replay = block_replay_column_family(db)?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    read_options.set_readahead_size(2 * 1024 * 1024);
    let mut iterator = db.raw_iterator_cf_opt(&block_replay, read_options);
    iterator.seek_to_first();
    let mut sequence = None;
    while iterator.valid() {
        let Some((key, encoded_replay)) = iterator.item() else {
            iterator
                .status()
                .map_err(|source| CanonicalStoreError::RocksDbOperation {
                    operation: "block replay readback iteration",
                    source,
                })?;
            break;
        };
        let (height, replay) = decode_persisted_replay(key, encoded_replay)?;
        let facts = replay.facts();
        let replay_bytes = u64::try_from(encoded_replay.len()).map_err(|_| {
            CanonicalStoreError::block_replay_sequence("replay byte length exceeds u64::MAX")
        })?;
        let row = ReplaySequenceRow {
            height,
            block_hash: facts.block_header.block_hash,
            parent_hash: facts.block_header.parent_hash,
            reference_digest: replay.reference_digest(),
            replay_bytes,
        };
        match &mut sequence {
            None => sequence = Some(ReplaySequence::new(row)?),
            Some(sequence) => sequence.append(row)?,
        }
        iterator.next();
    }
    iterator
        .status()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay readback iteration",
            source,
        })?;
    sequence.map(ReplaySequence::finish).ok_or_else(|| {
        CanonicalStoreError::block_replay_sequence(
            "persisted block replay family must not be empty",
        )
    })
}

fn decode_persisted_replay(
    key: &[u8],
    encoded_replay: &[u8],
) -> Result<(BlockHeight, ValidatedCanonicalBlockReplay), CanonicalStoreError> {
    let height = decode_height_key_ascending(key).map_err(|source| {
        CanonicalStoreError::BlockReplayKeyInvalid {
            reason: source.to_string(),
        }
    })?;
    let replay = decode_canonical_block_replay(encoded_replay)
        .map_err(|source| CanonicalStoreError::block_replay_invalid(height, source.to_string()))?;
    validate_replay_versions(replay.format_version(), replay.reference_digest())?;
    if replay.facts().block_header.height != height {
        return Err(CanonicalStoreError::block_replay_invalid(
            height,
            format!(
                "row contains height {}",
                replay.facts().block_header.height.value()
            ),
        ));
    }
    Ok((height, replay))
}

impl PersistedBlockReplayEvidence {
    pub(super) fn has_same_sequence(self, prepared: &CanonicalBlockLoadEvidence) -> bool {
        let replay_counts_match = self.block_count == prepared.block_count
            && prepared.block_count == prepared.block_replay_count;
        self.first_height == prepared.first_height
            && self.first_parent_hash == prepared.first_parent_hash
            && self.first_hash == prepared.first_hash
            && self.tip_height == prepared.tip_height
            && self.tip_hash == prepared.tip_hash
            && replay_counts_match
            && self.replay_format_version == prepared.replay_format_version
            && self.block_digest_version == prepared.block_digest_version
            && self.sequence_digest_version == prepared.sequence_digest_version
            && self.sequence_digest == prepared.sequence_digest
    }
}

fn block_replay_column_family(db: &DB) -> Result<Arc<BoundColumnFamily<'_>>, CanonicalStoreError> {
    db.cf_handle(BLOCK_REPLAY_COLUMN_FAMILY).ok_or_else(|| {
        CanonicalStoreError::block_replay_sequence("block_replay column family is absent")
    })
}

struct ReplaySequence {
    first_height: BlockHeight,
    first_parent_hash: BlockHash,
    first_hash: BlockHash,
    tip_height: BlockHeight,
    tip_hash: BlockHash,
    block_count: u64,
    logical_replay_bytes: u64,
    digest_builder: CanonicalBlockFactsSequenceDigestBuilder,
}

#[derive(Clone, Copy)]
struct ReplaySequenceRow {
    height: BlockHeight,
    block_hash: BlockHash,
    parent_hash: BlockHash,
    reference_digest: CanonicalBlockFactsDigest,
    replay_bytes: u64,
}

impl ReplaySequence {
    fn new(row: ReplaySequenceRow) -> Result<Self, CanonicalStoreError> {
        let mut digest_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::V1,
        );
        digest_builder
            .try_append(row.reference_digest)
            .map_err(|source| CanonicalStoreError::block_replay_sequence(source.to_string()))?;
        Ok(Self {
            first_height: row.height,
            first_parent_hash: row.parent_hash,
            first_hash: row.block_hash,
            tip_height: row.height,
            tip_hash: row.block_hash,
            block_count: 1,
            logical_replay_bytes: row.replay_bytes,
            digest_builder,
        })
    }

    fn append(&mut self, row: ReplaySequenceRow) -> Result<(), CanonicalStoreError> {
        let expected_height = self.tip_height.next().ok_or_else(|| {
            CanonicalStoreError::block_replay_sequence("block height overflow after u32::MAX")
        })?;
        if row.height != expected_height {
            return Err(CanonicalStoreError::block_replay_sequence(format!(
                "expected height {}, observed {}",
                expected_height.value(),
                row.height.value()
            )));
        }
        if row.parent_hash != self.tip_hash {
            return Err(CanonicalStoreError::block_replay_sequence(format!(
                "block {} parent does not match the preceding block hash",
                row.height.value()
            )));
        }
        self.block_count = self.block_count.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_replay_sequence("block count exceeds u64::MAX")
        })?;
        self.logical_replay_bytes = self
            .logical_replay_bytes
            .checked_add(row.replay_bytes)
            .ok_or_else(|| {
                CanonicalStoreError::block_replay_sequence(
                    "logical replay byte count exceeds u64::MAX",
                )
            })?;
        self.digest_builder
            .try_append(row.reference_digest)
            .map_err(|source| CanonicalStoreError::block_replay_sequence(source.to_string()))?;
        self.tip_height = row.height;
        self.tip_hash = row.block_hash;
        Ok(())
    }

    fn finish(self) -> PersistedBlockReplayEvidence {
        PersistedBlockReplayEvidence {
            first_height: self.first_height,
            first_parent_hash: self.first_parent_hash,
            first_hash: self.first_hash,
            tip_height: self.tip_height,
            tip_hash: self.tip_hash,
            block_count: self.block_count,
            logical_replay_bytes: self.logical_replay_bytes,
            replay_format_version: CanonicalBlockReplayFormatVersion::V1,
            block_digest_version: CanonicalBlockFactsDigestVersion::V1,
            sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::V1,
            sequence_digest: self.digest_builder.finish(),
        }
    }
}

fn validate_replay_versions(
    format_version: CanonicalBlockReplayFormatVersion,
    reference_digest: CanonicalBlockFactsDigest,
) -> Result<(), CanonicalStoreError> {
    if format_version != CanonicalBlockReplayFormatVersion::V1
        || reference_digest.version() != CanonicalBlockFactsDigestVersion::V1
    {
        return Err(CanonicalStoreError::block_replay_sequence(
            "replay and semantic digest contracts must both be version 1",
        ));
    }
    Ok(())
}
