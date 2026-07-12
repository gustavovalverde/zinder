//! On-disk fixed-range fixture: manifest, segment framing, and a
//! [`NodeSource`] that serves captured payloads for a deterministic replay.

use std::{
    collections::HashMap,
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
    BlockHash, BlockHeight, BlockId, ConsensusBranchId, Network, NetworkUpgradeActivation,
    NetworkUpgradeActivations, ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex,
    wire::decode_zinder_native_chain_name,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceChainSegment,
    SourceChainSegmentLimits, SourceChainUpdate, SourceError, SourceSubtreeRoot,
    SourceSubtreeRoots,
};

use crate::error::BenchError;

/// Version stamped into every manifest this crate writes.
pub const FIXTURE_FORMAT_VERSION: u32 = 1;

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
    /// Canonical artifact schema version at capture time.
    pub artifact_schema_version: u16,
    /// Hash of the block at `to_height`, hex-encoded, used as the replay tip.
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
        Ok(manifest)
    }

    /// Writes the manifest into a fixture directory as pretty JSON.
    pub fn write(&self, directory: &Path) -> Result<(), BenchError> {
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        let encoded = serde_json::to_vec_pretty(self)?;
        std::fs::write(&manifest_path, encoded)
            .map_err(|source| BenchError::io(&manifest_path, source))
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
        let hash = decode_block_hash_hex(&self.tip_hash_hex)?;
        Ok(BlockId::new(BlockHeight::new(self.to_height), hash))
    }
}

fn decode_block_hash_hex(encoded: &str) -> Result<BlockHash, BenchError> {
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
    let segment_path = directory.join(&descriptor.file);
    let file = File::open(&segment_path).map_err(|source| BenchError::io(&segment_path, source))?;
    let mut reader = BufReader::new(file);
    read_and_verify_magic(&mut reader, &segment_path)?;

    let mut blocks = Vec::with_capacity(descriptor.block_count as usize);
    for _ in 0..descriptor.block_count {
        let (height, byte_len) = read_record_header(&mut reader, &segment_path)?;
        let mut raw_block_bytes = vec![0_u8; byte_len as usize];
        reader
            .read_exact(&mut raw_block_bytes)
            .map_err(|source| BenchError::io(&segment_path, source))?;
        let block =
            SourceBlock::from_raw_block_bytes(network, BlockHeight::new(height), raw_block_bytes)?;
        blocks.push(block);
    }
    Ok(blocks)
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
        let file =
            File::open(&segment_path).map_err(|source| BenchError::io(&segment_path, source))?;
        let mut reader = BufReader::new(file);
        read_and_verify_magic(&mut reader, &segment_path)?;
        let mut position = u64::from(u32::try_from(SEGMENT_MAGIC.len()).unwrap_or(u32::MAX));
        for _ in 0..descriptor.block_count {
            let (height, byte_len) = read_record_header(&mut reader, &segment_path)?;
            let byte_offset = position + RECORD_HEADER_LEN;
            locations.insert(
                height,
                BlockLocation {
                    segment_path: segment_path.clone(),
                    byte_offset,
                    byte_len,
                },
            );
            reader
                .seek(SeekFrom::Current(i64::from(byte_len)))
                .map_err(|source| BenchError::io(&segment_path, source))?;
            position = byte_offset + u64::from(byte_len);
        }
    }
    Ok(locations)
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
    SourceBlock::from_raw_block_bytes(network, height, raw_block_bytes)
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
