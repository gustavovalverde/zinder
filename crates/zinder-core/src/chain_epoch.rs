/// Zcash network served by a Zinder store or service instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Network {
    /// Zcash mainnet.
    ZcashMainnet,
    /// Zcash testnet.
    ZcashTestnet,
    /// Local Zcash regtest.
    ZcashRegtest,
}

impl Network {
    /// Returns the stable numeric network identifier used in storage keys.
    #[must_use]
    pub const fn id(self) -> u32 {
        match self {
            Self::ZcashMainnet => 1,
            Self::ZcashTestnet => 2,
            Self::ZcashRegtest => 3,
        }
    }

    /// Returns the public configuration name for this network.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ZcashMainnet => "zcash-mainnet",
            Self::ZcashTestnet => "zcash-testnet",
            Self::ZcashRegtest => "zcash-regtest",
        }
    }

    /// Resolves a storage key network identifier into a known network.
    #[must_use]
    pub const fn from_id(network_id: u32) -> Option<Self> {
        match network_id {
            1 => Some(Self::ZcashMainnet),
            2 => Some(Self::ZcashTestnet),
            3 => Some(Self::ZcashRegtest),
            _ => None,
        }
    }

    /// Resolves a public configuration name into a known network.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "zcash-mainnet" => Some(Self::ZcashMainnet),
            "zcash-testnet" => Some(Self::ZcashTestnet),
            "zcash-regtest" => Some(Self::ZcashRegtest),
            _ => None,
        }
    }
}

/// Monotonic identifier for a visible chain epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChainEpochId(u64);

impl ChainEpochId {
    /// Creates a chain epoch identifier from its numeric value.
    #[must_use]
    pub const fn new(chain_epoch: u64) -> Self {
        Self(chain_epoch)
    }

    /// Returns the numeric chain epoch value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Block height in the Zcash chain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockHeight(u32);

impl BlockHeight {
    /// Creates a block height from its numeric value.
    #[must_use]
    pub const fn new(height: u32) -> Self {
        Self(height)
    }

    /// Returns the numeric block height.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

/// Zcash block hash bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlockHash([u8; 32]);

impl BlockHash {
    /// Creates a block hash from canonical 32-byte hash material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the block hash bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Version of the durable artifact schema used by a chain epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactSchemaVersion(u16);

impl ArtifactSchemaVersion {
    /// Creates an artifact schema version.
    #[must_use]
    pub const fn new(version: u16) -> Self {
        Self(version)
    }

    /// Returns the numeric artifact schema version.
    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// Unix timestamp in milliseconds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixTimestampMillis(u64);

impl UnixTimestampMillis {
    /// Creates a Unix millisecond timestamp.
    #[must_use]
    pub const fn new(timestamp_millis: u64) -> Self {
        Self(timestamp_millis)
    }

    /// Returns the Unix millisecond timestamp.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

use crate::{SUBTREE_LEAF_COUNT, ShieldedProtocol};

/// Tip metadata needed by wallet hot paths but independent of wallet protocol bytes.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ChainTipMetadata {
    /// Sapling note commitment tree size at the visible chain tip.
    pub sapling_commitment_tree_size: u32,
    /// Orchard note commitment tree size at the visible chain tip.
    pub orchard_commitment_tree_size: u32,
}

impl ChainTipMetadata {
    /// Creates chain-tip metadata from shielded note commitment tree sizes.
    #[must_use]
    pub const fn new(sapling_commitment_tree_size: u32, orchard_commitment_tree_size: u32) -> Self {
        Self {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size,
        }
    }

    /// Returns empty chain-tip metadata for epochs with no shielded commitments.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(0, 0)
    }

    /// Returns the note commitment tree size for `protocol`.
    #[must_use]
    pub const fn commitment_tree_size(self, protocol: ShieldedProtocol) -> u32 {
        match protocol {
            ShieldedProtocol::Sapling => self.sapling_commitment_tree_size,
            ShieldedProtocol::Orchard => self.orchard_commitment_tree_size,
        }
    }

    /// Returns the number of complete subtree-root shards for `protocol`.
    #[must_use]
    pub const fn completed_subtree_count(self, protocol: ShieldedProtocol) -> u32 {
        self.commitment_tree_size(protocol) / SUBTREE_LEAF_COUNT
    }
}

/// Visible, internally consistent chain snapshot exposed to readers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChainEpoch {
    /// Monotonic identifier for this visible chain epoch.
    pub id: ChainEpochId,
    /// Network this chain epoch belongs to.
    pub network: Network,
    /// Best visible tip height for this chain epoch.
    pub tip_height: BlockHeight,
    /// Best visible tip hash for this chain epoch.
    pub tip_hash: BlockHash,
    /// Finalized height for this chain epoch.
    pub finalized_height: BlockHeight,
    /// Finalized block hash for this chain epoch.
    pub finalized_hash: BlockHash,
    /// Artifact schema version used by artifacts in this chain epoch.
    pub artifact_schema_version: ArtifactSchemaVersion,
    /// Chain-derived metadata at the visible tip.
    pub tip_metadata: ChainTipMetadata,
    /// Wall-clock creation time for this chain epoch.
    ///
    /// This timestamp is diagnostic metadata, not an ordering primitive. Use
    /// [`ChainEpochId`] or the chain-event sequence for monotonic ordering.
    pub created_at: UnixTimestampMillis,
}

/// Inclusive block-height range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlockHeightRange {
    /// First height in the inclusive range.
    pub start: BlockHeight,
    /// Last height in the inclusive range.
    pub end: BlockHeight,
}

impl BlockHeightRange {
    /// Creates an inclusive block-height range.
    #[must_use]
    pub const fn inclusive(start: BlockHeight, end: BlockHeight) -> Self {
        Self { start, end }
    }
}

impl IntoIterator for BlockHeightRange {
    type Item = BlockHeight;
    type IntoIter = BlockHeightRangeIter;

    fn into_iter(self) -> Self::IntoIter {
        BlockHeightRangeIter {
            next: self.start.value(),
            end: self.end.value(),
            done: self.start > self.end,
        }
    }
}

/// Iterator over an inclusive [`BlockHeightRange`].
#[derive(Clone, Debug)]
pub struct BlockHeightRangeIter {
    next: u32,
    end: u32,
    done: bool,
}

impl Iterator for BlockHeightRangeIter {
    type Item = BlockHeight;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let current = BlockHeight::new(self.next);
        if self.next == self.end {
            self.done = true;
        } else {
            self.next += 1;
        }

        Some(current)
    }
}

impl ExactSizeIterator for BlockHeightRangeIter {
    fn len(&self) -> usize {
        if self.done {
            return 0;
        }

        u32_to_usize(self.end.saturating_sub(self.next).saturating_add(1))
    }
}

impl std::iter::FusedIterator for BlockHeightRangeIter {}

#[allow(
    clippy::cast_possible_truncation,
    reason = "crate-level cfg rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
