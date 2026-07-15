//! Shared fact-sequence preparation and validation for the concrete storage
//! round-trip benchmark arms.

pub mod postgres;
pub mod rocksdb;

use std::{num::NonZeroU32, path::Path, sync::Arc};

use futures_util::{StreamExt as _, stream};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigest, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, Network, NetworkUpgradeActivations,
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
/// The reference encoding contains every field committed by
/// [`CanonicalBlockFactsDigest`]. The scalar identity columns support ordered
/// access, while read-back validation ties them to the versioned reference
/// encoding before using them for continuity checks.
#[derive(Clone, Debug)]
pub struct CanonicalBlockFactRecord {
    /// Source block height.
    pub height: BlockHeight,
    /// Source block hash in Zinder's internal byte order.
    pub block_hash: BlockHash,
    /// Parent hash in Zinder's internal byte order.
    pub parent_hash: BlockHash,
    /// Ordered transaction count in the aggregate.
    pub transaction_count: u32,
    /// Digest of the complete reference encoding.
    pub digest: CanonicalBlockFactsDigest,
    /// Complete backend-neutral reference encoding.
    pub reference_encoding: Vec<u8>,
}

/// Physical row values read back by either concrete storage candidate.
pub struct PersistedCanonicalBlockFactRow {
    /// Persisted block height.
    pub height: BlockHeight,
    /// Persisted block hash in Zinder's internal byte order.
    pub block_hash: BlockHash,
    /// Persisted parent hash in Zinder's internal byte order.
    pub parent_hash: BlockHash,
    /// Persisted transaction count.
    pub transaction_count: u32,
    /// Digest contract used for the persisted reference encoding.
    pub digest_version: CanonicalBlockFactsDigestVersion,
    /// Digest stored alongside the reference encoding.
    pub stored_digest: [u8; 32],
    /// Complete backend-neutral reference encoding read from storage.
    pub reference_encoding: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalFactReferenceScalars {
    height: BlockHeight,
    block_hash: BlockHash,
    parent_hash: BlockHash,
    transaction_count: u64,
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
        let reference_encoding = prepared.facts.reference_encoding(digest_version);
        let digest = reference_encoding.digest();
        Ok(Self {
            height: prepared.facts.block_header.height,
            block_hash: prepared.facts.block_header.block_hash,
            parent_hash: prepared.facts.block_header.parent_hash,
            transaction_count,
            digest,
            reference_encoding: reference_encoding.into_bytes(),
        })
    }

    /// Reconstructs a read-back row and rejects a stored digest or scalar
    /// column that does not match the persisted reference-encoding bytes.
    pub fn from_persisted(row: PersistedCanonicalBlockFactRow) -> Result<Self, BenchError> {
        let PersistedCanonicalBlockFactRow {
            height,
            block_hash,
            parent_hash,
            transaction_count,
            digest_version,
            stored_digest,
            reference_encoding,
        } = row;
        let digest =
            CanonicalBlockFactsDigest::from_reference_encoding(digest_version, &reference_encoding);
        if digest.as_bytes() != stored_digest {
            return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} persisted fact digest does not match its reference encoding",
                height.value()
            )));
        }
        let reference_scalars =
            decode_reference_scalars(digest_version, &reference_encoding, height)?;
        if reference_scalars.height != height {
            return Err(persisted_scalar_mismatch(height, "height"));
        }
        if reference_scalars.block_hash != block_hash {
            return Err(persisted_scalar_mismatch(height, "block hash"));
        }
        if reference_scalars.parent_hash != parent_hash {
            return Err(persisted_scalar_mismatch(height, "parent hash"));
        }
        if reference_scalars.transaction_count != u64::from(transaction_count) {
            return Err(persisted_scalar_mismatch(height, "transaction count"));
        }
        Ok(Self {
            height,
            block_hash,
            parent_hash,
            transaction_count,
            digest,
            reference_encoding,
        })
    }

    /// Returns the logical fact-envelope byte count submitted to storage.
    #[must_use]
    pub fn logical_bytes(&self) -> u64 {
        u64::try_from(self.reference_encoding.len()).unwrap_or(u64::MAX)
    }
}

fn decode_reference_scalars(
    digest_version: CanonicalBlockFactsDigestVersion,
    reference_encoding: &[u8],
    persisted_height: BlockHeight,
) -> Result<CanonicalFactReferenceScalars, BenchError> {
    match digest_version {
        CanonicalBlockFactsDigestVersion::V1 => {
            decode_v1_reference_scalars(reference_encoding, persisted_height)
        }
        _ => Err(BenchError::canonical_fact_sequence_mismatch(format!(
            "block {} reference encoding uses an unsupported digest version {}",
            persisted_height.value(),
            digest_version.value()
        ))),
    }
}

fn decode_v1_reference_scalars(
    reference_encoding: &[u8],
    persisted_height: BlockHeight,
) -> Result<CanonicalFactReferenceScalars, BenchError> {
    let mut facts = ReferenceEncodingFieldDecoder::new(reference_encoding, persisted_height);
    let encoded_version = decode_fixed_reference_field::<2>(
        facts.read_field(1, "digest version")?,
        persisted_height,
        "digest version",
    )?;
    if u16::from_le_bytes(encoded_version) != CanonicalBlockFactsDigestVersion::V1.value() {
        return Err(BenchError::canonical_fact_sequence_mismatch(format!(
            "block {} reference encoding embeds an unexpected digest version",
            persisted_height.value()
        )));
    }
    let block_header = facts.read_field(2, "block header")?;
    let _raw_block_bytes = facts.read_field(3, "raw block bytes")?;
    let transactions = facts.read_field(4, "transactions")?;
    facts.reject_trailing_bytes("canonical block facts")?;

    let mut header = ReferenceEncodingFieldDecoder::new(block_header, persisted_height);
    let encoded_height = decode_fixed_reference_field::<4>(
        header.read_field(1, "block height")?,
        persisted_height,
        "block height",
    )?;
    let block_hash = decode_fixed_reference_field::<32>(
        header.read_field(2, "block hash")?,
        persisted_height,
        "block hash",
    )?;
    let parent_hash = decode_fixed_reference_field::<32>(
        header.read_field(3, "parent hash")?,
        persisted_height,
        "parent hash",
    )?;
    let encoded_transaction_count = transactions.get(..8).ok_or_else(|| {
        BenchError::canonical_fact_sequence_mismatch(format!(
            "block {} reference encoding transaction sequence is shorter than its count",
            persisted_height.value()
        ))
    })?;
    let transaction_count =
        u64::from_le_bytes(encoded_transaction_count.try_into().map_err(|_| {
            BenchError::canonical_fact_sequence_mismatch(format!(
                "block {} reference encoding transaction count has an invalid width",
                persisted_height.value()
            ))
        })?);

    Ok(CanonicalFactReferenceScalars {
        height: BlockHeight::new(u32::from_le_bytes(encoded_height)),
        block_hash: BlockHash::from_bytes(block_hash),
        parent_hash: BlockHash::from_bytes(parent_hash),
        transaction_count,
    })
}

fn decode_fixed_reference_field<const FIELD_BYTES: usize>(
    encoded: &[u8],
    persisted_height: BlockHeight,
    field_name: &'static str,
) -> Result<[u8; FIELD_BYTES], BenchError> {
    encoded.try_into().map_err(|_| {
        BenchError::canonical_fact_sequence_mismatch(format!(
            "block {} reference encoding {field_name} must contain {FIELD_BYTES} bytes",
            persisted_height.value()
        ))
    })
}

fn persisted_scalar_mismatch(height: BlockHeight, field_name: &'static str) -> BenchError {
    BenchError::canonical_fact_sequence_mismatch(format!(
        "block {} persisted {field_name} does not match its reference encoding",
        height.value()
    ))
}

struct ReferenceEncodingFieldDecoder<'encoding> {
    remaining: &'encoding [u8],
    persisted_height: BlockHeight,
}

impl<'encoding> ReferenceEncodingFieldDecoder<'encoding> {
    const FIELD_HEADER_BYTES: usize = 10;

    fn new(remaining: &'encoding [u8], persisted_height: BlockHeight) -> Self {
        Self {
            remaining,
            persisted_height,
        }
    }

    fn read_field(
        &mut self,
        expected_tag: u16,
        field_name: &'static str,
    ) -> Result<&'encoding [u8], BenchError> {
        let header = self
            .remaining
            .get(..Self::FIELD_HEADER_BYTES)
            .ok_or_else(|| self.invalid_field(field_name, "is missing its field header"))?;
        let encoded_tag = u16::from_le_bytes(
            header[..2]
                .try_into()
                .map_err(|_| self.invalid_field(field_name, "has an invalid field tag"))?,
        );
        if encoded_tag != expected_tag {
            return Err(self.invalid_field(field_name, "has an unexpected field tag"));
        }
        let encoded_payload_bytes = u64::from_le_bytes(
            header[2..]
                .try_into()
                .map_err(|_| self.invalid_field(field_name, "has an invalid length"))?,
        );
        let payload_bytes = usize::try_from(encoded_payload_bytes)
            .map_err(|_| self.invalid_field(field_name, "length exceeds usize::MAX"))?;
        let field_bytes = Self::FIELD_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or_else(|| self.invalid_field(field_name, "length overflows usize"))?;
        let field = self
            .remaining
            .get(Self::FIELD_HEADER_BYTES..field_bytes)
            .ok_or_else(|| self.invalid_field(field_name, "payload is truncated"))?;
        self.remaining = &self.remaining[field_bytes..];
        Ok(field)
    }

    fn reject_trailing_bytes(&self, field_name: &'static str) -> Result<(), BenchError> {
        if self.remaining.is_empty() {
            return Ok(());
        }
        Err(self.invalid_field(field_name, "contains trailing bytes"))
    }

    fn invalid_field(&self, field_name: &'static str, reason: &'static str) -> BenchError {
        BenchError::canonical_fact_sequence_mismatch(format!(
            "block {} reference encoding {field_name} {reason}",
            self.persisted_height.value()
        ))
    }
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
    /// Sum of complete reference-encoding byte lengths.
    pub logical_fact_bytes: u64,
}

/// In-memory continuity and ordered-digest validator shared by input and
/// persisted read-back passes.
#[derive(Debug)]
pub struct CanonicalFactSequenceAccumulator {
    position: CanonicalFactSequencePosition,
    digest_builder: CanonicalBlockFactsSequenceDigestBuilder,
}

impl CanonicalFactSequenceAccumulator {
    /// Starts an empty ordered fact sequence.
    #[must_use]
    pub fn new() -> Self {
        Self {
            position: CanonicalFactSequencePosition {
                first_height: None,
                first_hash: None,
                tip_height: None,
                tip_hash: None,
                block_count: 0,
                logical_fact_bytes: 0,
            },
            digest_builder: CanonicalBlockFactsSequenceDigestBuilder::new(
                CanonicalBlockFactsSequenceDigestVersion::CURRENT,
            ),
        }
    }

    /// Validates and appends one record without mutating state on failure.
    pub fn append(&mut self, record: &CanonicalBlockFactRecord) -> Result<(), BenchError> {
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
        self.digest_builder = next_digest_builder;
        Ok(())
    }

    /// Returns the current logical sequence position without consuming it.
    #[must_use]
    pub const fn position(&self) -> CanonicalFactSequencePosition {
        self.position
    }

    /// Finishes the ordered reference digest.
    #[must_use]
    pub fn finish(self) -> CanonicalBlockFactsSequenceDigest {
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
pub async fn prepare_fixture_segment(
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
    fn persisted_rows_reject_height_and_transaction_count_outside_the_reference_encoding()
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

    fn record(
        height: u32,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
        reference_encoding: &[u8],
    ) -> CanonicalBlockFactRecord {
        let digest = CanonicalBlockFactsDigest::from_reference_encoding(
            CanonicalBlockFactsDigestVersion::CURRENT,
            reference_encoding,
        );
        CanonicalBlockFactRecord {
            height: BlockHeight::new(height),
            block_hash: BlockHash::from_bytes(block_hash),
            parent_hash: BlockHash::from_bytes(parent_hash),
            transaction_count: 1,
            digest,
            reference_encoding: reference_encoding.to_vec(),
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
        let reference_encoding = facts.reference_encoding(digest_version);
        let stored_digest = reference_encoding.digest().as_bytes();
        PersistedCanonicalBlockFactRow {
            height: BlockHeight::new(height),
            block_hash,
            parent_hash,
            transaction_count: 0,
            digest_version,
            stored_digest,
            reference_encoding: reference_encoding.into_bytes(),
        }
    }
}
