//! Shared fact-sequence preparation and validation for the concrete storage
//! round-trip benchmark arms.

pub mod postgres;
pub mod rocksdb;

use std::{num::NonZeroU32, path::Path, sync::Arc};

use futures_util::{StreamExt as _, stream};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsReplayFormatVersion, CanonicalBlockFactsSequenceDigest,
    CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion, Network,
    NetworkUpgradeActivations, decode_canonical_block_facts_replay,
    encode_canonical_block_facts_replay,
};
use zinder_ingest::prepare_canonical_block;

use crate::{
    BenchError,
    fixture::{SegmentDescriptor, read_segment_blocks},
};

/// Shared phase vocabulary for one complete persisted fact round trip.
///
/// Backend-specific work is attributed to the closest phase, but the phases are
/// diagnostic within an arm rather than interchangeable engine microbenchmarks.
/// Their sum explains the end-to-end wall clock apart from explicitly reported
/// unattributed framework overhead.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanonicalFactRoundTripTimings {
    /// End-to-end invocation time.
    pub wall_clock_seconds: f64,
    /// Fixture metadata validation plus fresh backend and schema initialization.
    pub storage_initialization_seconds: f64,
    /// Fixture parsing, fact encoding, and prepared-sequence validation.
    pub fact_preparation_seconds: f64,
    /// Primary fact-file/table construction and durable loading.
    pub fact_persistence_seconds: f64,
    /// Deferred index and constraint construction.
    pub index_construction_seconds: f64,
    /// Post-load storage optimization such as `ANALYZE`.
    pub storage_optimization_seconds: f64,
    /// Pre-publication persisted-row and fixture-oracle validation.
    pub validation_seconds: f64,
    /// Durable completion-fence publication.
    pub publication_seconds: f64,
    /// Validation through a new reader: a database reopen or server reconnection.
    pub fresh_reader_validation_seconds: f64,
    /// Final physical-storage and write-amplification measurement.
    pub storage_measurement_seconds: f64,
}

/// Complete block-local fact payload lowered for one concrete storage writer.
///
/// The replay encoding reconstructs the complete semantic fact aggregate. The
/// scalar identity columns support ordered access, while read-back validation
/// ties them to the decoded facts before using them for continuity checks.
#[derive(Clone, Debug)]
struct CanonicalBlockFactRecord {
    /// Source block height.
    height: BlockHeight,
    /// Source block hash in Zinder's internal byte order.
    block_hash: BlockHash,
    /// Parent hash in Zinder's internal byte order.
    parent_hash: BlockHash,
    /// Ordered transaction count in the aggregate.
    transaction_count: u32,
    /// Backend-neutral reference digest committed by the replay envelope.
    digest: CanonicalBlockFactsDigest,
    /// Version of the semantic replay format.
    replay_format_version: CanonicalBlockFactsReplayFormatVersion,
    /// Complete backend-neutral semantic replay encoding.
    replay_encoding: Vec<u8>,
}

/// Physical row values read back by either concrete storage candidate.
struct PersistedCanonicalBlockFactRow {
    /// Persisted block height.
    height: BlockHeight,
    /// Persisted block hash in Zinder's internal byte order.
    block_hash: BlockHash,
    /// Persisted parent hash in Zinder's internal byte order.
    parent_hash: BlockHash,
    /// Persisted transaction count.
    transaction_count: u32,
    /// Digest contract used for the persisted canonical facts.
    digest_version: CanonicalBlockFactsDigestVersion,
    /// Digest stored alongside the semantic replay encoding.
    stored_digest: [u8; 32],
    /// Complete backend-neutral semantic replay encoding read from storage.
    replay_encoding: Vec<u8>,
}

impl CanonicalBlockFactRecord {
    fn prepare(
        block: &zinder_source::SourceBlock,
        activations: &NetworkUpgradeActivations,
    ) -> Result<Self, BenchError> {
        let prepared = prepare_canonical_block(block, activations)?;
        let transaction_count = u32::try_from(prepared.facts.transactions.len()).map_err(|_| {
            BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} transaction count exceeds u32",
                block.height.value()
            ))
        })?;
        let digest_version = CanonicalBlockFactsDigestVersion::CURRENT;
        let replay_encoding = encode_canonical_block_facts_replay(
            &prepared.facts,
            CanonicalBlockFactsReplayFormatVersion::CURRENT,
            digest_version,
        );
        let replay_format_version = replay_encoding.format_version();
        let digest = replay_encoding.reference_digest();
        Ok(Self {
            height: prepared.facts.block_header.height,
            block_hash: prepared.facts.block_header.block_hash,
            parent_hash: prepared.facts.block_header.parent_hash,
            transaction_count,
            digest,
            replay_format_version,
            replay_encoding: replay_encoding.into_bytes(),
        })
    }

    /// Reconstructs a read-back row and rejects a stored digest or scalar
    /// column that does not match the decoded semantic replay facts.
    fn from_persisted(row: PersistedCanonicalBlockFactRow) -> Result<Self, BenchError> {
        let PersistedCanonicalBlockFactRow {
            height,
            block_hash,
            parent_hash,
            transaction_count,
            digest_version,
            stored_digest,
            replay_encoding,
        } = row;
        let decoded_replay =
            decode_canonical_block_facts_replay(&replay_encoding).map_err(|source| {
                BenchError::canonical_fact_sequence_mismatch(format!(
                    "block {} semantic replay decode failed: {source}",
                    height.value()
                ))
            })?;
        let replay_format_version = decoded_replay.format_version();
        let digest = decoded_replay.reference_digest();
        if digest.version() != digest_version {
            return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} replay digest version does not match its scalar column",
                height.value()
            )));
        }
        let facts = decoded_replay.facts();
        if digest.as_bytes() != stored_digest {
            return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} persisted fact digest does not match its replay facts",
                height.value()
            )));
        }
        if facts.block_header.height != height {
            return Err(persisted_scalar_mismatch(height, "height"));
        }
        if facts.block_header.block_hash != block_hash {
            return Err(persisted_scalar_mismatch(height, "block hash"));
        }
        if facts.block_header.parent_hash != parent_hash {
            return Err(persisted_scalar_mismatch(height, "parent hash"));
        }
        let decoded_transaction_count = u32::try_from(facts.transactions.len()).map_err(|_| {
            BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} decoded transaction count exceeds u32",
                height.value()
            ))
        })?;
        if decoded_transaction_count != transaction_count {
            return Err(persisted_scalar_mismatch(height, "transaction count"));
        }
        Ok(Self {
            height,
            block_hash,
            parent_hash,
            transaction_count,
            digest,
            replay_format_version,
            replay_encoding,
        })
    }

    /// Returns the logical fact-envelope byte count submitted to storage.
    #[must_use]
    fn logical_bytes(&self) -> u64 {
        u64::try_from(self.replay_encoding.len()).unwrap_or(u64::MAX)
    }
}

fn persisted_scalar_mismatch(height: BlockHeight, field_name: &'static str) -> BenchError {
    BenchError::canonical_fact_sequence_mismatch(format!(
        "block {} persisted {field_name} does not match its replay facts",
        height.value()
    ))
}

/// Ordered position and reference digest accumulated from fact records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalFactSequencePosition {
    /// First block height, or `None` before any append.
    pub first_height: Option<BlockHeight>,
    /// First block hash, or `None` before any append.
    pub first_hash: Option<BlockHash>,
    /// Last block height, or `None` before any append.
    pub tip_height: Option<BlockHeight>,
    /// Last block hash, or `None` before any append.
    pub tip_hash: Option<BlockHash>,
    /// Number of blocks accumulated.
    pub block_count: u64,
    /// Sum of complete semantic replay encoding byte lengths.
    pub logical_fact_bytes: u64,
    /// Shared semantic replay format version, or `None` before any append.
    pub replay_format_version: Option<u32>,
}

/// In-memory continuity and ordered-digest validator shared by input and
/// persisted read-back passes.
#[derive(Debug)]
struct CanonicalFactSequenceAccumulator {
    position: CanonicalFactSequencePosition,
    digest_builder: CanonicalBlockFactsSequenceDigestBuilder,
}

impl CanonicalFactSequenceAccumulator {
    /// Starts an empty ordered fact sequence.
    #[must_use]
    fn new() -> Self {
        Self {
            position: CanonicalFactSequencePosition {
                first_height: None,
                first_hash: None,
                tip_height: None,
                tip_hash: None,
                block_count: 0,
                logical_fact_bytes: 0,
                replay_format_version: None,
            },
            digest_builder: CanonicalBlockFactsSequenceDigestBuilder::new(
                CanonicalBlockFactsSequenceDigestVersion::CURRENT,
            ),
        }
    }

    /// Validates and appends one record without mutating state on failure.
    fn append(&mut self, record: &CanonicalBlockFactRecord) -> Result<(), BenchError> {
        if let Some(tip_height) = self.position.tip_height {
            let Some(expected_height) = tip_height.next() else {
                return Err(BenchError::canonical_fact_sequence_mismatch(
                    "block height overflow after u32::MAX",
                ));
            };
            if record.height != expected_height {
                return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                    "expected height {}, observed {}",
                    expected_height.value(),
                    record.height.value()
                )));
            }
        }
        if let Some(tip_hash) = self.position.tip_hash
            && record.parent_hash != tip_hash
        {
            return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} parent hash does not match block {} hash",
                record.height.value(),
                record.height.value().saturating_sub(1)
            )));
        }
        let replay_format_version = record.replay_format_version.value();
        if let Some(expected_replay_format_version) = self.position.replay_format_version
            && replay_format_version != expected_replay_format_version
        {
            return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} replay format version {} does not match sequence version {}",
                record.height.value(),
                replay_format_version,
                expected_replay_format_version
            )));
        }
        let next_block_count = self.position.block_count.checked_add(1).ok_or_else(|| {
            BenchError::canonical_fact_sequence_mismatch("block count exceeds u64::MAX")
        })?;
        let next_logical_fact_bytes = self
            .position
            .logical_fact_bytes
            .checked_add(record.logical_bytes())
            .ok_or_else(|| {
                BenchError::canonical_fact_sequence_mismatch(
                    "logical fact byte count exceeds u64::MAX",
                )
            })?;
        let mut next_digest_builder = self.digest_builder.clone();
        next_digest_builder
            .try_append(record.digest)
            .map_err(|source| BenchError::canonical_fact_sequence_mismatch(source.to_string()))?;

        if self.position.first_height.is_none() {
            self.position.first_height = Some(record.height);
            self.position.first_hash = Some(record.block_hash);
        }
        self.position.tip_height = Some(record.height);
        self.position.tip_hash = Some(record.block_hash);
        self.position.block_count = next_block_count;
        self.position.logical_fact_bytes = next_logical_fact_bytes;
        self.position.replay_format_version = Some(replay_format_version);
        self.digest_builder = next_digest_builder;
        Ok(())
    }

    /// Returns the current logical sequence position without consuming it.
    #[must_use]
    const fn position(&self) -> CanonicalFactSequencePosition {
        self.position
    }

    /// Finishes the ordered reference digest.
    #[must_use]
    fn finish(self) -> CanonicalBlockFactsSequenceDigest {
        self.digest_builder.finish()
    }
}

impl Default for CanonicalFactSequenceAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads and prepares one fixture segment with bounded parallel block parsing.
///
/// `buffered` preserves source order even when individual blocking tasks finish
/// out of order, so concrete writers can stream sorted rows directly.
async fn prepare_fixture_segment(
    fixture_directory: &Path,
    descriptor: &SegmentDescriptor,
    network: Network,
    activations: Arc<NetworkUpgradeActivations>,
    block_prepare_concurrency: NonZeroU32,
) -> Result<Vec<CanonicalBlockFactRecord>, BenchError> {
    let directory = fixture_directory.to_path_buf();
    let descriptor_for_read = descriptor.clone();
    let blocks = tokio::task::spawn_blocking(move || {
        read_segment_blocks(&directory, &descriptor_for_read, network)
    })
    .await
    .map_err(|source| BenchError::canonical_fact_preparation_task(source.to_string()))??;
    let concurrency = usize::try_from(block_prepare_concurrency.get()).unwrap_or(usize::MAX);
    let tasks = stream::iter(blocks.into_iter().map(|block| {
        let activations = Arc::clone(&activations);
        tokio::task::spawn_blocking(move || CanonicalBlockFactRecord::prepare(&block, &activations))
    }))
    .buffered(concurrency);
    futures_util::pin_mut!(tasks);
    let mut records = Vec::with_capacity(descriptor.block_count as usize);
    while let Some(task_result) = tasks.next().await {
        let record = task_result
            .map_err(|source| BenchError::canonical_fact_preparation_task(source.to_string()))??;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, CanonicalBlockFacts,
        CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsReplayFormatVersion, encode_canonical_block_facts_replay,
    };

    use super::{
        CanonicalBlockFactRecord, CanonicalFactSequenceAccumulator, PersistedCanonicalBlockFactRow,
    };

    #[test]
    fn sequence_accumulator_rejects_a_disconnected_block_before_hashing_it()
    -> Result<(), crate::BenchError> {
        let first = record(7, [1; 32], [0; 32], b"first");
        let disconnected = record(8, [2; 32], [9; 32], b"second");
        let mut accumulator = CanonicalFactSequenceAccumulator::new();

        accumulator.append(&first)?;
        let before = accumulator.position();
        let Some(error) = accumulator.append(&disconnected).err() else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "disconnected parent was accepted",
            ));
        };

        assert!(error.to_string().contains("parent hash"));
        assert_eq!(accumulator.position(), before);
        Ok(())
    }

    #[test]
    fn persisted_rows_reject_a_coupled_intermediate_hash_and_next_parent_rewrite()
    -> Result<(), crate::BenchError> {
        let mut scalar_only_sequence = CanonicalFactSequenceAccumulator::new();
        let first = CanonicalBlockFactRecord::from_persisted(persisted_row(7, [1; 32], [0; 32]))?;
        let mut scalar_only_intermediate =
            CanonicalBlockFactRecord::from_persisted(persisted_row(8, [2; 32], [1; 32]))?;
        let mut scalar_only_next =
            CanonicalBlockFactRecord::from_persisted(persisted_row(9, [3; 32], [2; 32]))?;
        let mut intermediate = persisted_row(8, [2; 32], [1; 32]);
        let mut next = persisted_row(9, [3; 32], [2; 32]);
        let replacement_hash = BlockHash::from_bytes([9; 32]);
        scalar_only_intermediate.block_hash = replacement_hash;
        scalar_only_next.parent_hash = replacement_hash;
        intermediate.block_hash = replacement_hash;
        next.parent_hash = replacement_hash;

        scalar_only_sequence.append(&first)?;
        scalar_only_sequence.append(&scalar_only_intermediate)?;
        scalar_only_sequence.append(&scalar_only_next)?;
        assert_eq!(scalar_only_sequence.position().block_count, 3);

        let Some(intermediate_error) = CanonicalBlockFactRecord::from_persisted(intermediate).err()
        else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "rewritten intermediate block hash was accepted",
            ));
        };
        let Some(next_error) = CanonicalBlockFactRecord::from_persisted(next).err() else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "rewritten next parent hash was accepted",
            ));
        };

        assert!(intermediate_error.to_string().contains("block hash"));
        assert!(next_error.to_string().contains("parent hash"));
        Ok(())
    }

    #[test]
    fn persisted_rows_reject_height_and_transaction_count_outside_the_replay_encoding()
    -> Result<(), crate::BenchError> {
        let mut rewritten_height = persisted_row(7, [1; 32], [0; 32]);
        rewritten_height.height = BlockHeight::new(8);
        let Some(height_error) = CanonicalBlockFactRecord::from_persisted(rewritten_height).err()
        else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "rewritten persisted height was accepted",
            ));
        };

        let mut rewritten_transaction_count = persisted_row(7, [1; 32], [0; 32]);
        rewritten_transaction_count.transaction_count = 1;
        let Some(transaction_count_error) =
            CanonicalBlockFactRecord::from_persisted(rewritten_transaction_count).err()
        else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "rewritten persisted transaction count was accepted",
            ));
        };

        assert!(height_error.to_string().contains("height"));
        assert!(
            transaction_count_error
                .to_string()
                .contains("transaction count")
        );
        Ok(())
    }

    #[test]
    fn persisted_rows_reject_tampered_semantic_replay_bytes() -> Result<(), crate::BenchError> {
        let mut tampered = persisted_row(7, [1; 32], [0; 32]);
        let Some(last_byte) = tampered.replay_encoding.last_mut() else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "test replay encoding is empty",
            ));
        };
        *last_byte ^= 1;

        let Some(error) = CanonicalBlockFactRecord::from_persisted(tampered).err() else {
            return Err(crate::BenchError::canonical_fact_sequence_mismatch(
                "tampered replay encoding was accepted",
            ));
        };

        assert!(error.to_string().contains("semantic replay decode"));
        Ok(())
    }

    fn record(
        height: u32,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
        replay_encoding: &[u8],
    ) -> CanonicalBlockFactRecord {
        let digest = CanonicalBlockFactsDigest::from_reference_encoding(
            CanonicalBlockFactsDigestVersion::CURRENT,
            replay_encoding,
        );
        CanonicalBlockFactRecord {
            height: BlockHeight::new(height),
            block_hash: BlockHash::from_bytes(block_hash),
            parent_hash: BlockHash::from_bytes(parent_hash),
            transaction_count: 1,
            digest,
            replay_format_version: CanonicalBlockFactsReplayFormatVersion::CURRENT,
            replay_encoding: replay_encoding.to_vec(),
        }
    }

    fn persisted_row(
        height: u32,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
    ) -> PersistedCanonicalBlockFactRow {
        let block_hash = BlockHash::from_bytes(block_hash);
        let parent_hash = BlockHash::from_bytes(parent_hash);
        let facts = CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                BlockHeight::new(height),
                block_hash,
                parent_hash,
                [0; 32],
                [0; 32],
                0,
                0,
                [0; 32],
                0,
                0,
            ),
            raw_block_bytes: None,
            transactions: Vec::new(),
        };
        let digest_version = CanonicalBlockFactsDigestVersion::CURRENT;
        let replay_encoding = encode_canonical_block_facts_replay(
            &facts,
            CanonicalBlockFactsReplayFormatVersion::CURRENT,
            digest_version,
        );
        let stored_digest = replay_encoding.reference_digest().as_bytes();
        PersistedCanonicalBlockFactRow {
            height: BlockHeight::new(height),
            block_hash,
            parent_hash,
            transaction_count: 0,
            digest_version,
            stored_digest,
            replay_encoding: replay_encoding.into_bytes(),
        }
    }
}
