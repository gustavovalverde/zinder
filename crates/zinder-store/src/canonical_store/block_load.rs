mod codec;
mod fixed_record_sort;
mod ordered_sst;

use std::path::{Path, PathBuf};

use prost::Message;
use rust_rocksdb::{DB, IngestExternalFileOptions, IteratorMode, Options};
use zinder_core::{
    BlockBlobArtifact, BlockFinalNoteCommitmentRoots, BlockHash, BlockHeight, CanonicalBlockFacts,
    CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigest,
    CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    CanonicalBlockReplayEnvelope, CanonicalBlockReplayFormatVersion, ChainTipMetadata,
    CommitmentTreeCheckpoint, CommitmentTreeFrontiers, CompactBlockArtifact, SerializedBytesDigest,
    TransactionBlobArtifact,
};
use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;

use self::{
    codec::{
        BLOCK_HASH_INDEX_RECORD_LEN, BLOCK_HEADER_VALUE_LEN, TRANSACTION_LOCATION_RECORD_LEN,
        decode_block_final_note_commitment_roots, decode_tree_state_checkpoint,
        encode_block_final_note_commitment_roots, encode_block_hash_location, encode_block_header,
        encode_block_position, encode_transaction_location, encode_transaction_position,
        encode_tree_state_checkpoint,
    },
    fixed_record_sort::{FixedRecordSorter, record_capacity},
    ordered_sst::{OrderedSstSet, SstArtifacts},
};

use super::{
    CanonicalStoreBuildError, CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload,
    TREE_STATE_CHECKPOINT_STRIDE,
    block_replay::BLOCK_REPLAY_COLUMN_FAMILY,
    rocksdb::{
        BLOCK_BLOB_COLUMN_FAMILY, BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
        BLOCK_HASH_INDEX_COLUMN_FAMILY, BLOCK_HEADER_COLUMN_FAMILY, COMPACT_BLOCK_COLUMN_FAMILY,
        TRANSACTION_BLOB_COLUMN_FAMILY, TRANSACTION_LOCATION_COLUMN_FAMILY,
        TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
    },
};

pub(super) const CANONICAL_BLOCK_SST_TARGET_LOGICAL_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const REVERSE_INDEX_SORT_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// All source-derived values for one block in a fresh canonical construction.
///
/// This value deliberately does not implement `Clone`: raw transaction and
/// block bytes have one owner and move directly into bounded SST staging.
#[derive(Debug)]
pub struct CanonicalBuildBlock {
    /// Backend-neutral semantic facts used by every later projection.
    pub facts: CanonicalBlockFacts,
    /// Stable version-1 recovery representation of `facts`.
    pub replay_envelope: CanonicalBlockReplayEnvelope,
    /// Wallet protocol bytes derived while the source block is already parsed.
    pub compact_block: CompactBlockArtifact,
    /// Commitment-tree positions after applying this block.
    pub tip_metadata: ChainTipMetadata,
    /// Typed commitment-tree state at the global checkpoint cadence or exact build tip.
    pub tree_state_checkpoint: Option<CommitmentTreeCheckpoint>,
    /// Per-block final roots retained only by the explorer workload.
    pub block_final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
    /// Raw transactions in exact block order.
    pub transaction_blobs: Vec<TransactionBlobArtifact>,
    /// Raw block bytes required by the explorer workload.
    pub block_blob: Option<BlockBlobArtifact>,
}

/// Prepared identity and bounded-load measurements for ingested block families.
///
/// This value proves what was staged and that every staged SST was accepted by
/// `RocksDB`; it is not cache-bypassing persisted readback evidence and cannot
/// by itself publish a canonical store as READY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalBlockLoadEvidence {
    /// First retained block height.
    pub first_height: BlockHeight,
    /// Parent of the first retained block.
    pub first_parent_hash: BlockHash,
    /// First retained block hash.
    pub first_hash: BlockHash,
    /// Last retained block height.
    pub tip_height: BlockHeight,
    /// Last retained block hash.
    pub tip_hash: BlockHash,
    /// Commitment-tree positions after the last retained block.
    pub tip_metadata: ChainTipMetadata,
    /// Number of contiguous source blocks.
    pub block_count: u64,
    /// Number of source transactions.
    pub transaction_count: u64,
    /// Number of height-addressed header rows.
    pub block_header_count: u64,
    /// Number of hash-addressed block-location rows.
    pub block_hash_index_count: u64,
    /// Number of height-addressed semantic replay rows.
    pub block_replay_count: u64,
    /// Number of height-addressed compact block rows.
    pub compact_block_count: u64,
    /// Number of transaction-id-addressed location rows.
    pub transaction_location_count: u64,
    /// Number of position-addressed raw transaction rows.
    pub transaction_blob_count: u64,
    /// Number of height-addressed raw block rows.
    pub block_blob_count: u64,
    /// Number of typed commitment-tree checkpoints, including the history predecessor.
    pub tree_state_checkpoint_count: u64,
    /// Number of height-addressed final note-commitment-root rows.
    pub block_final_note_commitment_roots_count: u64,
    /// Header-family key and value bytes submitted to the SST writers.
    pub block_header_logical_bytes: u64,
    /// Block-hash-index key and value bytes submitted to the SST writers.
    pub block_hash_index_logical_bytes: u64,
    /// Replay-family key and value bytes submitted to the SST writers.
    pub block_replay_logical_bytes: u64,
    /// Compact-block key and value bytes submitted to the SST writers.
    pub compact_block_logical_bytes: u64,
    /// Transaction-location key and value bytes submitted to the SST writers.
    pub transaction_location_logical_bytes: u64,
    /// Transaction-blob key and value bytes submitted to the SST writers.
    pub transaction_blob_logical_bytes: u64,
    /// Block-blob key and value bytes submitted to the SST writers.
    pub block_blob_logical_bytes: u64,
    /// Tree-checkpoint key and value bytes submitted to the SST writers.
    pub tree_state_checkpoint_logical_bytes: u64,
    /// Final-root key and value bytes submitted to the SST writers.
    pub block_final_note_commitment_roots_logical_bytes: u64,
    /// Total key and value bytes submitted to the SST writers.
    pub logical_bytes: u64,
    /// Physical bytes occupied by every staged SST.
    pub sst_file_bytes: u64,
    /// Number of staged SST files.
    pub sst_file_count: u64,
    /// Canonical replay-envelope contract validated for every block.
    pub replay_format_version: CanonicalBlockReplayFormatVersion,
    /// Semantic fact-digest contract validated for every block.
    pub block_digest_version: CanonicalBlockFactsDigestVersion,
    /// Ordered fact-sequence digest contract.
    pub sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion,
    /// Ordered digest of every block's semantic facts.
    pub sequence_digest: CanonicalBlockFactsSequenceDigest,
}

pub(super) struct PreparedCanonicalBlockLoad {
    families: Vec<PreparedColumnFamily>,
    pub(super) evidence: CanonicalBlockLoadEvidence,
}

#[derive(Clone, Copy)]
pub(super) struct CanonicalBlockSstConfig<'options> {
    pub(super) staging_path: &'options Path,
    pub(super) options: &'options Options,
    pub(super) workload: CanonicalStoreWorkload,
    pub(super) build_plan: &'options CanonicalStoreBuildPlan,
    pub(super) sst_target_logical_bytes: u64,
    pub(super) reverse_index_sort_memory_bytes: usize,
}

struct PreparedColumnFamily {
    name: &'static str,
    paths: Vec<PathBuf>,
}

pub(super) fn write_canonical_block_ssts<SourceError>(
    config: CanonicalBlockSstConfig<'_>,
    blocks: impl IntoIterator<Item = Result<CanonicalBuildBlock, SourceError>>,
) -> Result<PreparedCanonicalBlockLoad, CanonicalStoreBuildError<SourceError>> {
    let mut stager = CanonicalBlockSstStager::new(config)?;
    for source_block in blocks {
        let block = source_block.map_err(|source| CanonicalStoreBuildError::Source { source })?;
        stager.stage(block)?;
    }
    Ok(stager.finish()?)
}

struct CanonicalBlockSstStager<'options> {
    config: CanonicalBlockSstConfig<'options>,
    header_writer: OrderedSstSet<'options>,
    replay_writer: OrderedSstSet<'options>,
    compact_writer: OrderedSstSet<'options>,
    transaction_blob_writer: OrderedSstSet<'options>,
    block_blob_writer: OrderedSstSet<'options>,
    tree_state_checkpoint_writer: OrderedSstSet<'options>,
    block_final_note_commitment_roots_writer: OrderedSstSet<'options>,
    block_hash_sorter: FixedRecordSorter<BLOCK_HASH_INDEX_RECORD_LEN>,
    transaction_location_sorter: FixedRecordSorter<TRANSACTION_LOCATION_RECORD_LEN>,
    sequence: Option<BlockSequence>,
    predecessor_checkpoint_logical_bytes: u64,
}

impl<'options> CanonicalBlockSstStager<'options> {
    fn new(config: CanonicalBlockSstConfig<'options>) -> Result<Self, CanonicalStoreError> {
        if config.sst_target_logical_bytes == 0 {
            return Err(CanonicalStoreError::block_load_sequence(
                "SST target logical bytes must be greater than zero",
            ));
        }
        let block_hash_capacity =
            record_capacity::<BLOCK_HASH_INDEX_RECORD_LEN>(config.reverse_index_sort_memory_bytes)?;
        let transaction_location_capacity = record_capacity::<TRANSACTION_LOCATION_RECORD_LEN>(
            config.reverse_index_sort_memory_bytes,
        )?;
        let mut tree_state_checkpoint_writer = config.ordered_writer("tree-state-checkpoint");
        let predecessor = config.build_plan.history_predecessor();
        let predecessor_key = encode_block_position(predecessor.block_id.height);
        let predecessor_value =
            encode_tree_state_checkpoint(predecessor.block_time_seconds, &predecessor.frontiers);
        tree_state_checkpoint_writer.put(&predecessor_key, &predecessor_value)?;
        let predecessor_checkpoint_logical_bytes =
            checked_row_bytes(predecessor_key.len(), predecessor_value.len())?;
        Ok(Self {
            header_writer: config.ordered_writer("block-header"),
            replay_writer: config.ordered_writer("block-replay"),
            compact_writer: config.ordered_writer("compact-block"),
            transaction_blob_writer: config.ordered_writer("transaction-blob"),
            block_blob_writer: config.ordered_writer("block-blob"),
            tree_state_checkpoint_writer,
            block_final_note_commitment_roots_writer: config
                .ordered_writer("block-final-note-commitment-roots"),
            block_hash_sorter: FixedRecordSorter::new(
                config.staging_path,
                "block-hash-index",
                block_hash_capacity,
            ),
            transaction_location_sorter: FixedRecordSorter::new(
                config.staging_path,
                "transaction-location",
                transaction_location_capacity,
            ),
            config,
            sequence: None,
            predecessor_checkpoint_logical_bytes,
        })
    }

    fn stage(&mut self, block: CanonicalBuildBlock) -> Result<(), CanonicalStoreError> {
        validate_build_block(self.config.workload, self.config.build_plan, &block)?;
        let row = BlockSequenceRow::from_block(&block)?;
        match &mut self.sequence {
            Some(sequence) => sequence.append(row)?,
            None => self.sequence = Some(BlockSequence::new(row)?),
        }
        write_build_block(
            block,
            &mut self.header_writer,
            &mut self.replay_writer,
            &mut self.compact_writer,
            &mut self.transaction_blob_writer,
            &mut self.block_blob_writer,
            &mut self.tree_state_checkpoint_writer,
            &mut self.block_final_note_commitment_roots_writer,
            &mut self.block_hash_sorter,
            &mut self.transaction_location_sorter,
        )
    }

    fn finish(self) -> Result<PreparedCanonicalBlockLoad, CanonicalStoreError> {
        let sequence = self.sequence.ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(
                "a canonical block load must contain at least one block",
            )
        })?;
        let artifacts = CanonicalBlockSstArtifacts {
            header: self.header_writer.finish()?,
            block_hash: self
                .block_hash_sorter
                .finish::<32>(self.config.options, self.config.sst_target_logical_bytes)?,
            replay: self.replay_writer.finish()?,
            compact: self.compact_writer.finish()?,
            transaction_location: self
                .transaction_location_sorter
                .finish::<32>(self.config.options, self.config.sst_target_logical_bytes)?,
            transaction_blob: self.transaction_blob_writer.finish()?,
            block_blob: self.block_blob_writer.finish()?,
            tree_state_checkpoint: self.tree_state_checkpoint_writer.finish()?,
            block_final_note_commitment_roots: self
                .block_final_note_commitment_roots_writer
                .finish()?,
        };
        prepare_canonical_block_load(
            sequence,
            artifacts,
            self.predecessor_checkpoint_logical_bytes,
        )
    }
}

impl<'options> CanonicalBlockSstConfig<'options> {
    fn ordered_writer(self, prefix: &'static str) -> OrderedSstSet<'options> {
        OrderedSstSet::new(
            self.staging_path,
            prefix,
            self.options,
            self.sst_target_logical_bytes,
        )
    }
}

struct CanonicalBlockSstArtifacts {
    header: SstArtifacts,
    block_hash: SstArtifacts,
    replay: SstArtifacts,
    compact: SstArtifacts,
    transaction_location: SstArtifacts,
    transaction_blob: SstArtifacts,
    block_blob: SstArtifacts,
    tree_state_checkpoint: SstArtifacts,
    block_final_note_commitment_roots: SstArtifacts,
}

fn prepare_canonical_block_load(
    sequence: BlockSequence,
    artifacts: CanonicalBlockSstArtifacts,
    predecessor_checkpoint_logical_bytes: u64,
) -> Result<PreparedCanonicalBlockLoad, CanonicalStoreError> {
    let families = vec![
        PreparedColumnFamily::new(BLOCK_HEADER_COLUMN_FAMILY, artifacts.header.paths),
        PreparedColumnFamily::new(BLOCK_HASH_INDEX_COLUMN_FAMILY, artifacts.block_hash.paths),
        PreparedColumnFamily::new(BLOCK_REPLAY_COLUMN_FAMILY, artifacts.replay.paths),
        PreparedColumnFamily::new(COMPACT_BLOCK_COLUMN_FAMILY, artifacts.compact.paths),
        PreparedColumnFamily::new(
            TRANSACTION_LOCATION_COLUMN_FAMILY,
            artifacts.transaction_location.paths,
        ),
        PreparedColumnFamily::new(
            TRANSACTION_BLOB_COLUMN_FAMILY,
            artifacts.transaction_blob.paths,
        ),
        PreparedColumnFamily::new(BLOCK_BLOB_COLUMN_FAMILY, artifacts.block_blob.paths),
        PreparedColumnFamily::new(
            TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
            artifacts.tree_state_checkpoint.paths,
        ),
        PreparedColumnFamily::new(
            BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
            artifacts.block_final_note_commitment_roots.paths,
        ),
    ];
    let sst_file_bytes = [
        artifacts.header.file_bytes,
        artifacts.block_hash.file_bytes,
        artifacts.replay.file_bytes,
        artifacts.compact.file_bytes,
        artifacts.transaction_location.file_bytes,
        artifacts.transaction_blob.file_bytes,
        artifacts.block_blob.file_bytes,
        artifacts.tree_state_checkpoint.file_bytes,
        artifacts.block_final_note_commitment_roots.file_bytes,
    ]
    .into_iter()
    .try_fold(0_u64, checked_add_sst_bytes)?;
    let sst_file_count = families.iter().try_fold(0_u64, |count, family| {
        let family_count = u64::try_from(family.paths.len()).map_err(|_| {
            CanonicalStoreError::block_load_sequence("SST file count exceeds u64::MAX")
        })?;
        count.checked_add(family_count).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("SST file count exceeds u64::MAX")
        })
    })?;

    Ok(PreparedCanonicalBlockLoad {
        families,
        evidence: sequence.finish(
            sst_file_bytes,
            sst_file_count,
            predecessor_checkpoint_logical_bytes,
        )?,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "each argument is one independently staged physical block family"
)]
fn write_build_block(
    block: CanonicalBuildBlock,
    header_writer: &mut OrderedSstSet<'_>,
    replay_writer: &mut OrderedSstSet<'_>,
    compact_writer: &mut OrderedSstSet<'_>,
    transaction_blob_writer: &mut OrderedSstSet<'_>,
    block_blob_writer: &mut OrderedSstSet<'_>,
    tree_state_checkpoint_writer: &mut OrderedSstSet<'_>,
    block_final_note_commitment_roots_writer: &mut OrderedSstSet<'_>,
    block_hash_sorter: &mut FixedRecordSorter<BLOCK_HASH_INDEX_RECORD_LEN>,
    transaction_location_sorter: &mut FixedRecordSorter<TRANSACTION_LOCATION_RECORD_LEN>,
) -> Result<(), CanonicalStoreError> {
    let CanonicalBuildBlock {
        facts,
        replay_envelope,
        compact_block,
        transaction_blobs,
        block_blob,
        tree_state_checkpoint,
        block_final_note_commitment_roots,
        ..
    } = block;
    let height = facts.block_header.height;
    let height_key = encode_block_position(height);
    let header_value = encode_block_header(&facts.block_header);
    header_writer.put(&height_key, &header_value)?;
    replay_writer.put(&height_key, replay_envelope.as_bytes())?;
    compact_writer.put(&height_key, &compact_block.payload_bytes)?;

    let block_hash_record = encode_block_hash_location(facts.block_header.block_hash, height);
    block_hash_sorter.push(block_hash_record)?;

    for (transaction_index, transaction_blob) in transaction_blobs.into_iter().enumerate() {
        let transaction_index = u32::try_from(transaction_index).map_err(|_| {
            CanonicalStoreError::block_load_sequence(format!(
                "block {} transaction count exceeds u32::MAX",
                height.value()
            ))
        })?;
        let transaction_key = encode_transaction_position(height, transaction_index);
        transaction_blob_writer.put(&transaction_key, &transaction_blob.raw_transaction_bytes)?;

        let location_record = encode_transaction_location(transaction_blob.location);
        transaction_location_sorter.push(location_record)?;
    }

    if let Some(block_blob) = block_blob {
        block_blob_writer.put(&height_key, &block_blob.raw_block_bytes)?;
    }
    if let Some(checkpoint) = tree_state_checkpoint {
        let checkpoint_value =
            encode_tree_state_checkpoint(checkpoint.block_time_seconds, &checkpoint.frontiers);
        tree_state_checkpoint_writer.put(&height_key, &checkpoint_value)?;
    }
    if let Some(roots) = block_final_note_commitment_roots {
        let roots_value = encode_block_final_note_commitment_roots(&roots);
        block_final_note_commitment_roots_writer.put(&height_key, &roots_value)?;
    }
    Ok(())
}

pub(super) fn ingest_canonical_block_ssts(
    db: &DB,
    prepared: PreparedCanonicalBlockLoad,
) -> Result<CanonicalBlockLoadEvidence, CanonicalStoreError> {
    for family in prepared.families {
        if family.paths.is_empty() {
            continue;
        }
        let column_family = db.cf_handle(family.name).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(format!(
                "{} column family is absent",
                family.name
            ))
        })?;
        let mut options = IngestExternalFileOptions::default();
        options.set_move_files(true);
        options.set_snapshot_consistency(true);
        options.set_allow_global_seqno(false);
        options.set_allow_blocking_flush(false);
        db.ingest_external_file_cf_opts(&column_family, &options, family.paths)
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical block-family external SST ingestion",
                source,
            })?;
    }
    Ok(prepared.evidence)
}

pub(super) fn canonical_block_families_are_empty(db: &DB) -> Result<bool, CanonicalStoreError> {
    for name in [
        BLOCK_HEADER_COLUMN_FAMILY,
        BLOCK_HASH_INDEX_COLUMN_FAMILY,
        BLOCK_REPLAY_COLUMN_FAMILY,
        COMPACT_BLOCK_COLUMN_FAMILY,
        TRANSACTION_LOCATION_COLUMN_FAMILY,
        TRANSACTION_BLOB_COLUMN_FAMILY,
        BLOCK_BLOB_COLUMN_FAMILY,
        TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
        BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
    ] {
        let column_family = db.cf_handle(name).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(format!("{name} column family is absent"))
        })?;
        let mut iterator = db.raw_iterator_cf(&column_family);
        iterator.seek_to_first();
        if iterator.valid() {
            return Ok(false);
        }
        iterator
            .status()
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical block-family empty-state validation",
                source,
            })?;
    }
    Ok(true)
}

pub(super) fn validate_persisted_commitment_tree_families(
    db: &DB,
    workload: CanonicalStoreWorkload,
    build_plan: &CanonicalStoreBuildPlan,
    evidence: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    validate_persisted_tree_state_checkpoints(db, build_plan, evidence)?;
    validate_persisted_block_final_note_commitment_roots(db, workload, build_plan, evidence)
}

fn validate_persisted_tree_state_checkpoints(
    db: &DB,
    build_plan: &CanonicalStoreBuildPlan,
    evidence: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    let family = db
        .cf_handle(TREE_STATE_CHECKPOINT_COLUMN_FAMILY)
        .ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(
                "tree_state_checkpoint column family is absent",
            )
        })?;
    let predecessor = build_plan.history_predecessor();
    let mut previous_height: Option<BlockHeight> = None;
    let mut row_count = 0_u64;
    let mut tip_frontiers = None;
    for row in db.iterator_cf(&family, IteratorMode::Start) {
        let (key, encoded_checkpoint) =
            row.map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "tree-state checkpoint persisted readback",
                source,
            })?;
        let height = zinder_core::wire::decode_height_key_ascending(&key).map_err(|source| {
            CanonicalStoreError::block_load_sequence(format!(
                "tree_state_checkpoint has an invalid height key: {source}"
            ))
        })?;
        let (block_time_seconds, frontiers) = decode_tree_state_checkpoint(&encoded_checkpoint)
            .map_err(|source| {
                CanonicalStoreError::block_load_sequence(format!(
                    "tree_state_checkpoint at height {} is invalid: {source}",
                    height.value()
                ))
            })?;
        match previous_height {
            None if height != predecessor.block_id.height
                || block_time_seconds != predecessor.block_time_seconds
                || frontiers != predecessor.frontiers =>
            {
                return Err(CanonicalStoreError::block_load_sequence(
                    "first persisted tree-state checkpoint does not match the history predecessor",
                ));
            }
            Some(previous) => {
                let gap = height
                    .value()
                    .checked_sub(previous.value())
                    .ok_or_else(|| {
                        CanonicalStoreError::block_load_sequence(
                            "persisted tree-state checkpoint heights do not increase",
                        )
                    })?;
                if gap == 0 || gap > TREE_STATE_CHECKPOINT_STRIDE {
                    return Err(CanonicalStoreError::block_load_sequence(format!(
                        "persisted tree-state checkpoint gap {gap} exceeds {TREE_STATE_CHECKPOINT_STRIDE} blocks"
                    )));
                }
                let checkpoint_expected =
                    tree_state_checkpoint_required(height, build_plan.build_tip().height);
                if !checkpoint_expected {
                    return Err(CanonicalStoreError::block_load_sequence(format!(
                        "persisted tree-state checkpoint at height {} is outside the global cadence and build tip",
                        height.value()
                    )));
                }
            }
            None => {}
        }
        if height == build_plan.build_tip().height {
            tip_frontiers = Some(frontiers.clone());
        }
        previous_height = Some(height);
        row_count = row_count.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(
                "persisted tree-state checkpoint count exceeds u64::MAX",
            )
        })?;
    }
    validate_tree_state_checkpoint_readback(
        row_count,
        previous_height,
        tip_frontiers.as_ref(),
        build_plan,
        evidence,
    )
}

fn validate_tree_state_checkpoint_readback(
    row_count: u64,
    last_height: Option<BlockHeight>,
    tip_frontiers: Option<&CommitmentTreeFrontiers>,
    build_plan: &CanonicalStoreBuildPlan,
    evidence: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    if row_count == evidence.tree_state_checkpoint_count
        && last_height == Some(build_plan.build_tip().height)
        && tip_frontiers.map(CommitmentTreeFrontiers::tip_metadata) == Some(evidence.tip_metadata)
    {
        return Ok(());
    }
    Err(CanonicalStoreError::BlockLoadReadbackMismatch)
}

fn validate_persisted_block_final_note_commitment_roots(
    db: &DB,
    workload: CanonicalStoreWorkload,
    build_plan: &CanonicalStoreBuildPlan,
    evidence: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    let family = db
        .cf_handle(BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY)
        .ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(
                "block_final_note_commitment_roots column family is absent",
            )
        })?;
    let mut row_count = 0_u64;
    for row in db.iterator_cf(&family, IteratorMode::Start) {
        let (key, encoded_roots) = row.map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "block final note-commitment roots persisted readback",
            source,
        })?;
        let height = zinder_core::wire::decode_height_key_ascending(&key).map_err(|source| {
            CanonicalStoreError::block_load_sequence(format!(
                "block_final_note_commitment_roots has an invalid height key: {source}"
            ))
        })?;
        if height < build_plan.history_bounds().first_available_height()
            || height > build_plan.build_tip().height
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block_final_note_commitment_roots height {} is outside retained history",
                height.value()
            )));
        }
        decode_block_final_note_commitment_roots(
            height,
            BlockHash::from_bytes([0; 32]),
            &encoded_roots,
        )
        .map_err(|source| {
            CanonicalStoreError::block_load_sequence(format!(
                "block_final_note_commitment_roots at height {} is invalid: {source}",
                height.value()
            ))
        })?;
        row_count = row_count.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence(
                "persisted block final note-commitment roots count exceeds u64::MAX",
            )
        })?;
    }
    if row_count != evidence.block_final_note_commitment_roots_count
        || (workload == CanonicalStoreWorkload::Wallet && row_count != 0)
    {
        return Err(CanonicalStoreError::BlockLoadReadbackMismatch);
    }
    Ok(())
}

fn validate_build_block(
    workload: CanonicalStoreWorkload,
    build_plan: &CanonicalStoreBuildPlan,
    block: &CanonicalBuildBlock,
) -> Result<(), CanonicalStoreError> {
    let header = &block.facts.block_header;
    let height = header.height;
    if block.replay_envelope.format_version() != CanonicalBlockReplayFormatVersion::V1
        || block.replay_envelope.reference_digest().version()
            != CanonicalBlockFactsDigestVersion::V1
    {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} does not use the version-1 replay and fact-digest contracts",
            height.value()
        )));
    }
    if block.replay_envelope.block_height() != height
        || block.replay_envelope.block_hash() != header.block_hash
        || block.replay_envelope.parent_hash() != header.parent_hash
        || block.replay_envelope.reference_digest()
            != block.facts.digest(CanonicalBlockFactsDigestVersion::V1)
    {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} replay envelope does not match its canonical facts",
            height.value()
        )));
    }
    if block.compact_block.height != height || block.compact_block.block_hash != header.block_hash {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} compact artifact does not match its canonical facts",
            height.value()
        )));
    }
    validate_compact_block_payload(block)?;
    validate_transaction_blobs(block)?;
    validate_block_blob(workload, block)?;
    validate_tree_state_checkpoint(build_plan, block)?;
    validate_block_final_note_commitment_roots(workload, block)
}

fn validate_compact_block_payload(block: &CanonicalBuildBlock) -> Result<(), CanonicalStoreError> {
    let header = &block.facts.block_header;
    let compact = LightwalletdCompactBlock::decode(block.compact_block.payload_bytes.as_slice())
        .map_err(|_| {
            CanonicalStoreError::block_load_sequence(format!(
                "block {} compact payload is not valid protobuf",
                header.height.value()
            ))
        })?;
    let metadata = compact.chain_metadata.ok_or_else(|| {
        CanonicalStoreError::block_load_sequence(format!(
            "block {} compact payload has no chain metadata",
            header.height.value()
        ))
    })?;
    if compact.height != u64::from(header.height.value())
        || compact.hash != header.block_hash.as_bytes()
        || compact.prev_hash != header.parent_hash.as_bytes()
        || metadata.sapling_commitment_tree_size != block.tip_metadata.sapling_commitment_tree_size
        || metadata.orchard_commitment_tree_size != block.tip_metadata.orchard_commitment_tree_size
        || metadata.ironwood_commitment_tree_size
            != block.tip_metadata.ironwood_commitment_tree_size
    {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} compact payload identity or tree positions do not match canonical facts",
            header.height.value()
        )));
    }
    Ok(())
}

fn validate_transaction_blobs(block: &CanonicalBuildBlock) -> Result<(), CanonicalStoreError> {
    let height = block.facts.block_header.height;
    if block.transaction_blobs.len() != block.facts.transactions.len() {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} has {} facts but {} raw transaction blobs",
            height.value(),
            block.facts.transactions.len(),
            block.transaction_blobs.len()
        )));
    }
    for (transaction_index, (transaction, blob)) in block
        .facts
        .transactions
        .iter()
        .zip(&block.transaction_blobs)
        .enumerate()
    {
        let transaction_index = u32::try_from(transaction_index).map_err(|_| {
            CanonicalStoreError::block_load_sequence(format!(
                "block {} transaction count exceeds u32::MAX",
                height.value()
            ))
        })?;
        let location = blob.location;
        if location.transaction_id != transaction.public_facts.transaction_id
            || location.block_height != height
            || location.block_hash != block.facts.block_header.block_hash
            || location.tx_index_in_block != transaction_index
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} transaction {transaction_index} raw blob has the wrong location",
                height.value()
            )));
        }
        if SerializedBytesDigest::from_serialized_bytes(&blob.raw_transaction_bytes)
            != transaction.serialized_bytes_digest
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} transaction {transaction_index} raw bytes do not match canonical facts",
                height.value()
            )));
        }
    }
    Ok(())
}

fn validate_block_blob(
    workload: CanonicalStoreWorkload,
    block: &CanonicalBuildBlock,
) -> Result<(), CanonicalStoreError> {
    let header = &block.facts.block_header;
    let height = header.height;
    match (workload, &block.block_blob) {
        (CanonicalStoreWorkload::Wallet, Some(_)) => {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "wallet block {} unexpectedly contains an explorer raw block",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Explorer, None) => {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "explorer block {} is missing its raw block",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Explorer, Some(blob))
            if blob.height != height
                || blob.block_hash != header.block_hash
                || blob.parent_hash != header.parent_hash =>
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "explorer block {} raw block identity does not match its canonical facts",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Explorer, Some(blob))
            if SerializedBytesDigest::from_serialized_bytes(&blob.raw_block_bytes)
                != block.facts.serialized_bytes_digest =>
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "explorer block {} raw bytes do not match canonical facts",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Wallet, None) | (CanonicalStoreWorkload::Explorer, Some(_)) => {}
    }
    Ok(())
}

fn validate_tree_state_checkpoint(
    build_plan: &CanonicalStoreBuildPlan,
    block: &CanonicalBuildBlock,
) -> Result<(), CanonicalStoreError> {
    let header = &block.facts.block_header;
    let height = header.height;
    let checkpoint_required = tree_state_checkpoint_required(height, build_plan.build_tip().height);
    match (checkpoint_required, &block.tree_state_checkpoint) {
        (true, None) => {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} is missing its required tree-state checkpoint",
                height.value()
            )));
        }
        (false, Some(_)) => {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} has a tree-state checkpoint outside the global cadence and build tip",
                height.value()
            )));
        }
        (true, Some(checkpoint))
            if checkpoint.block_id.height != height
                || checkpoint.block_id.hash != header.block_hash
                || i64::from(checkpoint.block_time_seconds) != header.block_time
                || checkpoint.tip_metadata() != block.tip_metadata =>
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} tree-state checkpoint does not match its canonical identity and tree positions",
                height.value()
            )));
        }
        (true, Some(_)) | (false, None) => {}
    }
    Ok(())
}

fn tree_state_checkpoint_required(height: BlockHeight, build_tip_height: BlockHeight) -> bool {
    height == build_tip_height || height.value().is_multiple_of(TREE_STATE_CHECKPOINT_STRIDE)
}

fn validate_block_final_note_commitment_roots(
    workload: CanonicalStoreWorkload,
    block: &CanonicalBuildBlock,
) -> Result<(), CanonicalStoreError> {
    let header = &block.facts.block_header;
    let height = header.height;
    match (workload, block.block_final_note_commitment_roots) {
        (CanonicalStoreWorkload::Wallet, Some(_)) => {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "wallet block {} unexpectedly contains explorer final note-commitment roots",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Explorer, Some(roots))
            if roots.height != height || roots.block_hash != header.block_hash =>
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "explorer block {} final note-commitment roots do not match its canonical identity",
                height.value()
            )));
        }
        (CanonicalStoreWorkload::Wallet, None)
        | (CanonicalStoreWorkload::Explorer, None | Some(_)) => {}
    }
    Ok(())
}

struct BlockSequence {
    first_height: BlockHeight,
    first_parent_hash: BlockHash,
    first_hash: BlockHash,
    tip_height: BlockHeight,
    tip_hash: BlockHash,
    tip_metadata: ChainTipMetadata,
    block_count: u64,
    transaction_count: u64,
    block_blob_count: u64,
    tree_state_checkpoint_count: u64,
    block_final_note_commitment_roots_count: u64,
    family_logical_bytes: BlockFamilyLogicalBytes,
    sequence_digest: CanonicalBlockFactsSequenceDigestBuilder,
}

#[derive(Clone, Copy)]
struct BlockSequenceRow {
    height: BlockHeight,
    block_hash: BlockHash,
    parent_hash: BlockHash,
    tip_metadata: ChainTipMetadata,
    transaction_count: u64,
    has_block_blob: bool,
    has_tree_state_checkpoint: bool,
    has_block_final_note_commitment_roots: bool,
    family_logical_bytes: BlockFamilyLogicalBytes,
    facts_digest: zinder_core::CanonicalBlockFactsDigest,
}

#[derive(Clone, Copy)]
struct BlockFamilyLogicalBytes {
    block_header: u64,
    block_hash_index: u64,
    block_replay: u64,
    compact_block: u64,
    transaction_location: u64,
    transaction_blob: u64,
    block_blob: u64,
    tree_state_checkpoint: u64,
    block_final_note_commitment_roots: u64,
}

impl BlockSequenceRow {
    fn from_block(block: &CanonicalBuildBlock) -> Result<Self, CanonicalStoreError> {
        let transaction_count = u64::try_from(block.facts.transactions.len()).map_err(|_| {
            CanonicalStoreError::block_load_sequence("transaction count exceeds u64::MAX")
        })?;
        let mut transaction_location_logical_bytes = 0;
        let mut transaction_blob_logical_bytes = 0;
        for transaction_blob in &block.transaction_blobs {
            transaction_blob_logical_bytes = checked_add_row_bytes(
                transaction_blob_logical_bytes,
                8,
                transaction_blob.raw_transaction_bytes.len(),
            )?;
            transaction_location_logical_bytes =
                checked_add_row_bytes(transaction_location_logical_bytes, 32, 40)?;
        }
        let block_blob_logical_bytes = block.block_blob.as_ref().map_or(Ok(0), |block_blob| {
            checked_row_bytes(4, block_blob.raw_block_bytes.len())
        })?;
        let tree_state_checkpoint_logical_bytes =
            block
                .tree_state_checkpoint
                .as_ref()
                .map_or(Ok(0), |checkpoint| {
                    checked_row_bytes(
                        4,
                        encode_tree_state_checkpoint(
                            checkpoint.block_time_seconds,
                            &checkpoint.frontiers,
                        )
                        .len(),
                    )
                })?;
        let block_final_note_commitment_roots_logical_bytes = block
            .block_final_note_commitment_roots
            .as_ref()
            .map_or(Ok(0), |roots| {
                checked_row_bytes(4, encode_block_final_note_commitment_roots(roots).len())
            })?;
        Ok(Self {
            height: block.facts.block_header.height,
            block_hash: block.facts.block_header.block_hash,
            parent_hash: block.facts.block_header.parent_hash,
            tip_metadata: block.tip_metadata,
            transaction_count,
            has_block_blob: block.block_blob.is_some(),
            has_tree_state_checkpoint: block.tree_state_checkpoint.is_some(),
            has_block_final_note_commitment_roots: block
                .block_final_note_commitment_roots
                .is_some(),
            family_logical_bytes: BlockFamilyLogicalBytes {
                block_header: checked_row_bytes(4, BLOCK_HEADER_VALUE_LEN)?,
                block_hash_index: checked_row_bytes(32, 4)?,
                block_replay: checked_row_bytes(4, block.replay_envelope.as_bytes().len())?,
                compact_block: checked_row_bytes(4, block.compact_block.payload_bytes.len())?,
                transaction_location: transaction_location_logical_bytes,
                transaction_blob: transaction_blob_logical_bytes,
                block_blob: block_blob_logical_bytes,
                tree_state_checkpoint: tree_state_checkpoint_logical_bytes,
                block_final_note_commitment_roots: block_final_note_commitment_roots_logical_bytes,
            },
            facts_digest: block.facts.digest(CanonicalBlockFactsDigestVersion::V1),
        })
    }
}

impl BlockFamilyLogicalBytes {
    fn checked_add(self, row: Self) -> Result<Self, CanonicalStoreError> {
        Ok(Self {
            block_header: checked_add_logical_bytes(self.block_header, row.block_header)?,
            block_hash_index: checked_add_logical_bytes(
                self.block_hash_index,
                row.block_hash_index,
            )?,
            block_replay: checked_add_logical_bytes(self.block_replay, row.block_replay)?,
            compact_block: checked_add_logical_bytes(self.compact_block, row.compact_block)?,
            transaction_location: checked_add_logical_bytes(
                self.transaction_location,
                row.transaction_location,
            )?,
            transaction_blob: checked_add_logical_bytes(
                self.transaction_blob,
                row.transaction_blob,
            )?,
            block_blob: checked_add_logical_bytes(self.block_blob, row.block_blob)?,
            tree_state_checkpoint: checked_add_logical_bytes(
                self.tree_state_checkpoint,
                row.tree_state_checkpoint,
            )?,
            block_final_note_commitment_roots: checked_add_logical_bytes(
                self.block_final_note_commitment_roots,
                row.block_final_note_commitment_roots,
            )?,
        })
    }

    fn checked_sum(self) -> Result<u64, CanonicalStoreError> {
        [
            self.block_header,
            self.block_hash_index,
            self.block_replay,
            self.compact_block,
            self.transaction_location,
            self.transaction_blob,
            self.block_blob,
            self.tree_state_checkpoint,
            self.block_final_note_commitment_roots,
        ]
        .into_iter()
        .try_fold(0_u64, checked_add_logical_bytes)
    }
}

impl BlockSequence {
    fn new(row: BlockSequenceRow) -> Result<Self, CanonicalStoreError> {
        let mut sequence_digest = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::V1,
        );
        sequence_digest.try_append(row.facts_digest).map_err(|_| {
            CanonicalStoreError::block_load_sequence("block sequence count exceeds u64::MAX")
        })?;
        Ok(Self {
            first_height: row.height,
            first_parent_hash: row.parent_hash,
            first_hash: row.block_hash,
            tip_height: row.height,
            tip_hash: row.block_hash,
            tip_metadata: row.tip_metadata,
            block_count: 1,
            transaction_count: row.transaction_count,
            block_blob_count: u64::from(row.has_block_blob),
            tree_state_checkpoint_count: u64::from(row.has_tree_state_checkpoint),
            block_final_note_commitment_roots_count: u64::from(
                row.has_block_final_note_commitment_roots,
            ),
            family_logical_bytes: row.family_logical_bytes,
            sequence_digest,
        })
    }

    fn append(&mut self, row: BlockSequenceRow) -> Result<(), CanonicalStoreError> {
        if self.tip_height.next() != Some(row.height) {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "height {} does not immediately follow height {}",
                row.height.value(),
                self.tip_height.value()
            )));
        }
        if row.parent_hash != self.tip_hash {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} parent does not match the preceding block",
                row.height.value()
            )));
        }
        if row.tip_metadata.sapling_commitment_tree_size
            < self.tip_metadata.sapling_commitment_tree_size
            || row.tip_metadata.orchard_commitment_tree_size
                < self.tip_metadata.orchard_commitment_tree_size
            || row.tip_metadata.ironwood_commitment_tree_size
                < self.tip_metadata.ironwood_commitment_tree_size
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "block {} commitment-tree positions move backwards",
                row.height.value()
            )));
        }
        self.block_count = self.block_count.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("block count exceeds u64::MAX")
        })?;
        self.transaction_count = self
            .transaction_count
            .checked_add(row.transaction_count)
            .ok_or_else(|| {
                CanonicalStoreError::block_load_sequence("transaction count exceeds u64::MAX")
            })?;
        self.block_blob_count = self
            .block_blob_count
            .checked_add(u64::from(row.has_block_blob))
            .ok_or_else(|| {
                CanonicalStoreError::block_load_sequence("block blob count exceeds u64::MAX")
            })?;
        self.tree_state_checkpoint_count = self
            .tree_state_checkpoint_count
            .checked_add(u64::from(row.has_tree_state_checkpoint))
            .ok_or_else(|| {
                CanonicalStoreError::block_load_sequence(
                    "tree-state checkpoint count exceeds u64::MAX",
                )
            })?;
        self.block_final_note_commitment_roots_count = self
            .block_final_note_commitment_roots_count
            .checked_add(u64::from(row.has_block_final_note_commitment_roots))
            .ok_or_else(|| {
                CanonicalStoreError::block_load_sequence(
                    "block final note-commitment roots count exceeds u64::MAX",
                )
            })?;
        self.family_logical_bytes = self
            .family_logical_bytes
            .checked_add(row.family_logical_bytes)?;
        self.sequence_digest
            .try_append(row.facts_digest)
            .map_err(|_| {
                CanonicalStoreError::block_load_sequence("block sequence count exceeds u64::MAX")
            })?;
        self.tip_height = row.height;
        self.tip_hash = row.block_hash;
        self.tip_metadata = row.tip_metadata;
        Ok(())
    }

    fn finish(
        self,
        sst_file_bytes: u64,
        sst_file_count: u64,
        predecessor_checkpoint_logical_bytes: u64,
    ) -> Result<CanonicalBlockLoadEvidence, CanonicalStoreError> {
        let mut family_logical_bytes = self.family_logical_bytes;
        family_logical_bytes.tree_state_checkpoint = checked_add_logical_bytes(
            family_logical_bytes.tree_state_checkpoint,
            predecessor_checkpoint_logical_bytes,
        )?;
        let logical_bytes = family_logical_bytes.checked_sum()?;
        Ok(CanonicalBlockLoadEvidence {
            first_height: self.first_height,
            first_parent_hash: self.first_parent_hash,
            first_hash: self.first_hash,
            tip_height: self.tip_height,
            tip_hash: self.tip_hash,
            tip_metadata: self.tip_metadata,
            block_count: self.block_count,
            transaction_count: self.transaction_count,
            block_header_count: self.block_count,
            block_hash_index_count: self.block_count,
            block_replay_count: self.block_count,
            compact_block_count: self.block_count,
            transaction_location_count: self.transaction_count,
            transaction_blob_count: self.transaction_count,
            block_blob_count: self.block_blob_count,
            tree_state_checkpoint_count: self
                .tree_state_checkpoint_count
                .checked_add(1)
                .ok_or_else(|| {
                    CanonicalStoreError::block_load_sequence(
                        "tree-state checkpoint count exceeds u64::MAX",
                    )
                })?,
            block_final_note_commitment_roots_count: self.block_final_note_commitment_roots_count,
            block_header_logical_bytes: family_logical_bytes.block_header,
            block_hash_index_logical_bytes: family_logical_bytes.block_hash_index,
            block_replay_logical_bytes: family_logical_bytes.block_replay,
            compact_block_logical_bytes: family_logical_bytes.compact_block,
            transaction_location_logical_bytes: family_logical_bytes.transaction_location,
            transaction_blob_logical_bytes: family_logical_bytes.transaction_blob,
            block_blob_logical_bytes: family_logical_bytes.block_blob,
            tree_state_checkpoint_logical_bytes: family_logical_bytes.tree_state_checkpoint,
            block_final_note_commitment_roots_logical_bytes: family_logical_bytes
                .block_final_note_commitment_roots,
            logical_bytes,
            sst_file_bytes,
            sst_file_count,
            replay_format_version: CanonicalBlockReplayFormatVersion::V1,
            block_digest_version: CanonicalBlockFactsDigestVersion::V1,
            sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::V1,
            sequence_digest: self.sequence_digest.finish(),
        })
    }
}

impl PreparedColumnFamily {
    fn new(name: &'static str, paths: Vec<PathBuf>) -> Self {
        Self { name, paths }
    }
}

fn checked_row_bytes(key_len: usize, value_len: usize) -> Result<u64, CanonicalStoreError> {
    let row_len = key_len.checked_add(value_len).ok_or_else(|| {
        CanonicalStoreError::block_load_sequence("logical row byte count exceeds usize::MAX")
    })?;
    u64::try_from(row_len).map_err(|_| {
        CanonicalStoreError::block_load_sequence("logical row byte count exceeds u64::MAX")
    })
}

fn checked_add_row_bytes(
    total: u64,
    key_len: usize,
    value_len: usize,
) -> Result<u64, CanonicalStoreError> {
    total
        .checked_add(checked_row_bytes(key_len, value_len)?)
        .ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("logical byte count exceeds u64::MAX")
        })
}

fn checked_add_sst_bytes(total: u64, file_bytes: u64) -> Result<u64, CanonicalStoreError> {
    total.checked_add(file_bytes).ok_or_else(|| {
        CanonicalStoreError::block_load_sequence("physical SST byte count exceeds u64::MAX")
    })
}

fn checked_add_logical_bytes(total: u64, bytes: u64) -> Result<u64, CanonicalStoreError> {
    total.checked_add(bytes).ok_or_else(|| {
        CanonicalStoreError::block_load_sequence("logical byte count exceeds u64::MAX")
    })
}
