//! On-disk fixed-range fixture: manifest, segment framing, and a
//! [`NodeSource`] that serves captured payloads for a deterministic replay.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, ConsensusBranchId, Network, NetworkUpgradeActivation,
    NetworkUpgradeActivations, ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex,
    wire::decode_zinder_native_chain_name,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceBlockHeader,
    SourceChainSegment, SourceChainSegmentLimits, SourceChainUpdate, SourceError,
    SourceSubtreeRoot, SourceSubtreeRoots, block_header_info_from_raw_block_bytes,
};

use crate::error::BenchError;

/// Version stamped into every manifest this crate writes.
pub const FIXTURE_FORMAT_VERSION: u32 = 4;

/// Manifest and segment file base names.
const MANIFEST_FILE_NAME: &str = "manifest.json";
const SEGMENT_MAGIC: [u8; 4] = *b"ZBS1";
const RECORD_HEADER_LEN: u64 = 8;

/// One consensus upgrade activation captured with the fixture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivationRecord {
    /// Consensus branch identifier as advertised by the node.
    pub branch_id: u32,
    /// Height at which the upgrade's rules first apply.
    pub activation_height: u32,
    /// Canonical upgrade name reported by the node.
    pub name: String,
}

/// One captured shielded subtree root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubtreeRootRecord {
    /// Subtree index within its protocol.
    pub index: u32,
    /// Subtree root hash in canonical byte order, hex-encoded.
    pub root_hash_hex: String,
    /// Height of the block that completed this subtree.
    pub completing_height: u32,
}

/// Captured subtree roots grouped by shielded protocol.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubtreeRootSet {
    /// Sapling subtree roots, ordered by index.
    pub sapling: Vec<SubtreeRootRecord>,
    /// Orchard subtree roots, ordered by index.
    pub orchard: Vec<SubtreeRootRecord>,
    /// Ironwood subtree roots, ordered by index.
    pub ironwood: Vec<SubtreeRootRecord>,
}

/// One captured segment of contiguous raw blocks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    /// Zero-based segment index.
    pub index: u32,
    /// First block height in the segment.
    pub from_height: u32,
    /// Last block height in the segment.
    pub to_height: u32,
    /// Number of blocks in the segment.
    pub block_count: u32,
    /// Segment file name, relative to the fixture directory.
    pub file: String,
    /// SHA-256 of the whole segment file, hex-encoded.
    pub sha256: String,
}

/// Backend-neutral canonical-fact oracle captured with one fixture.
///
/// Individual candidate rows carry their own block digests so read-back can
/// identify the first divergence. The fixture manifest keeps only the ordered
/// sequence evidence, which stays constant-size even for a full-chain corpus.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalBlockFactsDigestEvidence {
    /// Version used for every block digest folded into the sequence.
    pub block_digest_version: u16,
    /// Version used for [`Self::sequence_digest_sha256`].
    pub sequence_digest_version: u16,
    /// Number of ordered block digests folded into the sequence.
    pub block_count: u64,
    /// Ordered full-range reference digest encoded as lowercase SHA-256 hexadecimal.
    pub sequence_digest_sha256: String,
}

/// Source-workload density measured from the captured consensus bytes.
///
/// Totals show the amount of work in the fixture, while per-block maxima and
/// populated-block counts make burst-dominated ranges visible.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkloadDensity {
    /// Number of measured blocks.
    pub block_count: u32,
    /// Total raw serialized block bytes.
    pub raw_block_bytes: u64,
    /// Total transactions, including coinbase transactions.
    pub transaction_count: u64,
    /// Total non-coinbase transparent inputs.
    pub transparent_input_count: u64,
    /// Total transparent outputs, including coinbase outputs.
    pub transparent_output_count: u64,
    /// Blocks containing at least one non-coinbase transparent input.
    pub blocks_with_transparent_inputs: u32,
    /// Blocks containing at least one transparent output.
    pub blocks_with_transparent_outputs: u32,
    /// Largest raw serialized block in bytes.
    pub max_raw_block_bytes_per_block: u64,
    /// Largest transaction count observed in one block.
    pub max_transactions_per_block: u32,
    /// Largest non-coinbase transparent-input count observed in one block.
    pub max_transparent_inputs_per_block: u32,
    /// Largest transparent-output count observed in one block.
    pub max_transparent_outputs_per_block: u32,
}

impl WorkloadDensity {
    /// Adds another independently measured segment into this summary.
    pub fn merge(&mut self, segment: Self) {
        self.block_count = self.block_count.saturating_add(segment.block_count);
        self.raw_block_bytes = self.raw_block_bytes.saturating_add(segment.raw_block_bytes);
        self.transaction_count = self
            .transaction_count
            .saturating_add(segment.transaction_count);
        self.transparent_input_count = self
            .transparent_input_count
            .saturating_add(segment.transparent_input_count);
        self.transparent_output_count = self
            .transparent_output_count
            .saturating_add(segment.transparent_output_count);
        self.blocks_with_transparent_inputs = self
            .blocks_with_transparent_inputs
            .saturating_add(segment.blocks_with_transparent_inputs);
        self.blocks_with_transparent_outputs = self
            .blocks_with_transparent_outputs
            .saturating_add(segment.blocks_with_transparent_outputs);
        self.max_raw_block_bytes_per_block = self
            .max_raw_block_bytes_per_block
            .max(segment.max_raw_block_bytes_per_block);
        self.max_transactions_per_block = self
            .max_transactions_per_block
            .max(segment.max_transactions_per_block);
        self.max_transparent_inputs_per_block = self
            .max_transparent_inputs_per_block
            .max(segment.max_transparent_inputs_per_block);
        self.max_transparent_outputs_per_block = self
            .max_transparent_outputs_per_block
            .max(segment.max_transparent_outputs_per_block);
    }
}

/// The full fixed-range fixture manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureManifest {
    /// Fixture format version stamped by the writer.
    pub fixture_format_version: u32,
    /// Network name in Zinder-native encoding.
    pub network: String,
    /// First captured block height.
    pub from_height: u32,
    /// Last captured block height.
    pub to_height: u32,
    /// Total captured block count.
    pub block_count: u32,
    /// Consensus-byte workload density for the complete captured range.
    pub workload_density: WorkloadDensity,
    /// Current-schema oracle artifact version at capture time.
    ///
    /// This is provenance for comparisons with the temporary `RocksDB` oracle;
    /// it is not part of the backend-neutral canonical-fact contract.
    pub current_schema_oracle_artifact_schema_version: u16,
    /// Backend-neutral block-digest contract and ordered-sequence oracle.
    pub canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence,
    /// Hash of the block at `to_height`, hex-encoded in internal byte order,
    /// used as the replay tip.
    pub tip_hash_hex: String,
    /// Consensus activations required to derive the captured blocks.
    pub network_upgrade_activations: Vec<ActivationRecord>,
    /// Ordered segment descriptors.
    pub segments: Vec<SegmentDescriptor>,
    /// Captured shielded subtree roots.
    pub subtree_roots: SubtreeRootSet,
}

impl FixtureManifest {
    /// Reads and decodes a manifest from a fixture directory.
    pub fn read(directory: &Path) -> Result<Self, BenchError> {
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        let bytes = std::fs::read(&manifest_path)
            .map_err(|source| BenchError::io(&manifest_path, source))?;
        let manifest: Self = serde_json::from_slice(&bytes)?;
        if manifest.fixture_format_version != FIXTURE_FORMAT_VERSION {
            return Err(BenchError::fixture_format(format!(
                "unsupported fixture format version {} (expected {FIXTURE_FORMAT_VERSION})",
                manifest.fixture_format_version
            )));
        }
        manifest.validate_structure()?;
        Ok(manifest)
    }

    /// Writes the manifest into a fixture directory as pretty JSON.
    pub fn write(&self, directory: &Path) -> Result<(), BenchError> {
        self.validate_structure()?;
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        let encoded = serde_json::to_vec_pretty(self)?;
        std::fs::write(&manifest_path, encoded)
            .map_err(|source| BenchError::io(&manifest_path, source))
    }

    /// Returns a stable SHA-256 identity for the normalized manifest.
    ///
    /// The manifest includes the ordered SHA-256 digest of every segment, so
    /// this value identifies both the captured source corpus and the metadata
    /// required to replay it.
    pub fn digest_sha256(&self) -> Result<String, BenchError> {
        self.validate_structure()?;
        let normalized_manifest = serde_json::to_vec(self)?;
        let mut hasher = Sha256::new();
        hasher.update(b"zinder-bench-fixture-manifest-v1\0");
        hasher.update(normalized_manifest);
        Ok(hex::encode(hasher.finalize()))
    }

    fn validate_structure(&self) -> Result<(), BenchError> {
        if self.fixture_format_version != FIXTURE_FORMAT_VERSION {
            return Err(BenchError::fixture_format(format!(
                "fixture format version {} does not match {FIXTURE_FORMAT_VERSION}",
                self.fixture_format_version
            )));
        }
        self.validate_range_and_density()?;
        self.validate_canonical_block_facts_digest_evidence()?;
        self.validate_segments()
    }

    fn validate_range_and_density(&self) -> Result<(), BenchError> {
        let expected_block_count = self
            .to_height
            .checked_sub(self.from_height)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| {
                BenchError::fixture_format(format!(
                    "fixture range {}..={} is invalid",
                    self.from_height, self.to_height
                ))
            })?;
        if self.block_count != expected_block_count {
            return Err(BenchError::fixture_format(format!(
                "fixture block count {} does not cover range {}..={} ({expected_block_count} blocks)",
                self.block_count, self.from_height, self.to_height
            )));
        }
        if self.workload_density.block_count != self.block_count {
            return Err(BenchError::fixture_format(format!(
                "density block count {} does not match fixture block count {}",
                self.workload_density.block_count, self.block_count
            )));
        }
        Ok(())
    }

    fn validate_segments(&self) -> Result<(), BenchError> {
        if self.segments.is_empty() {
            return Err(BenchError::fixture_format(
                "fixture must contain at least one segment".to_owned(),
            ));
        }

        let mut next_height = self.from_height;
        let mut described_blocks = 0_u32;
        let mut segment_files = HashSet::new();
        for (position, descriptor) in self.segments.iter().enumerate() {
            let expected_index = u32::try_from(position).map_err(|_| {
                BenchError::fixture_format(
                    "fixture contains more than u32::MAX segments".to_owned(),
                )
            })?;
            if descriptor.index != expected_index {
                return Err(BenchError::fixture_format(format!(
                    "segment at manifest position {position} has index {}, expected {expected_index}",
                    descriptor.index
                )));
            }
            validate_segment_descriptor(descriptor)?;
            if descriptor.from_height != next_height {
                return Err(BenchError::fixture_format(format!(
                    "segment {} starts at {}, expected {}",
                    descriptor.index, descriptor.from_height, next_height
                )));
            }
            if !segment_files.insert(descriptor.file.as_str()) {
                return Err(BenchError::fixture_format(format!(
                    "segment file {} appears more than once",
                    descriptor.file
                )));
            }
            described_blocks = described_blocks
                .checked_add(descriptor.block_count)
                .ok_or_else(|| {
                    BenchError::fixture_format("segment block count overflow".to_owned())
                })?;
            if position + 1 == self.segments.len() {
                if descriptor.to_height != self.to_height {
                    return Err(BenchError::fixture_format(format!(
                        "final segment ends at {}, expected {}",
                        descriptor.to_height, self.to_height
                    )));
                }
            } else {
                next_height = descriptor.to_height.checked_add(1).ok_or_else(|| {
                    BenchError::fixture_format("segment height overflow".to_owned())
                })?;
            }
        }
        if described_blocks != self.block_count {
            return Err(BenchError::fixture_format(format!(
                "segments describe {described_blocks} blocks, expected {}",
                self.block_count
            )));
        }
        Ok(())
    }

    fn validate_canonical_block_facts_digest_evidence(&self) -> Result<(), BenchError> {
        let evidence = &self.canonical_block_facts_digest_evidence;
        CanonicalBlockFactsDigestVersion::try_from(evidence.block_digest_version)
            .map_err(|source| BenchError::fixture_format(source.to_string()))?;
        CanonicalBlockFactsSequenceDigestVersion::try_from(evidence.sequence_digest_version)
            .map_err(|source| BenchError::fixture_format(source.to_string()))?;

        if evidence.block_count != u64::from(self.block_count) {
            return Err(BenchError::fixture_format(format!(
                "canonical block facts digest count {} does not match fixture block count {}",
                evidence.block_count, self.block_count
            )));
        }
        validate_digest_hex(
            &evidence.sequence_digest_sha256,
            "canonical block facts sequence digest",
        )?;
        Ok(())
    }

    /// Resolves the captured network to a typed [`Network`].
    pub fn network_typed(&self) -> Result<Network, BenchError> {
        decode_zinder_native_chain_name(&self.network)
            .map_err(|source| BenchError::fixture_format(source.to_string()))
    }

    /// Rebuilds the typed activation table captured with the fixture.
    pub fn activations_typed(&self) -> Result<NetworkUpgradeActivations, BenchError> {
        let network = self.network_typed()?;
        let activations = self
            .network_upgrade_activations
            .iter()
            .map(|record| NetworkUpgradeActivation {
                branch_id: ConsensusBranchId::new(record.branch_id),
                activation_height: BlockHeight::new(record.activation_height),
                name: record.name.clone(),
            })
            .collect();
        NetworkUpgradeActivations::new(network, activations)
            .map_err(|source| BenchError::fixture_format(source.to_string()))
    }

    /// Resolves the replay tip identity from the captured tip hash.
    pub fn tip_id(&self) -> Result<BlockId, BenchError> {
        let hash = decode_internal_block_hash_hex(&self.tip_hash_hex)?;
        Ok(BlockId::new(BlockHeight::new(self.to_height), hash))
    }
}

fn validate_digest_hex(encoded: &str, field: &str) -> Result<(), BenchError> {
    if encoded.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(BenchError::fixture_format(format!(
            "{field} must use lowercase hexadecimal"
        )));
    }
    let bytes = hex::decode(encoded)
        .map_err(|source| BenchError::fixture_format(format!("invalid {field} hex: {source}")))?;
    if bytes.len() != 32 {
        return Err(BenchError::fixture_format(format!(
            "{field} must contain 32 bytes"
        )));
    }
    Ok(())
}

fn decode_internal_block_hash_hex(encoded: &str) -> Result<BlockHash, BenchError> {
    let bytes = hex::decode(encoded).map_err(|source| {
        BenchError::fixture_format(format!("invalid block hash hex: {source}"))
    })?;
    let fixed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| BenchError::fixture_format("block hash must be 32 bytes".to_owned()))?;
    Ok(BlockHash::from_bytes(fixed))
}

fn segment_file_name(index: u32) -> String {
    format!("segment-{index:06}.bin")
}

fn validate_segment_descriptor(descriptor: &SegmentDescriptor) -> Result<(), BenchError> {
    let expected_block_count = descriptor
        .to_height
        .checked_sub(descriptor.from_height)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| {
            BenchError::fixture_format(format!(
                "segment {} range {}..={} is invalid",
                descriptor.index, descriptor.from_height, descriptor.to_height
            ))
        })?;
    if descriptor.block_count != expected_block_count {
        return Err(BenchError::fixture_format(format!(
            "segment {} block count {} does not cover {}..={} ({expected_block_count} blocks)",
            descriptor.index, descriptor.block_count, descriptor.from_height, descriptor.to_height
        )));
    }
    let file_path = Path::new(&descriptor.file);
    let mut components = file_path.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(BenchError::fixture_format(format!(
            "segment {} file must be one relative file name",
            descriptor.index
        )));
    }
    let digest = hex::decode(&descriptor.sha256).map_err(|source| {
        BenchError::fixture_format(format!(
            "segment {} has an invalid SHA-256 descriptor: {source}",
            descriptor.index
        ))
    })?;
    if digest.len() != 32 {
        return Err(BenchError::fixture_format(format!(
            "segment {} SHA-256 descriptor must contain 32 bytes",
            descriptor.index
        )));
    }
    Ok(())
}

/// Writes one segment of contiguous blocks and returns its descriptor.
///
/// Blocks must be non-empty and ordered by ascending, contiguous height.
pub fn write_segment(
    directory: &Path,
    index: u32,
    blocks: &[SourceBlock],
) -> Result<SegmentDescriptor, BenchError> {
    let first = blocks
        .first()
        .ok_or_else(|| BenchError::invalid_argument("segment must contain at least one block"))?;
    let last = blocks
        .last()
        .ok_or_else(|| BenchError::invalid_argument("segment must contain at least one block"))?;
    for pair in blocks.windows(2) {
        let [previous, current] = pair else {
            continue;
        };
        if previous.height.next() != Some(current.height) || current.parent_hash != previous.hash {
            return Err(BenchError::invalid_argument(format!(
                "segment blocks {} and {} are not an ordered connected pair",
                previous.height.value(),
                current.height.value()
            )));
        }
    }
    let file_name = segment_file_name(index);
    let segment_path = directory.join(&file_name);
    let file =
        File::create(&segment_path).map_err(|source| BenchError::io(&segment_path, source))?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();

    write_tracked(&mut writer, &mut hasher, &SEGMENT_MAGIC, &segment_path)?;
    for block in blocks {
        let height_bytes = block.height.value().to_le_bytes();
        let byte_len = u32::try_from(block.raw_block_bytes.len()).map_err(|_| {
            BenchError::fixture_format("block exceeds 4 GiB frame limit".to_owned())
        })?;
        write_tracked(&mut writer, &mut hasher, &height_bytes, &segment_path)?;
        write_tracked(
            &mut writer,
            &mut hasher,
            &byte_len.to_le_bytes(),
            &segment_path,
        )?;
        write_tracked(
            &mut writer,
            &mut hasher,
            &block.raw_block_bytes,
            &segment_path,
        )?;
    }
    writer
        .flush()
        .map_err(|source| BenchError::io(&segment_path, source))?;

    let block_count = u32::try_from(blocks.len())
        .map_err(|_| BenchError::fixture_format("segment block count exceeds u32".to_owned()))?;
    Ok(SegmentDescriptor {
        index,
        from_height: first.height.value(),
        to_height: last.height.value(),
        block_count,
        file: file_name,
        sha256: hex::encode(hasher.finalize()),
    })
}

fn write_tracked(
    writer: &mut BufWriter<File>,
    hasher: &mut Sha256,
    bytes: &[u8],
    segment_path: &Path,
) -> Result<(), BenchError> {
    writer
        .write_all(bytes)
        .map_err(|source| BenchError::io(segment_path, source))?;
    hasher.update(bytes);
    Ok(())
}

/// Reads and decodes every block in one segment into memory.
///
/// Intended for tests and small fixtures. Large fixtures should be served
/// through [`FixtureNodeSource`], which reads one block at a time.
pub fn read_segment_blocks(
    directory: &Path,
    descriptor: &SegmentDescriptor,
    network: Network,
) -> Result<Vec<SourceBlock>, BenchError> {
    validate_segment_descriptor(descriptor)?;
    let segment_path = directory.join(&descriptor.file);
    verify_segment_sha256(&segment_path, &descriptor.sha256)?;
    let file = File::open(&segment_path).map_err(|source| BenchError::io(&segment_path, source))?;
    let mut reader = BufReader::new(file);
    read_and_verify_magic(&mut reader, &segment_path)?;

    let mut blocks = Vec::with_capacity(descriptor.block_count as usize);
    for offset in 0..descriptor.block_count {
        let (height, byte_len) = read_record_header(&mut reader, &segment_path)?;
        let expected_height = descriptor.from_height.checked_add(offset).ok_or_else(|| {
            BenchError::fixture_format("segment record height overflow".to_owned())
        })?;
        if height != expected_height {
            return Err(BenchError::fixture_format(format!(
                "segment {} record {offset} has height {height}, expected {expected_height}",
                descriptor.index
            )));
        }
        let mut raw_block_bytes = vec![0_u8; byte_len as usize];
        reader
            .read_exact(&mut raw_block_bytes)
            .map_err(|source| BenchError::io(&segment_path, source))?;
        let block =
            SourceBlock::from_raw_block_bytes(network, BlockHeight::new(height), raw_block_bytes)?;
        blocks.push(block);
    }
    reject_trailing_segment_bytes(&mut reader, &segment_path)?;
    Ok(blocks)
}

fn reject_trailing_segment_bytes(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(), BenchError> {
    let mut trailing_byte = [0_u8; 1];
    if reader
        .read(&mut trailing_byte)
        .map_err(|source| BenchError::io(segment_path, source))?
        != 0
    {
        return Err(BenchError::fixture_format(format!(
            "segment {} contains bytes after its declared records",
            segment_path.display()
        )));
    }
    Ok(())
}

fn read_and_verify_magic(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(), BenchError> {
    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|source| BenchError::io(segment_path, source))?;
    if magic != SEGMENT_MAGIC {
        return Err(BenchError::fixture_format(format!(
            "segment {} has an unexpected magic prefix",
            segment_path.display()
        )));
    }
    Ok(())
}

fn read_record_header(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(u32, u32), BenchError> {
    let mut header = [0_u8; 8];
    reader
        .read_exact(&mut header)
        .map_err(|source| BenchError::io(segment_path, source))?;
    let height = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let byte_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    Ok((height, byte_len))
}

#[derive(Clone, Debug)]
struct BlockLocation {
    segment_path: PathBuf,
    byte_offset: u64,
    byte_len: u32,
}

#[derive(Clone, Debug, Default)]
struct SubtreeRootsByProtocol {
    sapling: Vec<SourceSubtreeRoot>,
    orchard: Vec<SourceSubtreeRoot>,
    ironwood: Vec<SourceSubtreeRoot>,
}

impl SubtreeRootsByProtocol {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "ShieldedProtocol is non_exhaustive; a future protocol has no captured roots"
    )]
    fn for_protocol(&self, protocol: ShieldedProtocol) -> &[SourceSubtreeRoot] {
        match protocol {
            ShieldedProtocol::Sapling => &self.sapling,
            ShieldedProtocol::Orchard => &self.orchard,
            ShieldedProtocol::Ironwood => &self.ironwood,
            _ => &[],
        }
    }
}

fn subtree_roots_from_records(
    records: &[SubtreeRootRecord],
) -> Result<Vec<SourceSubtreeRoot>, BenchError> {
    records
        .iter()
        .map(|record| {
            let bytes = hex::decode(&record.root_hash_hex).map_err(|source| {
                BenchError::fixture_format(format!("invalid subtree root hex: {source}"))
            })?;
            let fixed: [u8; 32] = bytes.try_into().map_err(|_| {
                BenchError::fixture_format("subtree root hash must be 32 bytes".to_owned())
            })?;
            Ok(SourceSubtreeRoot::new(
                SubtreeRootIndex::new(record.index),
                SubtreeRootHash::from_bytes(fixed),
                BlockHeight::new(record.completing_height),
            ))
        })
        .collect()
}

/// A [`NodeSource`] that serves a captured fixed-range fixture.
///
/// Blocks are read from segment files one at a time, so replaying a large
/// range keeps only the requested block resident. The source advertises the
/// capabilities the bulk-catchup pipeline needs (best-chain blocks, tip
/// identity, subtree roots) and omits tree-state so sparse checkpoints are
/// skipped; the transparent hot path the benchmark measures does not use them.
#[derive(Clone)]
pub struct FixtureNodeSource {
    network: Network,
    tip: BlockId,
    capabilities: NodeCapabilities,
    locations: Arc<HashMap<u32, BlockLocation>>,
    subtree_roots: Arc<SubtreeRootsByProtocol>,
}

impl FixtureNodeSource {
    /// Opens a fixture directory and builds a block-offset index for replay.
    pub fn open(directory: &Path, manifest: &FixtureManifest) -> Result<Self, BenchError> {
        let network = manifest.network_typed()?;
        let tip = manifest.tip_id()?;
        let locations = index_segments(directory, manifest)?;
        let subtree_roots = SubtreeRootsByProtocol {
            sapling: subtree_roots_from_records(&manifest.subtree_roots.sapling)?,
            orchard: subtree_roots_from_records(&manifest.subtree_roots.orchard)?,
            ironwood: subtree_roots_from_records(&manifest.subtree_roots.ironwood)?,
        };
        let capabilities = NodeCapabilities::new([
            NodeCapability::JsonRpc,
            NodeCapability::BestChainBlocks,
            NodeCapability::TipId,
            NodeCapability::SubtreeRoots,
        ])
        .unwrap_or_default();
        Ok(Self {
            network,
            tip,
            capabilities,
            locations: Arc::new(locations),
            subtree_roots: Arc::new(subtree_roots),
        })
    }
}

fn index_segments(
    directory: &Path,
    manifest: &FixtureManifest,
) -> Result<HashMap<u32, BlockLocation>, BenchError> {
    let mut locations = HashMap::with_capacity(manifest.block_count as usize);
    for descriptor in &manifest.segments {
        let segment_path = directory.join(&descriptor.file);
        verify_segment_sha256(&segment_path, &descriptor.sha256)?;
        let file =
            File::open(&segment_path).map_err(|source| BenchError::io(&segment_path, source))?;
        let mut reader = BufReader::new(file);
        read_and_verify_magic(&mut reader, &segment_path)?;
        let mut position = u64::from(u32::try_from(SEGMENT_MAGIC.len()).unwrap_or(u32::MAX));
        for offset in 0..descriptor.block_count {
            let (height, byte_len) = read_record_header(&mut reader, &segment_path)?;
            let expected_height = descriptor.from_height.checked_add(offset).ok_or_else(|| {
                BenchError::fixture_format("segment record height overflow".to_owned())
            })?;
            if height != expected_height {
                return Err(BenchError::fixture_format(format!(
                    "segment {} record {offset} has height {height}, expected {expected_height}",
                    descriptor.index
                )));
            }
            let byte_offset = position + RECORD_HEADER_LEN;
            let replaced = locations.insert(
                height,
                BlockLocation {
                    segment_path: segment_path.clone(),
                    byte_offset,
                    byte_len,
                },
            );
            if replaced.is_some() {
                return Err(BenchError::fixture_format(format!(
                    "fixture contains duplicate block height {height}"
                )));
            }
            reader
                .seek(SeekFrom::Current(i64::from(byte_len)))
                .map_err(|source| BenchError::io(&segment_path, source))?;
            position = byte_offset + u64::from(byte_len);
        }
        reject_trailing_segment_bytes(&mut reader, &segment_path)?;
    }
    if locations.len() != manifest.block_count as usize {
        return Err(BenchError::fixture_format(format!(
            "fixture indexed {} unique blocks, expected {}",
            locations.len(),
            manifest.block_count
        )));
    }
    Ok(locations)
}

fn verify_segment_sha256(segment_path: &Path, expected_hex: &str) -> Result<(), BenchError> {
    let expected = hex::decode(expected_hex).map_err(|source| {
        BenchError::fixture_format(format!(
            "segment {} has an invalid SHA-256 descriptor: {source}",
            segment_path.display()
        ))
    })?;
    if expected.len() != 32 {
        return Err(BenchError::fixture_format(format!(
            "segment {} SHA-256 descriptor must contain 32 bytes",
            segment_path.display()
        )));
    }

    let file = File::open(segment_path).map_err(|source| BenchError::io(segment_path, source))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|source| BenchError::io(segment_path, source))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let actual = hasher.finalize();
    if actual.as_slice() != expected.as_slice() {
        return Err(BenchError::fixture_format(format!(
            "segment {} SHA-256 does not match its fixture descriptor",
            segment_path.display()
        )));
    }
    Ok(())
}

fn read_block_bytes(location: &BlockLocation) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(&location.segment_path)?;
    file.seek(SeekFrom::Start(location.byte_offset))?;
    let mut buffer = vec![0_u8; location.byte_len as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn read_and_decode_block(
    network: Network,
    location: &BlockLocation,
    height: BlockHeight,
) -> Result<SourceBlock, SourceError> {
    let raw_block_bytes =
        read_block_bytes(location).map_err(|source| SourceError::BlockUnavailable {
            height,
            reason: format!("fixture read failed: {source}"),
        })?;
    let header_info = block_header_info_from_raw_block_bytes(height, &raw_block_bytes)?;
    let block_time_seconds =
        u32::try_from(header_info.block_time).map_err(|_| SourceError::RawBlockTimeOutOfRange)?;
    Ok(SourceBlock::new(
        SourceBlockHeader {
            network,
            height,
            hash: header_info.block_id.hash,
            parent_hash: header_info.previous_block_hash,
            block_time_seconds,
        },
        raw_block_bytes,
    ))
}

impl FixtureNodeSource {
    fn read_block_task(
        &self,
        height: BlockHeight,
    ) -> Result<tokio::task::JoinHandle<Result<SourceBlock, SourceError>>, SourceError> {
        let Some(location) = self.locations.get(&height.value()).cloned() else {
            return Err(SourceError::BlockUnavailable {
                height,
                reason: "fixture does not contain this height".to_owned(),
            });
        };
        let network = self.network;
        Ok(tokio::task::spawn_blocking(move || {
            read_and_decode_block(network, &location, height)
        }))
    }

    /// Reads and decodes `[start_height, end_height]` off the blocking pool with
    /// every block in flight at once, returning them in ascending order.
    async fn read_connected_blocks(
        &self,
        start_height: BlockHeight,
        end_height: BlockHeight,
    ) -> Result<Vec<SourceBlock>, SourceError> {
        let mut tasks = Vec::new();
        let mut height = Some(start_height);
        while let Some(current) = height {
            if current > end_height {
                break;
            }
            tasks.push((current, self.read_block_task(current)?));
            height = current.next();
        }
        let mut blocks = Vec::with_capacity(tasks.len());
        for (current, task) in tasks {
            let block = task
                .await
                .map_err(|source| SourceError::BlockUnavailable {
                    height: current,
                    reason: format!("fixture read task failed: {source}"),
                })??;
            blocks.push(block);
        }
        Ok(blocks)
    }
}

#[async_trait]
impl NodeSource for FixtureNodeSource {
    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.read_block_task(height)?
            .await
            .map_err(|source| SourceError::BlockUnavailable {
                height,
                reason: format!("fixture read task failed: {source}"),
            })?
    }

    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError> {
        let (start_height, expected_parent_hash) = match limits.cursor.block_id() {
            None => match limits.cursor.next_connected_height() {
                Some(start_height) => (start_height, None),
                None => return Ok(SourceChainSegment::default()),
            },
            Some(block_id) => {
                if self.tip.height < block_id.height
                    || (self.tip.height == block_id.height && self.tip.hash != block_id.hash)
                {
                    return Ok(SourceChainSegment::new([
                        SourceChainUpdate::reverted_block(block_id),
                    ]));
                }
                match block_id.height.next() {
                    Some(start_height) => (start_height, Some(block_id.hash)),
                    None => return Ok(SourceChainSegment::default()),
                }
            }
        };
        if self.tip.height < start_height {
            return Ok(SourceChainSegment::default());
        }
        let end_height = BlockHeight::new(
            start_height
                .value()
                .saturating_add(limits.max_connected_blocks.get().saturating_sub(1))
                .min(self.tip.height.value()),
        );
        let blocks = self.read_connected_blocks(start_height, end_height).await?;
        if let Some(expected_parent_hash) = expected_parent_hash
            && blocks
                .first()
                .is_some_and(|block| block.parent_hash != expected_parent_hash)
        {
            let parent_height = start_height
                .value()
                .checked_sub(1)
                .map_or(start_height, BlockHeight::new);
            return Ok(SourceChainSegment::new([
                SourceChainUpdate::reverted_block(BlockId::new(
                    parent_height,
                    expected_parent_hash,
                )),
            ]));
        }
        Ok(SourceChainSegment::connected_blocks(blocks))
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        Ok(self.tip)
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        let all_roots = self.subtree_roots.for_protocol(protocol);
        let start = start_index.value() as usize;
        let end = start
            .saturating_add(max_entries.get() as usize)
            .min(all_roots.len());
        let slice = all_roots.get(start..end).unwrap_or(&[]).to_vec();
        Ok(SourceSubtreeRoots::new(protocol, start_index, slice))
    }
}
