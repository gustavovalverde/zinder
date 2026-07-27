//! On-disk fixed-range migration archive: manifest, segment framing, and a
//! [`NodeSource`] that serves captured payloads for a deterministic replay.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, ConsensusBranchId, Network, NetworkUpgradeActivation,
    NetworkUpgradeActivations, ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex,
    SubtreeRootRange, wire::decode_zinder_native_chain_name,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceBlockHeader,
    SourceChainSegment, SourceChainSegmentLimits, SourceChainSegmentStats, SourceChainUpdate,
    SourceError, SourceSubtreeRoot, SourceSubtreeRoots, block_header_from_raw_block_bytes,
};

use crate::migration_error::MigrationError;

/// Version stamped into every manifest this crate writes.
pub(crate) const MIGRATION_ARCHIVE_FORMAT_VERSION: u32 = 1;
/// Stable identity stamped into every canonical migration archive manifest.
pub(crate) const MIGRATION_ARCHIVE_IDENTITY: &str = "zinder-logical-state";

/// Manifest and segment file base names.
pub(crate) const MIGRATION_ARCHIVE_MANIFEST_FILE_NAME: &str = "migration-state.json";
const MIGRATION_ARCHIVE_MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const SEGMENT_MAGIC: [u8; 4] = *b"ZMS1";
const RECORD_HEADER_LEN: u64 = 8;

/// One consensus upgrade activation captured with the migration archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivationRecord {
    /// Consensus branch identifier as advertised by the node.
    pub(crate) branch_id: u32,
    /// Height at which the upgrade's rules first apply.
    pub(crate) activation_height: u32,
    /// Canonical upgrade name reported by the node.
    pub(crate) name: String,
}

/// One captured shielded subtree root.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubtreeRootRecord {
    /// Subtree index within its protocol.
    pub(crate) index: u32,
    /// Subtree root hash in canonical byte order, hex-encoded.
    pub(crate) root_hash_hex: String,
    /// Height of the block that completed this subtree.
    pub(crate) completing_height: u32,
}

/// Captured subtree roots grouped by shielded protocol.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubtreeRootSet {
    /// Sapling subtree roots, ordered by index.
    pub(crate) sapling: Vec<SubtreeRootRecord>,
    /// Orchard subtree roots, ordered by index.
    pub(crate) orchard: Vec<SubtreeRootRecord>,
    /// Ironwood subtree roots, ordered by index.
    pub(crate) ironwood: Vec<SubtreeRootRecord>,
}

/// One captured segment of contiguous raw blocks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SegmentDescriptor {
    /// Zero-based segment index.
    pub(crate) index: u32,
    /// First block height in the segment.
    pub(crate) from_height: u32,
    /// Last block height in the segment.
    pub(crate) to_height: u32,
    /// Number of blocks in the segment.
    pub(crate) block_count: u32,
    /// Segment file name, relative to the migration archive directory.
    pub(crate) file: String,
    /// SHA-256 of the whole segment file, hex-encoded.
    pub(crate) sha256: String,
}

/// Backend-neutral canonical-fact oracle captured with one migration archive.
///
/// Individual candidate rows carry their own block digests so read-back can
/// identify the first divergence. The migration archive manifest keeps only the ordered
/// sequence evidence, which stays constant-size even for a full-chain corpus.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalBlockFactsDigestEvidence {
    /// Version used for every block digest folded into the sequence.
    pub(crate) block_digest_version: u16,
    /// Version used for [`Self::sequence_digest_sha256`].
    pub(crate) sequence_digest_version: u16,
    /// Number of ordered block digests folded into the sequence.
    pub(crate) block_count: u64,
    /// Ordered full-range reference digest encoded as lowercase SHA-256 hexadecimal.
    pub(crate) sequence_digest_sha256: String,
}

/// Source-workload density measured from the captured consensus bytes.
///
/// Totals show the amount of work in the migration archive, while per-block maxima and
/// populated-block counts make burst-dominated ranges visible.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkloadDensity {
    /// Number of measured blocks.
    pub(crate) block_count: u32,
    /// Total raw serialized block bytes.
    pub(crate) raw_block_bytes: u64,
    /// Total transactions, including coinbase transactions.
    pub(crate) transaction_count: u64,
    /// Total non-coinbase transparent inputs.
    pub(crate) transparent_input_count: u64,
    /// Total transparent outputs, including coinbase outputs.
    pub(crate) transparent_output_count: u64,
    /// Blocks containing at least one non-coinbase transparent input.
    pub(crate) blocks_with_transparent_inputs: u32,
    /// Blocks containing at least one transparent output.
    pub(crate) blocks_with_transparent_outputs: u32,
    /// Largest raw serialized block in bytes.
    pub(crate) max_raw_block_bytes_per_block: u64,
    /// Largest transaction count observed in one block.
    pub(crate) max_transactions_per_block: u32,
    /// Largest non-coinbase transparent-input count observed in one block.
    pub(crate) max_transparent_inputs_per_block: u32,
    /// Largest transparent-output count observed in one block.
    pub(crate) max_transparent_outputs_per_block: u32,
}

impl WorkloadDensity {
    /// Adds another independently measured segment into this summary.
    pub(crate) fn merge(&mut self, segment: Self) {
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

/// The full fixed-range migration archive manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MigrationArchiveManifest {
    /// Stable contract identity that disambiguates this version from earlier formats.
    pub(crate) contract_identity: String,
    /// Migration format version stamped by the writer.
    pub(crate) archive_format_version: u32,
    /// Network name in Zinder-native encoding.
    pub(crate) network: String,
    /// First captured block height.
    pub(crate) from_height: u32,
    /// Last captured block height.
    pub(crate) to_height: u32,
    /// Total captured block count.
    pub(crate) block_count: u32,
    /// Consensus-byte workload density for the complete captured range.
    pub(crate) workload_density: WorkloadDensity,
    /// Canonical artifact schema version at capture time.
    ///
    /// This is provenance only; replay always constructs the destination
    /// binary's current schema from the backend-neutral source contract.
    pub(crate) source_canonical_schema_version: u16,
    /// Backend-neutral block-digest contract and ordered-sequence oracle.
    pub(crate) canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence,
    /// Hash of the block at `to_height`, hex-encoded in internal byte order,
    /// used as the replay tip.
    pub(crate) tip_hash_hex: String,
    /// Consensus activations required to derive the captured blocks.
    pub(crate) network_upgrade_activations: Vec<ActivationRecord>,
    /// Ordered segment descriptors.
    pub(crate) segments: Vec<SegmentDescriptor>,
    /// Captured shielded subtree roots.
    pub(crate) subtree_roots: SubtreeRootSet,
}

impl MigrationArchiveManifest {
    /// Reads and decodes a manifest from a migration archive directory.
    #[cfg(test)]
    pub(crate) fn read(directory: &Path) -> Result<Self, MigrationError> {
        Self::read_with_sha256(directory).map(|(manifest, _digest)| manifest)
    }

    /// Reads one manifest and returns the SHA-256 of its exact encoded bytes.
    pub(crate) fn read_with_sha256(directory: &Path) -> Result<(Self, String), MigrationError> {
        require_real_directory(directory, "migration archive directory")?;
        let manifest_path = directory.join(MIGRATION_ARCHIVE_MANIFEST_FILE_NAME);
        let bytes = read_bounded_regular_file(
            &manifest_path,
            "migration archive manifest",
            MIGRATION_ARCHIVE_MAX_MANIFEST_BYTES,
        )?;
        let manifest: Self = serde_json::from_slice(&bytes)?;
        if manifest.archive_format_version != MIGRATION_ARCHIVE_FORMAT_VERSION {
            return Err(MigrationError::archive_format(format!(
                "unsupported migration archive format version {} (expected {MIGRATION_ARCHIVE_FORMAT_VERSION})",
                manifest.archive_format_version
            )));
        }
        manifest.validate_structure()?;
        validate_archive_layout(directory, &manifest, true)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        Ok((manifest, digest))
    }

    /// Writes the manifest into a migration archive directory as pretty JSON.
    pub(crate) fn write(&self, directory: &Path) -> Result<(), MigrationError> {
        self.validate_structure()?;
        validate_archive_layout(directory, self, false)?;
        let manifest_path = directory.join(MIGRATION_ARCHIVE_MANIFEST_FILE_NAME);
        let encoded = serde_json::to_vec_pretty(self)?;
        let encoded_length = u64::try_from(encoded.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if encoded_length > MIGRATION_ARCHIVE_MAX_MANIFEST_BYTES {
            return Err(MigrationError::archive_format(
                "migration archive manifest exceeds its fixed byte limit",
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest_path)
            .map_err(|source| MigrationError::io(&manifest_path, source))?;
        file.write_all(&encoded)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| MigrationError::io(&manifest_path, source))?;
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| MigrationError::io(directory, source))
    }

    fn validate_structure(&self) -> Result<(), MigrationError> {
        if self.contract_identity != MIGRATION_ARCHIVE_IDENTITY {
            return Err(MigrationError::archive_format(format!(
                "migration archive contract identity {:?} does not match {MIGRATION_ARCHIVE_IDENTITY:?}",
                self.contract_identity
            )));
        }
        if self.archive_format_version != MIGRATION_ARCHIVE_FORMAT_VERSION {
            return Err(MigrationError::archive_format(format!(
                "migration archive format version {} does not match {MIGRATION_ARCHIVE_FORMAT_VERSION}",
                self.archive_format_version
            )));
        }
        self.network_typed()?;
        self.activations_typed()?;
        self.validate_range_and_density()?;
        self.validate_canonical_block_facts_digest_evidence()?;
        self.validate_segments()?;
        validate_digest_hex(&self.tip_hash_hex, "migration archive tip hash")?;
        self.validate_subtree_roots()
    }

    fn validate_range_and_density(&self) -> Result<(), MigrationError> {
        if self.from_height != 0 {
            return Err(MigrationError::archive_format(
                "format 1 migration archives must include the height-zero source predecessor",
            ));
        }
        if self.to_height == 0 {
            return Err(MigrationError::archive_format(
                "format 1 migration archives must contain at least canonical height 1",
            ));
        }
        let expected_block_count = self
            .to_height
            .checked_sub(self.from_height)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| {
                MigrationError::archive_format(format!(
                    "migration archive range {}..={} is invalid",
                    self.from_height, self.to_height
                ))
            })?;
        if self.block_count != expected_block_count {
            return Err(MigrationError::archive_format(format!(
                "migration archive block count {} does not cover range {}..={} ({expected_block_count} blocks)",
                self.block_count, self.from_height, self.to_height
            )));
        }
        if self.workload_density.block_count != self.block_count {
            return Err(MigrationError::archive_format(format!(
                "density block count {} does not match migration archive block count {}",
                self.workload_density.block_count, self.block_count
            )));
        }
        Ok(())
    }

    fn validate_segments(&self) -> Result<(), MigrationError> {
        if self.segments.is_empty() {
            return Err(MigrationError::archive_format(
                "migration archive must contain at least one segment".to_owned(),
            ));
        }

        let mut next_height = self.from_height;
        let mut described_blocks = 0_u32;
        let mut segment_files = HashSet::new();
        for (position, descriptor) in self.segments.iter().enumerate() {
            let expected_index = u32::try_from(position).map_err(|_| {
                MigrationError::archive_format(
                    "migration archive contains more than u32::MAX segments".to_owned(),
                )
            })?;
            if descriptor.index != expected_index {
                return Err(MigrationError::archive_format(format!(
                    "segment at manifest position {position} has index {}, expected {expected_index}",
                    descriptor.index
                )));
            }
            validate_segment_descriptor(descriptor)?;
            if descriptor.from_height != next_height {
                return Err(MigrationError::archive_format(format!(
                    "segment {} starts at {}, expected {}",
                    descriptor.index, descriptor.from_height, next_height
                )));
            }
            if !segment_files.insert(descriptor.file.as_str()) {
                return Err(MigrationError::archive_format(format!(
                    "segment file {} appears more than once",
                    descriptor.file
                )));
            }
            described_blocks = described_blocks
                .checked_add(descriptor.block_count)
                .ok_or_else(|| {
                    MigrationError::archive_format("segment block count overflow".to_owned())
                })?;
            if position + 1 == self.segments.len() {
                if descriptor.to_height != self.to_height {
                    return Err(MigrationError::archive_format(format!(
                        "final segment ends at {}, expected {}",
                        descriptor.to_height, self.to_height
                    )));
                }
            } else {
                next_height = descriptor.to_height.checked_add(1).ok_or_else(|| {
                    MigrationError::archive_format("segment height overflow".to_owned())
                })?;
            }
        }
        if described_blocks != self.block_count {
            return Err(MigrationError::archive_format(format!(
                "segments describe {described_blocks} blocks, expected {}",
                self.block_count
            )));
        }
        Ok(())
    }

    fn validate_subtree_roots(&self) -> Result<(), MigrationError> {
        for (protocol, roots) in [
            ("sapling", self.subtree_roots.sapling.as_slice()),
            ("orchard", self.subtree_roots.orchard.as_slice()),
            ("ironwood", self.subtree_roots.ironwood.as_slice()),
        ] {
            for (position, root) in roots.iter().enumerate() {
                let expected_index = u32::try_from(position).map_err(|_| {
                    MigrationError::archive_format(format!(
                        "{protocol} subtree root count exceeds u32"
                    ))
                })?;
                if root.index != expected_index {
                    return Err(MigrationError::archive_format(format!(
                        "{protocol} subtree root at position {position} has index {}, expected {expected_index}",
                        root.index
                    )));
                }
                if root.completing_height > self.to_height {
                    return Err(MigrationError::archive_format(format!(
                        "{protocol} subtree root {expected_index} completes after the archive tip"
                    )));
                }
                validate_digest_hex(
                    &root.root_hash_hex,
                    &format!("{protocol} subtree root {expected_index}"),
                )?;
            }
        }
        Ok(())
    }

    fn validate_canonical_block_facts_digest_evidence(&self) -> Result<(), MigrationError> {
        let evidence = &self.canonical_block_facts_digest_evidence;
        CanonicalBlockFactsDigestVersion::try_from(evidence.block_digest_version)
            .map_err(|source| MigrationError::archive_format(source.to_string()))?;
        CanonicalBlockFactsSequenceDigestVersion::try_from(evidence.sequence_digest_version)
            .map_err(|source| MigrationError::archive_format(source.to_string()))?;

        let expected_canonical_block_count = u64::from(self.block_count.saturating_sub(1));
        if evidence.block_count != expected_canonical_block_count {
            return Err(MigrationError::archive_format(format!(
                "canonical block facts digest count {} does not match retained block count {expected_canonical_block_count}",
                evidence.block_count
            )));
        }
        validate_digest_hex(
            &evidence.sequence_digest_sha256,
            "canonical block facts sequence digest",
        )?;
        Ok(())
    }

    /// Resolves the captured network to a typed [`Network`].
    pub(crate) fn network_typed(&self) -> Result<Network, MigrationError> {
        decode_zinder_native_chain_name(&self.network)
            .map_err(|source| MigrationError::archive_format(source.to_string()))
    }

    /// Rebuilds the typed activation table captured with the migration archive.
    pub(crate) fn activations_typed(&self) -> Result<NetworkUpgradeActivations, MigrationError> {
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
            .map_err(|source| MigrationError::archive_format(source.to_string()))
    }

    /// Resolves the replay tip identity from the captured tip hash.
    pub(crate) fn tip_id(&self) -> Result<BlockId, MigrationError> {
        let hash = decode_internal_block_hash_hex(&self.tip_hash_hex)?;
        Ok(BlockId::new(BlockHeight::new(self.to_height), hash))
    }
}

fn validate_archive_layout(
    directory: &Path,
    manifest: &MigrationArchiveManifest,
    published: bool,
) -> Result<(), MigrationError> {
    require_real_directory(directory, "migration archive directory")?;
    let mut expected = manifest
        .segments
        .iter()
        .map(|segment| segment.file.as_str())
        .collect::<BTreeSet<_>>();
    if published {
        expected.insert(MIGRATION_ARCHIVE_MANIFEST_FILE_NAME);
    }
    let mut observed = BTreeSet::new();
    for entry in
        std::fs::read_dir(directory).map_err(|source| MigrationError::io(directory, source))?
    {
        let entry = entry.map_err(|source| MigrationError::io(directory, source))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            MigrationError::archive_format("migration archive file names must be UTF-8")
        })?;
        if !expected.contains(name) {
            return Err(MigrationError::archive_format(format!(
                "migration archive contains unexpected entry {name:?}"
            )));
        }
        require_regular_file(&entry.path(), "migration archive entry")?;
        if !observed.insert(name.to_owned()) {
            return Err(MigrationError::archive_format(format!(
                "migration archive contains duplicate entry {name:?}"
            )));
        }
    }
    if observed.len() != expected.len()
        || expected
            .iter()
            .any(|expected_name| !observed.contains(*expected_name))
    {
        return Err(MigrationError::archive_format(
            "migration archive is missing a fixed-layout file",
        ));
    }
    Ok(())
}

fn require_real_directory(path: &Path, purpose: &str) -> Result<(), MigrationError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| MigrationError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MigrationError::archive_format(format!(
            "{purpose} must be a real directory"
        )));
    }
    Ok(())
}

fn require_regular_file(path: &Path, purpose: &str) -> Result<(), MigrationError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| MigrationError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MigrationError::archive_format(format!(
            "{purpose} must be a regular file"
        )));
    }
    require_single_link(path, &metadata, purpose)
}

#[cfg(unix)]
fn require_single_link(
    path: &Path,
    metadata: &std::fs::Metadata,
    purpose: &str,
) -> Result<(), MigrationError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.nlink() != 1 {
        return Err(MigrationError::archive_format(format!(
            "{purpose} must not be hard-linked: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_single_link(
    _path: &Path,
    _metadata: &std::fs::Metadata,
    _purpose: &str,
) -> Result<(), MigrationError> {
    Ok(())
}

fn read_bounded_regular_file(
    path: &Path,
    purpose: &str,
    maximum_bytes: u64,
) -> Result<Vec<u8>, MigrationError> {
    require_regular_file(path, purpose)?;
    let metadata = std::fs::metadata(path).map_err(|source| MigrationError::io(path, source))?;
    if metadata.len() > maximum_bytes {
        return Err(MigrationError::archive_format(format!(
            "{purpose} exceeds its fixed byte limit"
        )));
    }
    let file = File::open(path).map_err(|source| MigrationError::io(path, source))?;
    let opened = file
        .metadata()
        .map_err(|source| MigrationError::io(path, source))?;
    require_single_link(path, &opened, purpose)?;
    if !opened.is_file() || opened.len() != metadata.len() {
        return Err(MigrationError::archive_format(format!(
            "{purpose} changed while it was opened"
        )));
    }
    let mut bytes = Vec::new();
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| MigrationError::io(path, source))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum_bytes {
        return Err(MigrationError::archive_format(format!(
            "{purpose} exceeds its fixed byte limit"
        )));
    }
    Ok(bytes)
}

fn validate_digest_hex(encoded: &str, field: &str) -> Result<(), MigrationError> {
    if encoded.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(MigrationError::archive_format(format!(
            "{field} must use lowercase hexadecimal"
        )));
    }
    let bytes = hex::decode(encoded).map_err(|source| {
        MigrationError::archive_format(format!("invalid {field} hex: {source}"))
    })?;
    if bytes.len() != 32 {
        return Err(MigrationError::archive_format(format!(
            "{field} must contain 32 bytes"
        )));
    }
    Ok(())
}

fn decode_internal_block_hash_hex(encoded: &str) -> Result<BlockHash, MigrationError> {
    let bytes = hex::decode(encoded).map_err(|source| {
        MigrationError::archive_format(format!("invalid block hash hex: {source}"))
    })?;
    let fixed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| MigrationError::archive_format("block hash must be 32 bytes".to_owned()))?;
    Ok(BlockHash::from_bytes(fixed))
}

fn segment_file_name(index: u32) -> String {
    format!("segment-{index:06}.bin")
}

fn validate_segment_descriptor(descriptor: &SegmentDescriptor) -> Result<(), MigrationError> {
    let expected_block_count = descriptor
        .to_height
        .checked_sub(descriptor.from_height)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| {
            MigrationError::archive_format(format!(
                "segment {} range {}..={} is invalid",
                descriptor.index, descriptor.from_height, descriptor.to_height
            ))
        })?;
    if descriptor.block_count != expected_block_count {
        return Err(MigrationError::archive_format(format!(
            "segment {} block count {} does not cover {}..={} ({expected_block_count} blocks)",
            descriptor.index, descriptor.block_count, descriptor.from_height, descriptor.to_height
        )));
    }
    if descriptor.file != segment_file_name(descriptor.index) {
        return Err(MigrationError::archive_format(format!(
            "segment {} file name does not match the fixed layout",
            descriptor.index
        )));
    }
    let file_path = Path::new(&descriptor.file);
    let mut components = file_path.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(MigrationError::archive_format(format!(
            "segment {} file must be one relative file name",
            descriptor.index
        )));
    }
    validate_digest_hex(
        &descriptor.sha256,
        &format!("segment {} SHA-256", descriptor.index),
    )
}

/// Writes one segment of contiguous blocks and returns its descriptor.
///
/// Blocks must be non-empty and ordered by ascending, contiguous height.
pub(crate) fn write_segment(
    directory: &Path,
    index: u32,
    blocks: &[SourceBlock],
) -> Result<SegmentDescriptor, MigrationError> {
    let first = blocks.first().ok_or_else(|| {
        MigrationError::invalid_argument("segment must contain at least one block")
    })?;
    let last = blocks.last().ok_or_else(|| {
        MigrationError::invalid_argument("segment must contain at least one block")
    })?;
    for pair in blocks.windows(2) {
        let [previous, current] = pair else {
            continue;
        };
        if previous.height.next() != Some(current.height) || current.parent_hash != previous.hash {
            return Err(MigrationError::invalid_argument(format!(
                "segment blocks {} and {} are not an ordered connected pair",
                previous.height.value(),
                current.height.value()
            )));
        }
    }
    let file_name = segment_file_name(index);
    let segment_path = directory.join(&file_name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&segment_path)
        .map_err(|source| MigrationError::io(&segment_path, source))?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();

    write_tracked(&mut writer, &mut hasher, &SEGMENT_MAGIC, &segment_path)?;
    for block in blocks {
        let height_bytes = block.height.value().to_le_bytes();
        let byte_len = u32::try_from(block.raw_block_bytes.len()).map_err(|_| {
            MigrationError::archive_format("block exceeds 4 GiB frame limit".to_owned())
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
        .map_err(|source| MigrationError::io(&segment_path, source))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|source| MigrationError::io(&segment_path, source))?;

    let block_count = u32::try_from(blocks.len()).map_err(|_| {
        MigrationError::archive_format("segment block count exceeds u32".to_owned())
    })?;
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
) -> Result<(), MigrationError> {
    writer
        .write_all(bytes)
        .map_err(|source| MigrationError::io(segment_path, source))?;
    hasher.update(bytes);
    Ok(())
}

fn reject_trailing_segment_bytes(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(), MigrationError> {
    let mut trailing_byte = [0_u8; 1];
    if reader
        .read(&mut trailing_byte)
        .map_err(|source| MigrationError::io(segment_path, source))?
        != 0
    {
        return Err(MigrationError::archive_format(format!(
            "segment {} contains bytes after its declared records",
            segment_path.display()
        )));
    }
    Ok(())
}

fn read_and_verify_magic(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(), MigrationError> {
    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|source| MigrationError::io(segment_path, source))?;
    if magic != SEGMENT_MAGIC {
        return Err(MigrationError::archive_format(format!(
            "segment {} has an unexpected magic prefix",
            segment_path.display()
        )));
    }
    Ok(())
}

fn read_record_header(
    reader: &mut BufReader<File>,
    segment_path: &Path,
) -> Result<(u32, u32), MigrationError> {
    let mut header = [0_u8; 8];
    reader
        .read_exact(&mut header)
        .map_err(|source| MigrationError::io(segment_path, source))?;
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
) -> Result<Vec<SourceSubtreeRoot>, MigrationError> {
    records
        .iter()
        .map(|record| {
            let bytes = hex::decode(&record.root_hash_hex).map_err(|source| {
                MigrationError::archive_format(format!("invalid subtree root hex: {source}"))
            })?;
            let fixed: [u8; 32] = bytes.try_into().map_err(|_| {
                MigrationError::archive_format("subtree root hash must be 32 bytes".to_owned())
            })?;
            Ok(SourceSubtreeRoot::new(
                SubtreeRootIndex::new(record.index),
                SubtreeRootHash::from_bytes(fixed),
                BlockHeight::new(record.completing_height),
            ))
        })
        .collect()
}

/// A [`NodeSource`] that serves a captured fixed-range migration archive.
///
/// Blocks are read from segment files one at a time, so replaying a large
/// range keeps only the requested block resident. The source advertises the
/// capabilities the bulk-catchup pipeline needs (best-chain blocks, tip
/// identity, subtree roots) and omits tree-state so sparse checkpoints are
/// skipped because replay derives the destination state from raw blocks and
/// the captured predecessor data rather than a physical tree-state decoder.
#[derive(Clone)]
pub(crate) struct MigrationArchiveSource {
    network: Network,
    tip: BlockId,
    capabilities: NodeCapabilities,
    locations: Arc<HashMap<u32, BlockLocation>>,
    subtree_roots: Arc<SubtreeRootsByProtocol>,
    segment_response_delay: Duration,
}

impl MigrationArchiveSource {
    /// Opens a migration archive directory and builds a block-offset index for replay.
    pub(crate) fn open(
        directory: &Path,
        manifest: &MigrationArchiveManifest,
    ) -> Result<Self, MigrationError> {
        Self::open_with_segment_delay(directory, manifest, Duration::ZERO)
    }

    /// Opens a migration archive and delays each source-segment response by `delay`.
    pub(crate) fn open_with_segment_delay(
        directory: &Path,
        manifest: &MigrationArchiveManifest,
        delay: Duration,
    ) -> Result<Self, MigrationError> {
        let network = manifest.network_typed()?;
        let tip = manifest.tip_id()?;
        let locations = index_segments(directory, manifest)?;
        validate_connected_blocks(network, tip, &locations)?;
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
            segment_response_delay: delay,
        })
    }
}

fn validate_connected_blocks(
    network: Network,
    tip: BlockId,
    locations: &HashMap<u32, BlockLocation>,
) -> Result<(), MigrationError> {
    let mut previous = None;
    for height in 0..=tip.height.value() {
        let block_height = BlockHeight::new(height);
        let location = locations.get(&height).ok_or_else(|| {
            MigrationError::archive_format(format!(
                "migration archive is missing block height {height}"
            ))
        })?;
        let block = read_and_decode_block(network, location, block_height)?;
        if height == 0 && block.hash != network.genesis_hash() {
            return Err(MigrationError::archive_format(
                "migration archive height-zero block is not the network genesis",
            ));
        }
        if let Some(previous_hash) = previous
            && block.parent_hash != previous_hash
        {
            return Err(MigrationError::archive_format(format!(
                "migration archive block {height} is disconnected from its predecessor"
            )));
        }
        previous = Some(block.hash);
    }
    if previous != Some(tip.hash) {
        return Err(MigrationError::archive_format(
            "migration archive tip hash does not match its final raw block",
        ));
    }
    Ok(())
}

fn index_segments(
    directory: &Path,
    manifest: &MigrationArchiveManifest,
) -> Result<HashMap<u32, BlockLocation>, MigrationError> {
    let mut locations = HashMap::with_capacity(manifest.block_count as usize);
    for descriptor in &manifest.segments {
        let segment_path = directory.join(&descriptor.file);
        verify_segment_sha256(&segment_path, &descriptor.sha256)?;
        let file = File::open(&segment_path)
            .map_err(|source| MigrationError::io(&segment_path, source))?;
        let mut reader = BufReader::new(file);
        read_and_verify_magic(&mut reader, &segment_path)?;
        let mut position = u64::from(u32::try_from(SEGMENT_MAGIC.len()).unwrap_or(u32::MAX));
        for offset in 0..descriptor.block_count {
            let (height, byte_len) = read_record_header(&mut reader, &segment_path)?;
            let expected_height = descriptor.from_height.checked_add(offset).ok_or_else(|| {
                MigrationError::archive_format("segment record height overflow".to_owned())
            })?;
            if height != expected_height {
                return Err(MigrationError::archive_format(format!(
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
                return Err(MigrationError::archive_format(format!(
                    "migration archive contains duplicate block height {height}"
                )));
            }
            reader
                .seek(SeekFrom::Current(i64::from(byte_len)))
                .map_err(|source| MigrationError::io(&segment_path, source))?;
            position = byte_offset + u64::from(byte_len);
        }
        reject_trailing_segment_bytes(&mut reader, &segment_path)?;
    }
    if locations.len() != manifest.block_count as usize {
        return Err(MigrationError::archive_format(format!(
            "migration archive indexed {} unique blocks, expected {}",
            locations.len(),
            manifest.block_count
        )));
    }
    Ok(locations)
}

fn verify_segment_sha256(segment_path: &Path, expected_hex: &str) -> Result<(), MigrationError> {
    require_regular_file(segment_path, "migration archive segment")?;
    let expected = hex::decode(expected_hex).map_err(|source| {
        MigrationError::archive_format(format!(
            "segment {} has an invalid SHA-256 descriptor: {source}",
            segment_path.display()
        ))
    })?;

    let file =
        File::open(segment_path).map_err(|source| MigrationError::io(segment_path, source))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|source| MigrationError::io(segment_path, source))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let actual = hasher.finalize();
    if actual.as_slice() != expected.as_slice() {
        return Err(MigrationError::archive_format(format!(
            "segment {} SHA-256 does not match its migration archive descriptor",
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
            reason: format!("migration archive read failed: {source}"),
        })?;
    let header = block_header_from_raw_block_bytes(height, &raw_block_bytes)?;
    let block_time_seconds =
        u32::try_from(header.block_time).map_err(|_| SourceError::RawBlockTimeOutOfRange)?;
    Ok(SourceBlock::new(
        SourceBlockHeader {
            network,
            height,
            hash: header.block_id.hash,
            parent_hash: header.previous_block_hash,
            block_time_seconds,
        },
        raw_block_bytes,
    ))
}

impl MigrationArchiveSource {
    fn read_block_task(
        &self,
        height: BlockHeight,
    ) -> Result<tokio::task::JoinHandle<Result<SourceBlock, SourceError>>, SourceError> {
        let Some(location) = self.locations.get(&height.value()).cloned() else {
            return Err(SourceError::BlockUnavailable {
                height,
                reason: "migration archive does not contain this height".to_owned(),
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
                    reason: format!("migration archive read task failed: {source}"),
                })??;
            blocks.push(block);
        }
        Ok(blocks)
    }

    fn json_hex_payload_bytes(
        &self,
        start_height: BlockHeight,
        end_height: BlockHeight,
    ) -> Result<u64, SourceError> {
        let mut response_payload_bytes = 0_u64;
        let mut height = Some(start_height);
        while let Some(current) = height {
            if current > end_height {
                break;
            }
            let location = self.locations.get(&current.value()).ok_or_else(|| {
                SourceError::BlockUnavailable {
                    height: current,
                    reason: "migration archive does not contain this height".to_owned(),
                }
            })?;
            response_payload_bytes = response_payload_bytes
                .saturating_add(u64::from(location.byte_len).saturating_mul(2));
            height = current.next();
        }
        Ok(response_payload_bytes)
    }

    async fn read_connected_blocks_with_response_limit(
        &self,
        start_height: BlockHeight,
        end_height: BlockHeight,
        max_response_bytes: u64,
    ) -> Result<(Vec<SourceBlock>, SourceChainSegmentStats), SourceError> {
        let mut pending = vec![(start_height, end_height)];
        let mut blocks = Vec::new();
        let mut stats = SourceChainSegmentStats::default();

        while let Some((range_start, range_end)) = pending.pop() {
            let response_payload_bytes = self.json_hex_payload_bytes(range_start, range_end)?;
            if response_payload_bytes > max_response_bytes {
                let Some(((left_start, left_end), (right_start, right_end))) =
                    split_migration_archive_height_range(range_start, range_end)
                else {
                    return Err(SourceError::SourceResponseTooLarge {
                        operation: "batch_getblock",
                        max_response_bytes,
                    });
                };
                stats = stats.with_added_splits(1);
                pending.push((right_start, right_end));
                pending.push((left_start, left_end));
                continue;
            }

            let mut range_blocks = self.read_connected_blocks(range_start, range_end).await?;
            stats = stats.with_added_response_payload_bytes(response_payload_bytes);
            blocks.append(&mut range_blocks);
        }

        Ok((blocks, stats))
    }
}

type MigrationHeightRange = (BlockHeight, BlockHeight);

fn split_migration_archive_height_range(
    start_height: BlockHeight,
    end_height: BlockHeight,
) -> Option<(MigrationHeightRange, MigrationHeightRange)> {
    if start_height >= end_height {
        return None;
    }
    let midpoint = start_height
        .value()
        .saturating_add(end_height.value().saturating_sub(start_height.value()) / 2);
    let left_end = BlockHeight::new(midpoint);
    let right_start = left_end.next()?;
    Some(((start_height, left_end), (right_start, end_height)))
}

#[async_trait]
impl NodeSource for MigrationArchiveSource {
    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.read_block_task(height)?
            .await
            .map_err(|source| SourceError::BlockUnavailable {
                height,
                reason: format!("migration archive read task failed: {source}"),
            })?
    }

    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError> {
        if !self.segment_response_delay.is_zero() {
            tokio::time::sleep(self.segment_response_delay).await;
        }
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
        let (blocks, stats) = self
            .read_connected_blocks_with_response_limit(
                start_height,
                end_height,
                limits.max_response_bytes,
            )
            .await?;
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
        Ok(SourceChainSegment::connected_blocks_with_stats(
            blocks, stats,
        ))
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

    async fn fetch_subtree_root_range(
        &self,
        range: SubtreeRootRange,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        let subtree_roots = self
            .fetch_subtree_roots(range.protocol, range.start_index, range.max_entries)
            .await?;
        let expected_count = usize::try_from(range.max_entries.get()).unwrap_or(usize::MAX);
        let has_exact_indices = subtree_roots
            .subtree_roots
            .iter()
            .zip(range)
            .all(|(subtree_root, expected_index)| subtree_root.subtree_index == expected_index);
        if subtree_roots.protocol != range.protocol
            || subtree_roots.start_index != range.start_index
            || subtree_roots.subtree_roots.len() != expected_count
            || !has_exact_indices
        {
            return Err(SourceError::SubtreeRootsUnavailable {
                protocol: range.protocol,
                start_index: range.start_index,
                reason: format!(
                    "expected {} contiguous subtree roots, got {}",
                    range.max_entries,
                    subtree_roots.subtree_roots.len()
                ),
            });
        }

        Ok(subtree_roots)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use zinder_core::wire::encode_zinder_native_chain_name;
    use zinder_store::CANONICAL_STORE_SCHEMA_VERSION;

    use super::*;

    #[test]
    fn fixed_layout_round_trips_and_rejects_unexpected_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let archive = temporary.path().join("archive");
        fs::create_dir(&archive)?;
        let manifest = sample_archive(&archive)?;
        manifest.write(&archive)?;

        let admitted = MigrationArchiveManifest::read(&archive)?;
        assert_eq!(admitted.contract_identity, MIGRATION_ARCHIVE_IDENTITY);

        fs::write(archive.join("README"), b"not part of the contract")?;
        assert!(MigrationArchiveManifest::read(&archive).is_err());
        Ok(())
    }

    #[test]
    fn replay_source_rejects_segment_replacement() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let archive = temporary.path().join("archive");
        fs::create_dir(&archive)?;
        let manifest = sample_archive(&archive)?;
        manifest.write(&archive)?;
        fs::write(archive.join(&manifest.segments[0].file), b"replacement")?;

        assert!(MigrationArchiveSource::open(&archive, &manifest).is_err());
        Ok(())
    }

    #[test]
    fn manifest_read_is_bounded_before_json_decode() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let archive = temporary.path().join("archive");
        fs::create_dir(&archive)?;
        let oversized = usize::try_from(MIGRATION_ARCHIVE_MAX_MANIFEST_BYTES)?
            .checked_add(1)
            .ok_or("manifest test length overflow")?;
        fs::write(
            archive.join(MIGRATION_ARCHIVE_MANIFEST_FILE_NAME),
            vec![b'x'; oversized],
        )?;

        assert!(MigrationArchiveManifest::read(&archive).is_err());
        Ok(())
    }

    fn sample_archive(
        archive: &Path,
    ) -> Result<MigrationArchiveManifest, Box<dyn std::error::Error>> {
        let first_hash = BlockHash::from_bytes([0x11; 32]);
        let tip_hash = BlockHash::from_bytes([0x22; 32]);
        let blocks = [
            SourceBlock::new(
                SourceBlockHeader {
                    network: Network::ZcashRegtest,
                    height: BlockHeight::new(0),
                    hash: first_hash,
                    parent_hash: BlockHash::from_bytes([0; 32]),
                    block_time_seconds: 1,
                },
                vec![0x01],
            ),
            SourceBlock::new(
                SourceBlockHeader {
                    network: Network::ZcashRegtest,
                    height: BlockHeight::new(1),
                    hash: tip_hash,
                    parent_hash: first_hash,
                    block_time_seconds: 2,
                },
                vec![0x02],
            ),
        ];
        let segment = write_segment(archive, 0, &blocks)?;
        Ok(MigrationArchiveManifest {
            contract_identity: MIGRATION_ARCHIVE_IDENTITY.to_owned(),
            archive_format_version: MIGRATION_ARCHIVE_FORMAT_VERSION,
            network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
            from_height: 0,
            to_height: 1,
            block_count: 2,
            workload_density: WorkloadDensity {
                block_count: 2,
                raw_block_bytes: 2,
                ..WorkloadDensity::default()
            },
            source_canonical_schema_version: CANONICAL_STORE_SCHEMA_VERSION,
            canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence {
                block_digest_version: CanonicalBlockFactsDigestVersion::CURRENT.value(),
                sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::CURRENT.value(),
                block_count: 1,
                sequence_digest_sha256: hex::encode([0x33; 32]),
            },
            tip_hash_hex: hex::encode(tip_hash.as_bytes()),
            network_upgrade_activations: Vec::new(),
            segments: vec![segment],
            subtree_roots: SubtreeRootSet::default(),
        })
    }
}
