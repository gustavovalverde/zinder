use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};

use rust_rocksdb::{DB, IteratorMode, ReadOptions, WriteBatch};
use sha2::{Digest, Sha256};
use zinder_core::{
    BlockHash, BlockHeight, ChainTipMetadata, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange,
};

use super::{
    CanonicalBlockLoadEvidence, CanonicalStoreBuildPlan, CanonicalStoreError,
    block_load::{BLOCK_HEADER_VALUE_LEN, encode_block_position},
    construction_manifest::CanonicalColdFamilyEvidence,
    rocksdb::{BLOCK_HEADER_COLUMN_FAMILY, SUBTREE_ROOT_COLUMN_FAMILY},
};

const SUBTREE_ROOT_KEY_LEN: usize = 1 + 4;
const SUBTREE_ROOT_VALUE_LEN: usize = 32 + 4 + 32;
const SUBTREE_ROOT_SEQUENCE_DIGEST_DOMAIN: &[u8] = b"zinder.canonical.subtree-root-sequence.v1\0";
type EncodedSubtreeRoot = ([u8; SUBTREE_ROOT_KEY_LEN], [u8; SUBTREE_ROOT_VALUE_LEN]);

/// Source-authenticated subtree-root fields before canonical block identity is attached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalBuildSubtreeRoot {
    /// Shielded protocol containing the completed subtree.
    pub protocol: ShieldedProtocol,
    /// Exact completed subtree index.
    pub subtree_index: SubtreeRootIndex,
    /// Root returned by the canonical node source.
    pub root_hash: SubtreeRootHash,
    /// Retained canonical block that completed the subtree.
    pub completing_block_height: BlockHeight,
}

/// Measurements from one complete canonical subtree-root load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalSubtreeRootLoadEvidence {
    /// Number of contiguous source-authenticated subtree roots.
    pub subtree_root_count: u64,
    /// Total key and value bytes written to the subtree-root family.
    pub subtree_root_logical_bytes: u64,
    /// Version-1 SHA-256 digest of every exact persisted key and value in order.
    pub subtree_root_sequence_digest: [u8; 32],
}

pub(super) struct PersistedSubtreeRootValidation {
    pub(super) evidence: CanonicalSubtreeRootLoadEvidence,
    pub(super) family_evidence: CanonicalColdFamilyEvidence,
}

impl PersistedSubtreeRootValidation {
    fn complete(
        evidence: CanonicalSubtreeRootLoadEvidence,
        family_evidence: CanonicalColdFamilyEvidence,
        elapsed: Duration,
    ) -> Self {
        record_subtree_cold_family_scan(&family_evidence, elapsed);
        Self {
            evidence,
            family_evidence,
        }
    }
}

pub(super) fn required_subtree_root_ranges(
    predecessor_tip_metadata: ChainTipMetadata,
    build_tip_metadata: ChainTipMetadata,
) -> Result<Vec<SubtreeRootRange>, CanonicalStoreError> {
    let mut ranges = Vec::with_capacity(3);
    for protocol in shielded_protocols() {
        let start_index = predecessor_tip_metadata.completed_subtree_count(protocol);
        let completed_at_tip = build_tip_metadata.completed_subtree_count(protocol);
        let root_count = completed_at_tip.checked_sub(start_index).ok_or_else(|| {
            CanonicalStoreError::subtree_root_sequence(format!(
                "{protocol:?} completed-subtree count regressed from {start_index} to {completed_at_tip}"
            ))
        })?;
        if let Some(max_entries) = NonZeroU32::new(root_count) {
            ranges.push(SubtreeRootRange::new(
                protocol,
                SubtreeRootIndex::new(start_index),
                max_entries,
            ));
        }
    }
    Ok(ranges)
}

pub(super) fn load_subtree_roots(
    db: &DB,
    build_plan: &CanonicalStoreBuildPlan,
    block_evidence: &CanonicalBlockLoadEvidence,
    subtree_roots: impl IntoIterator<Item = CanonicalBuildSubtreeRoot>,
) -> Result<CanonicalSubtreeRootLoadEvidence, CanonicalStoreError> {
    let family = subtree_root_family(db)?;
    if let Some(first_row) = db.iterator_cf(&family, IteratorMode::Start).next() {
        first_row.map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "inspect subtree-root family before load",
            source,
        })?;
        return Err(CanonicalStoreError::SubtreeRootLoadAlreadyLoaded);
    }
    let required_ranges = required_subtree_root_ranges(
        build_plan.history_predecessor().tip_metadata(),
        block_evidence.tip_metadata,
    )?;
    let mut stager = SubtreeRootStager::new(db, &family, build_plan, subtree_roots.into_iter());
    for range in required_ranges {
        stager.stage_range(range)?;
    }
    let (mut source_rows, expected_rows, batch) = stager.finish();
    if let Some(extra) = source_rows.next() {
        return Err(CanonicalStoreError::subtree_root_sequence(format!(
            "unexpected {:?} subtree root at index {}",
            extra.protocol,
            extra.subtree_index.value()
        )));
    }
    if !expected_rows.is_empty() {
        db.write(&batch)
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "write subtree roots",
                source,
            })?;
    }
    validate_persisted_subtree_roots(db, build_plan, &expected_rows)?;
    let subtree_root_count = u64::try_from(expected_rows.len()).map_err(|_| {
        CanonicalStoreError::subtree_root_sequence("subtree-root count exceeds u64::MAX")
    })?;
    let subtree_root_row_bytes = u64::try_from(SUBTREE_ROOT_KEY_LEN + SUBTREE_ROOT_VALUE_LEN)
        .map_err(|_| {
            CanonicalStoreError::subtree_root_sequence("subtree-root row length exceeds u64::MAX")
        })?;
    let subtree_root_logical_bytes = subtree_root_count
        .checked_mul(subtree_root_row_bytes)
        .ok_or_else(|| {
            CanonicalStoreError::subtree_root_sequence("subtree-root logical bytes exceed u64::MAX")
        })?;
    let subtree_root_sequence_digest = digest_subtree_root_rows(&expected_rows);
    Ok(CanonicalSubtreeRootLoadEvidence {
        subtree_root_count,
        subtree_root_logical_bytes,
        subtree_root_sequence_digest,
    })
}

pub(super) fn validate_persisted_subtree_root_family(
    db: &DB,
    build_plan: &CanonicalStoreBuildPlan,
    block_evidence: &CanonicalBlockLoadEvidence,
) -> Result<PersistedSubtreeRootValidation, CanonicalStoreError> {
    let started_at = Instant::now();
    let family = subtree_root_family(db)?;
    let required_ranges = required_subtree_root_ranges(
        build_plan.history_predecessor().tip_metadata(),
        block_evidence.tip_metadata,
    )?;
    let mut iteration_options = ReadOptions::default();
    iteration_options.fill_cache(false);
    iteration_options.set_readahead_size(2 * 1024 * 1024);
    let mut persisted_rows = db.raw_iterator_cf_opt(&family, iteration_options);
    persisted_rows.seek_to_first();
    let mut header_read_options = ReadOptions::default();
    header_read_options.fill_cache(false);
    let mut digest = Sha256::new();
    digest.update(SUBTREE_ROOT_SEQUENCE_DIGEST_DOMAIN);
    let mut subtree_root_count = 0_u64;
    let mut subtree_root_logical_bytes = 0_u64;
    let mut family_evidence = CanonicalColdFamilyEvidence::accumulator(SUBTREE_ROOT_COLUMN_FAMILY);
    for range in required_ranges {
        let mut previous_completion_height = None;
        for expected_index in range {
            let (key, encoded_root) = persisted_rows
                .item()
                .ok_or(CanonicalStoreError::SubtreeRootReadbackMismatch)?;
            let artifact = decode_subtree_root(key, encoded_root)?;
            if artifact.protocol != range.protocol
                || artifact.subtree_index != expected_index
                || previous_completion_height
                    .is_some_and(|previous| artifact.completing_block_height < previous)
                || artifact.completing_block_height
                    < build_plan.history_bounds().first_available_height()
                || artifact.completing_block_height > build_plan.build_tip().height
                || retained_block_hash_with_options(
                    db,
                    artifact.completing_block_height,
                    &header_read_options,
                )? != artifact.completing_block_hash
            {
                return Err(CanonicalStoreError::SubtreeRootReadbackMismatch);
            }
            digest.update(key.as_ref());
            digest.update(encoded_root.as_ref());
            family_evidence.observe(key, encoded_root)?;
            subtree_root_count = subtree_root_count.checked_add(1).ok_or_else(|| {
                CanonicalStoreError::subtree_root_sequence("subtree-root count exceeds u64::MAX")
            })?;
            let row_bytes = subtree_root_logical_row_bytes(key, encoded_root)?;
            subtree_root_logical_bytes = subtree_root_logical_bytes
                .checked_add(row_bytes)
                .ok_or_else(|| {
                    CanonicalStoreError::subtree_root_sequence(
                        "subtree-root logical bytes exceed u64::MAX",
                    )
                })?;
            previous_completion_height = Some(artifact.completing_block_height);
            persisted_rows.next();
        }
    }
    if persisted_rows.valid() {
        return Err(CanonicalStoreError::SubtreeRootReadbackMismatch);
    }
    persisted_rows
        .status()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "subtree-root publication readback",
            source,
        })?;
    let evidence = CanonicalSubtreeRootLoadEvidence {
        subtree_root_count,
        subtree_root_logical_bytes,
        subtree_root_sequence_digest: digest.finalize().into(),
    };
    Ok(PersistedSubtreeRootValidation::complete(
        evidence,
        family_evidence.finish(),
        started_at.elapsed(),
    ))
}

fn subtree_root_logical_row_bytes(
    key: &[u8],
    encoded_root: &[u8],
) -> Result<u64, CanonicalStoreError> {
    u64::try_from(key.len() + encoded_root.len()).map_err(|_| {
        CanonicalStoreError::subtree_root_sequence("subtree-root row length exceeds u64::MAX")
    })
}

fn record_subtree_cold_family_scan(evidence: &CanonicalColdFamilyEvidence, elapsed: Duration) {
    metrics::histogram!(
        "zinder_store_canonical_publication_family_scan_duration_seconds",
        "family" => evidence.family
    )
    .record(elapsed);
    metrics::counter!(
        "zinder_store_canonical_publication_family_scan_rows_total",
        "family" => evidence.family
    )
    .increment(evidence.row_count);
    metrics::counter!(
        "zinder_store_canonical_publication_family_scan_logical_bytes_total",
        "family" => evidence.family
    )
    .increment(evidence.logical_bytes);
}

fn digest_subtree_root_rows(rows: &[EncodedSubtreeRoot]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(SUBTREE_ROOT_SEQUENCE_DIGEST_DOMAIN);
    for (key, encoded_root) in rows {
        digest.update(key);
        digest.update(encoded_root);
    }
    digest.finalize().into()
}

struct SubtreeRootStager<'db, SourceRows> {
    db: &'db DB,
    family: &'db std::sync::Arc<rust_rocksdb::BoundColumnFamily<'db>>,
    build_plan: &'db CanonicalStoreBuildPlan,
    source_rows: SourceRows,
    expected_rows: Vec<EncodedSubtreeRoot>,
    batch: WriteBatch,
}

impl<'db, SourceRows> SubtreeRootStager<'db, SourceRows>
where
    SourceRows: Iterator<Item = CanonicalBuildSubtreeRoot>,
{
    fn new(
        db: &'db DB,
        family: &'db std::sync::Arc<rust_rocksdb::BoundColumnFamily<'db>>,
        build_plan: &'db CanonicalStoreBuildPlan,
        source_rows: SourceRows,
    ) -> Self {
        Self {
            db,
            family,
            build_plan,
            source_rows,
            expected_rows: Vec::new(),
            batch: WriteBatch::default(),
        }
    }

    fn stage_range(&mut self, range: SubtreeRootRange) -> Result<(), CanonicalStoreError> {
        let mut previous_completion_height = None;
        for expected_index in range {
            let source_root = self.source_rows.next().ok_or_else(|| {
                CanonicalStoreError::subtree_root_sequence(format!(
                    "missing {:?} subtree root at index {}",
                    range.protocol,
                    expected_index.value()
                ))
            })?;
            validate_source_subtree_root(
                self.build_plan,
                range.protocol,
                expected_index,
                previous_completion_height,
                source_root,
            )?;
            let completing_block_hash =
                retained_block_hash(self.db, source_root.completing_block_height)?;
            let artifact = SubtreeRootArtifact::new(
                source_root.protocol,
                source_root.subtree_index,
                source_root.root_hash,
                source_root.completing_block_height,
                completing_block_hash,
            );
            let encoded_key = encode_subtree_root_key(&artifact);
            let encoded_root = encode_subtree_root_value(&artifact);
            self.batch.put_cf(self.family, encoded_key, encoded_root);
            self.expected_rows.push((encoded_key, encoded_root));
            previous_completion_height = Some(source_root.completing_block_height);
        }
        Ok(())
    }

    fn finish(self) -> (SourceRows, Vec<EncodedSubtreeRoot>, WriteBatch) {
        (self.source_rows, self.expected_rows, self.batch)
    }
}

fn validate_source_subtree_root(
    build_plan: &CanonicalStoreBuildPlan,
    expected_protocol: ShieldedProtocol,
    expected_index: SubtreeRootIndex,
    previous_completion_height: Option<BlockHeight>,
    source_root: CanonicalBuildSubtreeRoot,
) -> Result<(), CanonicalStoreError> {
    if source_root.protocol != expected_protocol || source_root.subtree_index != expected_index {
        return Err(CanonicalStoreError::subtree_root_sequence(format!(
            "expected {expected_protocol:?} subtree index {}, observed {:?} index {}",
            expected_index.value(),
            source_root.protocol,
            source_root.subtree_index.value()
        )));
    }
    let first_height = build_plan.history_bounds().first_available_height();
    if source_root.completing_block_height < first_height
        || source_root.completing_block_height > build_plan.build_tip().height
    {
        return Err(CanonicalStoreError::subtree_root_sequence(format!(
            "{expected_protocol:?} subtree index {} completed outside retained history at height {}",
            expected_index.value(),
            source_root.completing_block_height.value()
        )));
    }
    if previous_completion_height
        .is_some_and(|previous| source_root.completing_block_height < previous)
    {
        return Err(CanonicalStoreError::subtree_root_sequence(format!(
            "{expected_protocol:?} subtree completion heights are not ascending"
        )));
    }
    Ok(())
}

fn validate_persisted_subtree_roots(
    db: &DB,
    build_plan: &CanonicalStoreBuildPlan,
    expected_rows: &[EncodedSubtreeRoot],
) -> Result<(), CanonicalStoreError> {
    let family = subtree_root_family(db)?;
    let mut persisted_rows = db.iterator_cf(&family, IteratorMode::Start);
    for (expected_key, expected_value) in expected_rows {
        let (persisted_key, persisted_value) = persisted_rows
            .next()
            .ok_or(CanonicalStoreError::SubtreeRootReadbackMismatch)?
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "subtree-root persisted readback",
                source,
            })?;
        if persisted_key.as_ref() != expected_key || persisted_value.as_ref() != expected_value {
            return Err(CanonicalStoreError::SubtreeRootReadbackMismatch);
        }
        let artifact = decode_subtree_root(expected_key, expected_value)?;
        if artifact.completing_block_height < build_plan.history_bounds().first_available_height()
            || artifact.completing_block_height > build_plan.build_tip().height
            || retained_block_hash(db, artifact.completing_block_height)?
                != artifact.completing_block_hash
        {
            return Err(CanonicalStoreError::SubtreeRootReadbackMismatch);
        }
    }
    if let Some(extra_row) = persisted_rows.next() {
        extra_row.map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "subtree-root persisted readback",
            source,
        })?;
        return Err(CanonicalStoreError::SubtreeRootReadbackMismatch);
    }
    Ok(())
}

fn subtree_root_family(
    db: &DB,
) -> Result<std::sync::Arc<rust_rocksdb::BoundColumnFamily<'_>>, CanonicalStoreError> {
    db.cf_handle(SUBTREE_ROOT_COLUMN_FAMILY).ok_or_else(|| {
        CanonicalStoreError::subtree_root_sequence("subtree_root column family is absent")
    })
}

pub(super) fn retained_block_hash(
    db: &DB,
    height: BlockHeight,
) -> Result<BlockHash, CanonicalStoreError> {
    retained_block_hash_with_options(db, height, &ReadOptions::default())
}

fn retained_block_hash_with_options(
    db: &DB,
    height: BlockHeight,
    read_options: &ReadOptions,
) -> Result<BlockHash, CanonicalStoreError> {
    let family = db.cf_handle(BLOCK_HEADER_COLUMN_FAMILY).ok_or_else(|| {
        CanonicalStoreError::subtree_root_sequence("block_header column family is absent")
    })?;
    let encoded_header = db
        .get_cf_opt(&family, encode_block_position(height), read_options)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "read subtree completing block header",
            source,
        })?
        .ok_or_else(|| {
            CanonicalStoreError::subtree_root_sequence(format!(
                "subtree completion header at height {} is absent",
                height.value()
            ))
        })?;
    if encoded_header.len() != BLOCK_HEADER_VALUE_LEN {
        return Err(CanonicalStoreError::subtree_root_sequence(format!(
            "subtree completion header at height {} has {} bytes; expected {BLOCK_HEADER_VALUE_LEN}",
            height.value(),
            encoded_header.len()
        )));
    }
    let mut block_hash = [0; 32];
    block_hash.copy_from_slice(&encoded_header[..32]);
    Ok(BlockHash::from_bytes(block_hash))
}

pub(super) fn encode_subtree_root_key(
    artifact: &SubtreeRootArtifact,
) -> [u8; SUBTREE_ROOT_KEY_LEN] {
    let mut key = [0; SUBTREE_ROOT_KEY_LEN];
    key[0] = artifact.protocol.id();
    key[1..].copy_from_slice(&artifact.subtree_index.value().to_be_bytes());
    key
}

pub(super) fn encode_subtree_root_value(
    artifact: &SubtreeRootArtifact,
) -> [u8; SUBTREE_ROOT_VALUE_LEN] {
    let mut encoded_root = [0; SUBTREE_ROOT_VALUE_LEN];
    encoded_root[..32].copy_from_slice(&artifact.root_hash.as_bytes());
    encoded_root[32..36].copy_from_slice(&artifact.completing_block_height.value().to_be_bytes());
    encoded_root[36..].copy_from_slice(&artifact.completing_block_hash.as_bytes());
    encoded_root
}

pub(in crate::canonical_store) fn decode_subtree_root(
    key: &[u8],
    encoded_root: &[u8],
) -> Result<SubtreeRootArtifact, CanonicalStoreError> {
    if key.len() != SUBTREE_ROOT_KEY_LEN || encoded_root.len() != SUBTREE_ROOT_VALUE_LEN {
        return Err(CanonicalStoreError::subtree_root_sequence(
            "subtree-root key or value has a non-v1 length",
        ));
    }
    let protocol = ShieldedProtocol::from_id(key[0]).ok_or_else(|| {
        CanonicalStoreError::subtree_root_sequence("subtree-root key has an unknown protocol")
    })?;
    let mut index = [0; 4];
    index.copy_from_slice(&key[1..]);
    let mut root_hash = [0; 32];
    root_hash.copy_from_slice(&encoded_root[..32]);
    let mut completing_height = [0; 4];
    completing_height.copy_from_slice(&encoded_root[32..36]);
    let mut completing_hash = [0; 32];
    completing_hash.copy_from_slice(&encoded_root[36..]);
    Ok(SubtreeRootArtifact::new(
        protocol,
        SubtreeRootIndex::new(u32::from_be_bytes(index)),
        SubtreeRootHash::from_bytes(root_hash),
        BlockHeight::new(u32::from_be_bytes(completing_height)),
        BlockHash::from_bytes(completing_hash),
    ))
}

const fn shielded_protocols() -> [ShieldedProtocol; 3] {
    [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ]
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use zinder_core::{
        BlockHash, BlockHeight, ChainTipMetadata, SUBTREE_LEAF_COUNT, ShieldedProtocol,
        SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange,
    };

    use super::{
        decode_subtree_root, encode_subtree_root_key, encode_subtree_root_value,
        required_subtree_root_ranges,
    };

    #[test]
    fn checkpointed_ranges_begin_after_predecessor_completed_subtrees()
    -> Result<(), Box<dyn std::error::Error>> {
        let predecessor = ChainTipMetadata::new(SUBTREE_LEAF_COUNT * 2 + 1, SUBTREE_LEAF_COUNT, 0);
        let tip = ChainTipMetadata::new(SUBTREE_LEAF_COUNT * 5, SUBTREE_LEAF_COUNT * 3 + 9, 0);

        let ranges = required_subtree_root_ranges(predecessor, tip)?;

        assert_eq!(
            ranges,
            vec![
                SubtreeRootRange::new(
                    ShieldedProtocol::Sapling,
                    SubtreeRootIndex::new(2),
                    NonZeroU32::new(3).ok_or("Sapling range must be nonzero")?,
                ),
                SubtreeRootRange::new(
                    ShieldedProtocol::Orchard,
                    SubtreeRootIndex::new(1),
                    NonZeroU32::new(2).ok_or("Orchard range must be nonzero")?,
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn required_ranges_reject_tree_size_regression() {
        let outcome = required_subtree_root_ranges(
            ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0, 0),
            ChainTipMetadata::empty(),
        );
        assert!(outcome.is_err());
    }

    #[test]
    fn subtree_root_v1_has_exact_known_bytes_and_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        let artifact = SubtreeRootArtifact::new(
            ShieldedProtocol::Orchard,
            SubtreeRootIndex::new(0x0102_0304),
            SubtreeRootHash::from_bytes([5; 32]),
            BlockHeight::new(0x1112_1314),
            BlockHash::from_bytes([6; 32]),
        );
        let key = encode_subtree_root_key(&artifact);
        let value = encode_subtree_root_value(&artifact);
        assert_eq!(key, [2, 1, 2, 3, 4]);
        assert_eq!(&value[..32], &[5; 32]);
        assert_eq!(&value[32..36], &[0x11, 0x12, 0x13, 0x14]);
        assert_eq!(&value[36..], &[6; 32]);
        assert_eq!(decode_subtree_root(&key, &value)?, artifact);
        Ok(())
    }
}
