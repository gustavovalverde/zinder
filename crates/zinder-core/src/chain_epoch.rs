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

    /// Returns the authoritative height-zero block hash in internal byte order.
    #[must_use]
    pub const fn genesis_hash(self) -> BlockHash {
        match self {
            Self::ZcashMainnet => BlockHash::from_bytes([
                0x08, 0xce, 0x3d, 0x97, 0x31, 0xb0, 0x00, 0xc0, 0x83, 0x38, 0x45, 0x5c, 0x8a, 0x4a,
                0x6b, 0xd0, 0x5d, 0xa1, 0x6e, 0x26, 0xb1, 0x1d, 0xaa, 0x1b, 0x91, 0x71, 0x84, 0xec,
                0xe8, 0x0f, 0x04, 0x00,
            ]),
            Self::ZcashTestnet => BlockHash::from_bytes([
                0x38, 0x2c, 0x4a, 0x33, 0x26, 0x61, 0xc7, 0xed, 0x06, 0x71, 0xf3, 0x2a, 0x34, 0xd7,
                0x24, 0x61, 0x9f, 0x08, 0x6c, 0x61, 0x87, 0x3b, 0xce, 0x7c, 0x99, 0x85, 0x9d, 0xd9,
                0x92, 0x0a, 0xa6, 0x05,
            ]),
            Self::ZcashRegtest => BlockHash::from_bytes([
                0x27, 0xe3, 0x01, 0x34, 0xd6, 0x20, 0xe9, 0xfe, 0x61, 0xf7, 0x19, 0x93, 0x83, 0x20,
                0xba, 0xb6, 0x3e, 0x7e, 0x72, 0xc9, 0x1b, 0x5e, 0x23, 0x02, 0x56, 0x76, 0xf9, 0x0e,
                0xd8, 0x11, 0x9f, 0x02,
            ]),
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

    /// Returns the next block height, or `None` when already at `u32::MAX`.
    ///
    /// Returning `Option` instead of saturating keeps callers honest at
    /// the rollover edge: a loop walking forward through chain heights
    /// should terminate when it hits the ceiling, not silently re-fetch
    /// the same height.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
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

    /// Returns the wall-clock Unix millisecond timestamp.
    ///
    /// Infallible: clocks before [`std::time::UNIX_EPOCH`] saturate to 0,
    /// and millisecond counts past `u64::MAX` saturate to `u64::MAX`.
    #[must_use]
    pub fn now() -> Self {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        Self(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
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
    /// Ironwood note commitment tree size at the visible chain tip.
    pub ironwood_commitment_tree_size: u32,
}

impl ChainTipMetadata {
    /// Creates chain-tip metadata from shielded note commitment tree sizes.
    #[must_use]
    pub const fn new(
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
        ironwood_commitment_tree_size: u32,
    ) -> Self {
        Self {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size,
            ironwood_commitment_tree_size,
        }
    }

    /// Returns empty chain-tip metadata for epochs with no shielded commitments.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(0, 0, 0)
    }

    /// Returns the note commitment tree size for `protocol`.
    #[must_use]
    pub const fn commitment_tree_size(self, protocol: ShieldedProtocol) -> u32 {
        match protocol {
            ShieldedProtocol::Sapling => self.sapling_commitment_tree_size,
            ShieldedProtocol::Orchard => self.orchard_commitment_tree_size,
            ShieldedProtocol::Ironwood => self.ironwood_commitment_tree_size,
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
    pub visible_tip_height: BlockHeight,
    /// Best visible tip hash for this chain epoch.
    pub visible_tip_hash: BlockHash,
    /// Reorg-window finality watermark. Wallets scan through `visible_tip_height`
    /// under the pinned epoch and use this lower height for settlement-sensitive policy.
    pub settled_tip_height: BlockHeight,
    /// Block hash at `settled_tip_height`.
    pub settled_tip_hash: BlockHash,
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

    /// Creates an empty range anchored at `height`.
    ///
    /// Empty ranges represent chain events that advance epoch metadata
    /// without publishing new or replacement block artifacts. The iterator
    /// yields no heights because `start > end`.
    #[must_use]
    pub const fn empty_at(height: BlockHeight) -> Self {
        if height.0 == u32::MAX {
            Self {
                start: height,
                end: BlockHeight(height.0 - 1),
            }
        } else {
            Self {
                start: BlockHeight(height.0 + 1),
                end: height,
            }
        }
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
