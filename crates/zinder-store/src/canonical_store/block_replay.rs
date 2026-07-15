use std::{fs, path::PathBuf, sync::Arc};

use rust_rocksdb::{
    BoundColumnFamily, DB, IngestExternalFileOptions, Options, ReadOptions, SstFileWriter,
};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigest, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayEnvelope,
    CanonicalBlockReplayFormatVersion, decode_canonical_block_replay,
    wire::{decode_height_key_ascending, encode_height_key_ascending},
};

use super::{CanonicalStoreBuildError, CanonicalStoreError};

pub(super) const BLOCK_REPLAY_COLUMN_FAMILY: &str = "block_replay";
pub(super) const BLOCK_REPLAY_SST_TARGET_LOGICAL_BYTES: u64 = 256 * 1024 * 1024;

/// Persisted identity and measurements of one complete canonical replay load.
///
/// This evidence is returned only after the ingested column family has been
/// decoded and compared with the ordered input. It does not publish the whole
/// canonical store as ready; every other required family must be completed and
/// validated first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CanonicalBlockReplayLoadEvidence {
    /// First retained block height.
    pub(super) first_height: BlockHeight,
    /// Parent of the first retained block.
    pub(super) first_parent_hash: BlockHash,
    /// First retained block hash.
    pub(super) first_hash: BlockHash,
    /// Last retained block height.
    pub(super) tip_height: BlockHeight,
    /// Last retained block hash.
    pub(super) tip_hash: BlockHash,
    /// Number of contiguous replay rows.
    pub(super) block_count: u64,
    /// Complete semantic replay-envelope bytes.
    pub(super) logical_replay_bytes: u64,
    /// Physical bytes of every ingested SST file.
    pub(super) sst_file_bytes: u64,
    /// Number of bounded SST files ingested in one atomic call.
    pub(super) sst_file_count: u64,
    /// Canonical replay-envelope contract validated for every row.
    pub(super) replay_format_version: CanonicalBlockReplayFormatVersion,
    /// Semantic block-facts digest contract validated for every row.
    pub(super) block_digest_version: CanonicalBlockFactsDigestVersion,
    /// Version of the ordered sequence digest.
    pub(super) sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion,
    /// Ordered digest of every replay row's semantic fact digest.
    pub(super) sequence_digest: CanonicalBlockFactsSequenceDigest,
}

pub(super) struct PreparedBlockReplayLoad {
    pub(super) external_sst_paths: Vec<PathBuf>,
    pub(super) evidence: CanonicalBlockReplayLoadEvidence,
}

pub(super) fn write_block_replay_ssts_with_target<SourceError>(
    staging_path: &std::path::Path,
    options: &Options,
    sst_target_logical_bytes: u64,
    replay_envelopes: impl IntoIterator<Item = Result<CanonicalBlockReplayEnvelope, SourceError>>,
) -> Result<PreparedBlockReplayLoad, CanonicalStoreBuildError<SourceError>> {
    let mut replay_envelopes = replay_envelopes.into_iter();
    let first_replay = match replay_envelopes.next() {
        Some(Ok(replay)) => replay,
        Some(Err(source)) => return Err(CanonicalStoreBuildError::Source { source }),
        None => {
            return Err(CanonicalStoreError::block_replay_sequence(
                "a canonical replay load must contain at least one row",
            )
            .into());
        }
    };

    let mut sequence = ReplaySequence::from_envelope(&first_replay)?;
    let mut external_sst_paths = Vec::new();
    let mut sst_file_bytes = 0_u64;
    let mut sst_index = 0_u64;
    let mut current_path = replay_sst_path(staging_path, sst_index);
    let mut writer = open_sst_writer(options, &current_path)?;
    write_replay(&mut writer, &first_replay)?;
    let mut current_sst_logical_bytes = checked_replay_len(&first_replay)?;

    for replay in replay_envelopes {
        let replay = replay.map_err(|source| CanonicalStoreBuildError::Source { source })?;
        sequence.append_envelope(&replay)?;
        if current_sst_logical_bytes >= sst_target_logical_bytes {
            sst_file_bytes = finish_sst(writer, &current_path, sst_file_bytes)?;
            external_sst_paths.push(current_path);
            sst_index = sst_index.checked_add(1).ok_or_else(|| {
                CanonicalStoreError::block_replay_sequence("SST file count exceeds u64::MAX")
            })?;
            current_path = replay_sst_path(staging_path, sst_index);
            writer = open_sst_writer(options, &current_path)?;
            current_sst_logical_bytes = 0;
        }
        write_replay(&mut writer, &replay)?;
        current_sst_logical_bytes = current_sst_logical_bytes
            .checked_add(checked_replay_len(&replay)?)
            .ok_or_else(|| {
                CanonicalStoreError::block_replay_sequence(
                    "current SST logical byte count exceeds u64::MAX",
                )
            })?;
    }

    sst_file_bytes = finish_sst(writer, &current_path, sst_file_bytes)?;
    external_sst_paths.push(current_path);
    let sst_file_count = u64::try_from(external_sst_paths.len()).map_err(|_| {
        CanonicalStoreError::block_replay_sequence("SST file count exceeds u64::MAX")
    })?;
    Ok(PreparedBlockReplayLoad {
        external_sst_paths,
        evidence: sequence.finish(sst_file_bytes, sst_file_count),
    })
}

pub(super) fn ingest_block_replay_ssts(
    db: &DB,
    external_sst_paths: Vec<PathBuf>,
) -> Result<(), CanonicalStoreError> {
    if external_sst_paths.is_empty() {
        return Err(CanonicalStoreError::block_replay_sequence(
            "external SST ingestion requires at least one file",
        ));
    }
    let block_replay = block_replay_column_family(db)?;
    let mut options = IngestExternalFileOptions::default();
    options.set_move_files(true);
    options.set_snapshot_consistency(true);
    options.set_allow_global_seqno(false);
    options.set_allow_blocking_flush(false);
    db.ingest_external_file_cf_opts(&block_replay, &options, external_sst_paths)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay external SST ingestion",
            source,
        })
}

pub(super) fn block_replay_is_empty(db: &DB) -> Result<bool, CanonicalStoreError> {
    let block_replay = block_replay_column_family(db)?;
    let mut iterator = db.raw_iterator_cf(&block_replay);
    iterator.seek_to_first();
    if iterator.valid() {
        return Ok(false);
    }
    iterator
        .status()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay empty-state validation",
            source,
        })?;
    Ok(true)
}

pub(super) fn validate_persisted_block_replays(
    db: &DB,
    sst_file_bytes: u64,
) -> Result<CanonicalBlockReplayLoadEvidence, CanonicalStoreError> {
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
        let height = decode_height_key_ascending(key).map_err(|source| {
            CanonicalStoreError::BlockReplayKeyInvalid {
                reason: source.to_string(),
            }
        })?;
        let replay = decode_canonical_block_replay(encoded_replay).map_err(|source| {
            CanonicalStoreError::block_replay_invalid(height, source.to_string())
        })?;
        let facts = replay.facts();
        if facts.block_header.height != height {
            return Err(CanonicalStoreError::block_replay_invalid(
                height,
                format!("row contains height {}", facts.block_header.height.value()),
            ));
        }
        validate_replay_versions(replay.format_version(), replay.reference_digest())?;
        let replay_bytes = u64::try_from(encoded_replay.len()).map_err(|_| {
            CanonicalStoreError::block_replay_sequence("replay byte length exceeds u64::MAX")
        })?;
        match &mut sequence {
            None => {
                sequence = Some(ReplaySequence::new(ReplaySequenceRow {
                    height,
                    block_hash: facts.block_header.block_hash,
                    parent_hash: facts.block_header.parent_hash,
                    reference_digest: replay.reference_digest(),
                    replay_bytes,
                })?);
            }
            Some(sequence) => sequence.append(ReplaySequenceRow {
                height,
                block_hash: facts.block_header.block_hash,
                parent_hash: facts.block_header.parent_hash,
                reference_digest: replay.reference_digest(),
                replay_bytes,
            })?,
        }
        iterator.next();
    }
    iterator
        .status()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay readback iteration",
            source,
        })?;
    sequence
        .map(|sequence| sequence.finish(sst_file_bytes, 0))
        .ok_or_else(|| {
            CanonicalStoreError::block_replay_sequence(
                "persisted block replay family must not be empty",
            )
        })
}

fn block_replay_column_family(db: &DB) -> Result<Arc<BoundColumnFamily<'_>>, CanonicalStoreError> {
    db.cf_handle(BLOCK_REPLAY_COLUMN_FAMILY).ok_or_else(|| {
        CanonicalStoreError::block_replay_sequence("block_replay column family is absent")
    })
}

fn replay_sst_path(staging_path: &std::path::Path, index: u64) -> PathBuf {
    staging_path.join(format!("block-replay-{index:08}.sst"))
}

fn open_sst_writer<'options>(
    options: &'options Options,
    path: &std::path::Path,
) -> Result<SstFileWriter<'options>, CanonicalStoreError> {
    let writer = SstFileWriter::create(options);
    writer
        .open(path)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay SST open",
            source,
        })?;
    Ok(writer)
}

fn finish_sst(
    mut writer: SstFileWriter<'_>,
    path: &std::path::Path,
    total_sst_file_bytes: u64,
) -> Result<u64, CanonicalStoreError> {
    writer
        .finish()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay SST finish",
            source,
        })?;
    let file_bytes = fs::metadata(path)
        .map_err(|source| CanonicalStoreError::PathUnavailable {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    total_sst_file_bytes.checked_add(file_bytes).ok_or_else(|| {
        CanonicalStoreError::block_replay_sequence("physical SST byte count exceeds u64::MAX")
    })
}

fn write_replay(
    writer: &mut SstFileWriter<'_>,
    replay: &CanonicalBlockReplayEnvelope,
) -> Result<(), CanonicalStoreError> {
    writer
        .put(
            encode_height_key_ascending(replay.block_height()),
            replay.as_bytes(),
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block replay SST write",
            source,
        })
}

fn checked_replay_len(replay: &CanonicalBlockReplayEnvelope) -> Result<u64, CanonicalStoreError> {
    u64::try_from(replay.as_bytes().len()).map_err(|_| {
        CanonicalStoreError::block_replay_sequence("replay byte length exceeds u64::MAX")
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

impl ReplaySequenceRow {
    fn from_envelope(replay: &CanonicalBlockReplayEnvelope) -> Result<Self, CanonicalStoreError> {
        validate_replay_versions(replay.format_version(), replay.reference_digest())?;
        Ok(Self {
            height: replay.block_height(),
            block_hash: replay.block_hash(),
            parent_hash: replay.parent_hash(),
            reference_digest: replay.reference_digest(),
            replay_bytes: checked_replay_len(replay)?,
        })
    }
}

impl ReplaySequence {
    fn from_envelope(replay: &CanonicalBlockReplayEnvelope) -> Result<Self, CanonicalStoreError> {
        Self::new(ReplaySequenceRow::from_envelope(replay)?)
    }

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

    fn append_envelope(
        &mut self,
        replay: &CanonicalBlockReplayEnvelope,
    ) -> Result<(), CanonicalStoreError> {
        self.append(ReplaySequenceRow::from_envelope(replay)?)
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

    fn finish(self, sst_file_bytes: u64, sst_file_count: u64) -> CanonicalBlockReplayLoadEvidence {
        CanonicalBlockReplayLoadEvidence {
            first_height: self.first_height,
            first_parent_hash: self.first_parent_hash,
            first_hash: self.first_hash,
            tip_height: self.tip_height,
            tip_hash: self.tip_hash,
            block_count: self.block_count,
            logical_replay_bytes: self.logical_replay_bytes,
            sst_file_bytes,
            sst_file_count,
            replay_format_version: CanonicalBlockReplayFormatVersion::V1,
            block_digest_version: CanonicalBlockFactsDigestVersion::V1,
            sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::V1,
            sequence_digest: self.digest_builder.finish(),
        }
    }
}

impl CanonicalBlockReplayLoadEvidence {
    pub(super) fn has_same_sequence(self, other: Self) -> bool {
        self.first_height == other.first_height
            && self.first_parent_hash == other.first_parent_hash
            && self.first_hash == other.first_hash
            && self.tip_height == other.tip_height
            && self.tip_hash == other.tip_hash
            && self.block_count == other.block_count
            && self.logical_replay_bytes == other.logical_replay_bytes
            && self.replay_format_version == other.replay_format_version
            && self.block_digest_version == other.block_digest_version
            && self.sequence_digest_version == other.sequence_digest_version
            && self.sequence_digest == other.sequence_digest
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
