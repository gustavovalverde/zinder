//! Deterministic in-memory chain shapes for Zinder tests.
//!
//! A [`ChainFixture`] is a sequence of synthetic blocks at heights 1..=N. The
//! fixture's hashes, timestamps, and payloads are derived from the height and
//! a per-fork salt so that two fixtures with the same constructor calls
//! produce byte-identical artifacts. Fork helpers branch the chain at a given
//! height with a salt-perturbed hash space so reorg tests can compare two
//! shapes that share a common prefix.
//!
//! Fixture artifacts are *not* parseable by `zebra_chain`; they exist for
//! tests that bypass the artifact builder (validating storage, query, or
//! protocol surfaces). Tests that exercise `derive_*_artifact` should pull
//! real Zcash bytes from `services/zinder-ingest/tests/fixtures/`.

use prost::Message;
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash,
    SubtreeRootIndex, TransactionArtifact, TransparentAddressUtxoArtifact,
    TransparentUtxoSpendArtifact, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_proto::compat::lightwalletd::{ChainMetadata, CompactBlock as LightwalletdCompactBlock};
use zinder_source::{SourceBlock, SourceBlockHeader};
use zinder_store::{CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ReorgWindowChange};

const FIXTURE_GENESIS_TIMESTAMP_SECONDS: u32 = 1_774_668_400;
const FIXTURE_HASH_HEIGHT_MIX: u32 = 0x9e37_79b9;
const FIXTURE_TREE_STATE_PAYLOAD: &[u8] =
    br#"{"sapling":{"commitments":{"size":0}},"orchard":{"commitments":{"size":0}}}"#;
const FIXTURE_LIGHTWALLETD_PROTO_VERSION: u32 = 1;
const FIXTURE_CHAIN_EPOCH_CREATED_AT_MILLIS: u64 = 1_774_669_000_000;

/// One synthetic block in a [`ChainFixture`].
///
/// Field values are derived from the block height and the parent fixture's
/// fork salt; tests can read them directly to assert against the canonical
/// artifacts produced by the fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureBlock {
    /// Block height. Heights start at 1.
    pub height: BlockHeight,
    /// Synthetic block hash derived from height and fork salt.
    pub hash: BlockHash,
    /// Hash of the previous block in the same fixture branch.
    pub parent_hash: BlockHash,
    /// Block timestamp in Unix seconds. Monotonic with height.
    pub block_time_seconds: u32,
    /// Placeholder raw block bytes. Not parseable by `zebra_chain`.
    pub raw_block_bytes: Vec<u8>,
    /// Tree-state JSON payload bytes attached to this block.
    pub tree_state_payload_bytes: Vec<u8>,
    /// Optional override for the compact-block payload; used by tests that
    /// need a fully-populated lightwalletd `CompactBlock` shape.
    pub compact_block_payload_override: Option<Vec<u8>>,
}

impl FixtureBlock {
    /// Returns the durable [`BlockArtifact`] for this fixture block.
    #[must_use]
    pub fn block_artifact(&self) -> BlockArtifact {
        BlockArtifact::new(
            self.height,
            self.hash,
            self.parent_hash,
            self.raw_block_bytes.clone(),
        )
    }

    /// Returns the compact-block artifact for this fixture block.
    ///
    /// The compact block is encoded as a lightwalletd `CompactBlock` with the
    /// fixture's hash, parent hash, time, and an empty transaction list, or
    /// uses [`Self::compact_block_payload_override`] when present.
    #[must_use]
    pub fn compact_block_artifact(&self) -> CompactBlockArtifact {
        let payload_bytes = self
            .compact_block_payload_override
            .as_ref()
            .map_or_else(|| self.default_compact_block_payload(), Clone::clone);

        CompactBlockArtifact::new(self.height, self.hash, payload_bytes)
    }

    fn default_compact_block_payload(&self) -> Vec<u8> {
        LightwalletdCompactBlock {
            proto_version: FIXTURE_LIGHTWALLETD_PROTO_VERSION,
            height: u64::from(self.height.value()),
            hash: self.hash.as_bytes().to_vec(),
            prev_hash: self.parent_hash.as_bytes().to_vec(),
            time: self.block_time_seconds,
            header: Vec::new(),
            vtx: Vec::new(),
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
            }),
        }
        .encode_to_vec()
    }

    /// Returns the node-shaped [`SourceBlock`] for this fixture block.
    #[must_use]
    pub fn source_block(&self, network: Network) -> SourceBlock {
        SourceBlock::new(
            SourceBlockHeader {
                network,
                height: self.height,
                hash: self.hash,
                parent_hash: self.parent_hash,
                block_time_seconds: self.block_time_seconds,
            },
            self.raw_block_bytes.clone(),
        )
        .with_tree_state_payload_bytes(self.tree_state_payload_bytes.clone())
    }

    /// Returns the tree-state artifact for this fixture block.
    #[must_use]
    pub fn tree_state_artifact(&self) -> TreeStateArtifact {
        TreeStateArtifact::new(
            self.height,
            self.hash,
            self.tree_state_payload_bytes.clone(),
        )
    }
}

/// Deterministic in-memory chain of synthetic blocks.
///
/// Use [`ChainFixture::new`] to start an empty chain and
/// [`ChainFixture::extend_blocks`] to append height-by-height. Use
/// [`ChainFixture::fork_at`] to branch a divergent variant that shares an
/// ancestor prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainFixture {
    network: Network,
    branch_salt: u32,
    blocks: Vec<FixtureBlock>,
    tip_metadata_override: Option<ChainTipMetadata>,
    sapling_subtree_roots: Vec<SubtreeRootArtifact>,
    transaction_artifacts: Vec<TransactionArtifact>,
    transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
}

impl ChainFixture {
    /// Creates an empty fixture for the given network.
    #[must_use]
    pub const fn new(network: Network) -> Self {
        Self {
            network,
            branch_salt: 0,
            blocks: Vec::new(),
            tip_metadata_override: None,
            sapling_subtree_roots: Vec::new(),
            transaction_artifacts: Vec::new(),
            transparent_address_utxos: Vec::new(),
            transparent_utxo_spends: Vec::new(),
        }
    }

    /// Appends `count` synthetic blocks to the fixture, in increasing height
    /// order, starting from the current tip + 1.
    #[must_use]
    pub fn extend_blocks(mut self, count: u32) -> Self {
        let next_starting_height = self
            .blocks
            .last()
            .map_or(1, |last| last.height.value().saturating_add(1));

        for offset in 0..count {
            let height_value = next_starting_height.saturating_add(offset);
            let height = BlockHeight::new(height_value);
            let parent_hash = self
                .blocks
                .last()
                .map_or_else(|| synthetic_block_hash(0, 0), |last| last.hash);
            let hash = synthetic_block_hash(height_value, self.branch_salt);
            let block_time_seconds = FIXTURE_GENESIS_TIMESTAMP_SECONDS.saturating_add(height_value);
            let raw_block_bytes = format!("zinder-testkit-block-{height_value}").into_bytes();

            self.blocks.push(FixtureBlock {
                height,
                hash,
                parent_hash,
                block_time_seconds,
                raw_block_bytes,
                tree_state_payload_bytes: FIXTURE_TREE_STATE_PAYLOAD.to_vec(),
                compact_block_payload_override: None,
            });
        }

        self
    }

    /// Returns a new fixture that shares blocks `< divergence_height` with
    /// `self` and is empty from `divergence_height` onwards.
    ///
    /// Subsequent calls to [`ChainFixture::extend_blocks`] on the returned
    /// fixture build a divergent branch with hashes that differ from the
    /// parent fixture for every height at or above `divergence_height`.
    ///
    /// # Errors
    ///
    /// Returns an error if `divergence_height` is greater than the parent
    /// fixture's tip height + 1, or if the parent fixture is empty.
    pub fn fork_at(&self, divergence_height: BlockHeight) -> Result<Self, ChainFixtureError> {
        let Some(tip_height) = self.tip_height() else {
            return Err(ChainFixtureError::ForkBeforeGenesis);
        };
        let max_divergence_height = tip_height.value().saturating_add(1);
        if divergence_height.value() > max_divergence_height {
            return Err(ChainFixtureError::ForkAboveTip {
                requested: divergence_height,
                tip_plus_one: BlockHeight::new(max_divergence_height),
            });
        }
        if divergence_height.value() == 0 {
            return Err(ChainFixtureError::ForkAtGenesis);
        }

        let prefix_blocks = self
            .blocks
            .iter()
            .take_while(|fixture_block| fixture_block.height.value() < divergence_height.value())
            .cloned()
            .collect::<Vec<_>>();

        Ok(Self {
            network: self.network,
            branch_salt: self.branch_salt.wrapping_add(1).max(1),
            blocks: prefix_blocks,
            tip_metadata_override: None,
            sapling_subtree_roots: Vec::new(),
            transaction_artifacts: Vec::new(),
            transparent_address_utxos: Vec::new(),
            transparent_utxo_spends: Vec::new(),
        })
    }

    /// Overrides the [`ChainTipMetadata`] reported on the tip [`ChainEpoch`].
    ///
    /// Useful for tests that exercise subtree-root behavior at large tip
    /// commitment-tree sizes without committing thousands of synthetic
    /// outputs.
    #[must_use]
    pub const fn with_tip_metadata_override(mut self, tip_metadata: ChainTipMetadata) -> Self {
        self.tip_metadata_override = Some(tip_metadata);
        self
    }

    /// Replaces the tree-state payload bytes attached to the block at `height`.
    ///
    /// Returns the fixture unchanged if no block exists at `height`.
    #[must_use]
    pub fn with_tree_state_payload_at(
        mut self,
        height: BlockHeight,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        let payload_bytes = payload_bytes.into();
        for block in &mut self.blocks {
            if block.height == height {
                block.tree_state_payload_bytes = payload_bytes;
                break;
            }
        }
        self
    }

    /// Replaces the compact-block payload bytes for the block at `height`.
    ///
    /// Useful when a test needs a fully-populated [`CompactBlockArtifact`]
    /// shape that the default builder does not produce.
    ///
    /// Returns the fixture unchanged if no block exists at `height`.
    #[must_use]
    pub fn with_compact_block_payload_at(
        mut self,
        height: BlockHeight,
        compact_block_payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        let payload_bytes = compact_block_payload_bytes.into();
        for block in &mut self.blocks {
            if block.height == height {
                block.compact_block_payload_override = Some(payload_bytes);
                break;
            }
        }
        self
    }

    /// Adds a Sapling subtree-root artifact to this fixture.
    ///
    /// Subtree roots are emitted by [`ChainFixture::chain_epoch_artifacts`]
    /// alongside the synthetic Sapling root produced by
    /// [`ChainFixture::synthetic_sapling_subtree_root`] when the chain is
    /// non-empty.
    #[must_use]
    pub fn with_sapling_subtree_root(mut self, subtree_root: SubtreeRootArtifact) -> Self {
        self.sapling_subtree_roots.push(subtree_root);
        self
    }

    /// Attaches a [`TransactionArtifact`] to this fixture's commit set.
    ///
    /// Transactions added via this builder are included in
    /// [`ChainFixture::chain_epoch_artifacts`] so the resulting
    /// [`ChainEpochArtifacts`] can be committed in one batch.
    #[must_use]
    pub fn with_transaction_artifact(mut self, transaction: TransactionArtifact) -> Self {
        self.transaction_artifacts.push(transaction);
        self
    }

    /// Attaches a [`TransparentAddressUtxoArtifact`] to this fixture's commit set.
    #[must_use]
    pub fn with_transparent_address_utxo(
        mut self,
        transparent_address_utxo: TransparentAddressUtxoArtifact,
    ) -> Self {
        self.transparent_address_utxos
            .push(transparent_address_utxo);
        self
    }

    /// Attaches a [`TransparentUtxoSpendArtifact`] to this fixture's commit set.
    #[must_use]
    pub fn with_transparent_utxo_spend(
        mut self,
        transparent_utxo_spend: TransparentUtxoSpendArtifact,
    ) -> Self {
        self.transparent_utxo_spends.push(transparent_utxo_spend);
        self
    }

    /// Returns the network this fixture was built for.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Returns the fixture's tip height, or `None` for an empty fixture.
    #[must_use]
    pub fn tip_height(&self) -> Option<BlockHeight> {
        self.blocks.last().map(|fixture_block| fixture_block.height)
    }

    /// Returns the fixture's tip hash, or `None` for an empty fixture.
    #[must_use]
    pub fn tip_hash(&self) -> Option<BlockHash> {
        self.blocks.last().map(|fixture_block| fixture_block.hash)
    }

    /// Returns the count of blocks in this fixture.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Returns a borrowed view of every block in this fixture, in ascending
    /// height order.
    #[must_use]
    pub fn blocks(&self) -> &[FixtureBlock] {
        &self.blocks
    }

    /// Returns the fixture block at `height`, or `None` if absent.
    #[must_use]
    pub fn block_at(&self, height: BlockHeight) -> Option<&FixtureBlock> {
        self.blocks
            .iter()
            .find(|fixture_block| fixture_block.height == height)
    }

    /// Returns the [`SourceBlock`] for `height`, or `None` if absent.
    #[must_use]
    pub fn source_block_at(&self, height: BlockHeight) -> Option<SourceBlock> {
        self.block_at(height)
            .map(|fixture_block| fixture_block.source_block(self.network))
    }

    /// Returns every block as a [`BlockArtifact`] in ascending height order.
    #[must_use]
    pub fn block_artifacts(&self) -> Vec<BlockArtifact> {
        self.blocks
            .iter()
            .map(FixtureBlock::block_artifact)
            .collect()
    }

    /// Returns every block as a [`CompactBlockArtifact`] in ascending height order.
    #[must_use]
    pub fn compact_block_artifacts(&self) -> Vec<CompactBlockArtifact> {
        self.blocks
            .iter()
            .map(FixtureBlock::compact_block_artifact)
            .collect()
    }

    /// Returns every block as a [`TreeStateArtifact`] in ascending height order.
    #[must_use]
    pub fn tree_state_artifacts(&self) -> Vec<TreeStateArtifact> {
        self.blocks
            .iter()
            .map(FixtureBlock::tree_state_artifact)
            .collect()
    }

    /// Returns one synthetic Sapling subtree-root artifact rooted at the
    /// fixture tip, or `None` for an empty fixture. Useful as a placeholder
    /// for tests that exercise the subtree-root read path.
    #[must_use]
    pub fn synthetic_sapling_subtree_root(&self) -> Option<SubtreeRootArtifact> {
        let tip_block = self.blocks.last()?;
        Some(SubtreeRootArtifact::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            SubtreeRootHash::from_bytes([0x07; 32]),
            tip_block.height,
            tip_block.hash,
        ))
    }

    /// Builds the [`ChainEpoch`] descriptor that names this fixture as a
    /// canonical commit input.
    ///
    /// Returns `None` for an empty fixture; canonical chain epochs require at
    /// least one block.
    #[must_use]
    pub fn chain_epoch(&self, epoch_id: ChainEpochId) -> Option<ChainEpoch> {
        let tip_block = self.blocks.last()?;
        Some(ChainEpoch {
            id: epoch_id,
            network: self.network,
            tip_height: tip_block.height,
            tip_hash: tip_block.hash,
            finalized_height: tip_block.height,
            finalized_hash: tip_block.hash,
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: self
                .tip_metadata_override
                .unwrap_or_else(ChainTipMetadata::empty),
            created_at: UnixTimestampMillis::new(FIXTURE_CHAIN_EPOCH_CREATED_AT_MILLIS),
        })
    }

    /// Builds a [`ChainEpochArtifacts`] commit value covering every block in
    /// this fixture.
    ///
    /// Returns `None` for an empty fixture.
    #[must_use]
    pub fn chain_epoch_artifacts(&self, epoch_id: ChainEpochId) -> Option<ChainEpochArtifacts> {
        let chain_epoch = self.chain_epoch(epoch_id)?;
        let block_artifacts = self.block_artifacts();
        let compact_block_artifacts = self.compact_block_artifacts();
        let tree_state_artifacts = self.tree_state_artifacts();
        let subtree_root_artifacts = self.subtree_root_artifacts();

        let block_range =
            zinder_core::BlockHeightRange::inclusive(BlockHeight::new(1), chain_epoch.tip_height);

        let mut chain_epoch_artifacts =
            ChainEpochArtifacts::new(chain_epoch, block_artifacts, compact_block_artifacts)
                .with_tree_states(tree_state_artifacts)
                .with_subtree_roots(subtree_root_artifacts)
                .with_reorg_window_change(ReorgWindowChange::Extend { block_range });
        if !self.transaction_artifacts.is_empty() {
            chain_epoch_artifacts =
                chain_epoch_artifacts.with_transactions(self.transaction_artifacts.clone());
        }
        if !self.transparent_address_utxos.is_empty() {
            chain_epoch_artifacts = chain_epoch_artifacts
                .with_transparent_address_utxos(self.transparent_address_utxos.clone());
        }
        if !self.transparent_utxo_spends.is_empty() {
            chain_epoch_artifacts = chain_epoch_artifacts
                .with_transparent_utxo_spends(self.transparent_utxo_spends.clone());
        }
        Some(chain_epoch_artifacts)
    }

    /// Returns the subtree-root artifacts associated with this fixture in
    /// commit-ready order.
    ///
    /// Defaults to one synthetic Sapling root rooted at the tip when the
    /// fixture is non-empty and no explicit roots have been added. Calling
    /// [`ChainFixture::with_sapling_subtree_root`] replaces the synthetic
    /// default with the explicit set.
    #[must_use]
    pub fn subtree_root_artifacts(&self) -> Vec<SubtreeRootArtifact> {
        if !self.sapling_subtree_roots.is_empty() {
            return self.sapling_subtree_roots.clone();
        }
        self.synthetic_sapling_subtree_root()
            .map(|subtree_root| vec![subtree_root])
            .unwrap_or_default()
    }
}

/// Errors raised when building a [`ChainFixture`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ChainFixtureError {
    /// Cannot fork an empty fixture; build the parent prefix first.
    #[error("cannot fork an empty chain fixture")]
    ForkBeforeGenesis,
    /// Fork height must be at least 1; the genesis block is not represented.
    #[error("cannot fork a chain fixture at the genesis (height 0)")]
    ForkAtGenesis,
    /// Fork height exceeds the parent fixture's tip + 1.
    #[error("fork height {requested:?} exceeds parent tip + 1 ({tip_plus_one:?})")]
    ForkAboveTip {
        /// Requested divergence height.
        requested: BlockHeight,
        /// Parent fixture's tip height + 1.
        tip_plus_one: BlockHeight,
    },
}

fn synthetic_block_hash(height: u32, branch_salt: u32) -> BlockHash {
    let mixed_word = height.wrapping_mul(FIXTURE_HASH_HEIGHT_MIX) ^ branch_salt;
    let mixed_bytes = mixed_word.to_be_bytes();
    let mut hash_bytes = [0u8; 32];
    for hash_chunk in hash_bytes.chunks_exact_mut(4) {
        hash_chunk.copy_from_slice(&mixed_bytes);
    }
    BlockHash::from_bytes(hash_bytes)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{ChainFixture, ChainFixtureError, synthetic_block_hash};
    use zinder_core::{BlockHeight, ChainEpochId, Network};

    #[test]
    fn extend_blocks_links_parent_hashes() -> Result<(), Box<dyn Error>> {
        let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(5);

        assert_eq!(fixture.block_count(), 5);
        for height_value in 1..=5_u32 {
            let block = fixture
                .block_at(BlockHeight::new(height_value))
                .ok_or("block must exist at every extended height")?;
            let expected_parent_hash = if height_value == 1 {
                synthetic_block_hash(0, 0)
            } else {
                synthetic_block_hash(height_value - 1, 0)
            };
            assert_eq!(block.parent_hash, expected_parent_hash);
        }
        Ok(())
    }

    #[test]
    fn fork_at_shares_prefix_and_diverges_above() -> Result<(), Box<dyn Error>> {
        let main_branch = ChainFixture::new(Network::ZcashRegtest).extend_blocks(5);
        let alternate_branch = main_branch.fork_at(BlockHeight::new(3))?.extend_blocks(2);

        let main_block_2 = main_branch
            .block_at(BlockHeight::new(2))
            .ok_or("main fixture must contain block 2")?;
        let alternate_block_2 = alternate_branch
            .block_at(BlockHeight::new(2))
            .ok_or("alternate fixture must contain block 2")?;
        assert_eq!(
            main_block_2.hash, alternate_block_2.hash,
            "shared prefix must produce identical hashes below the divergence height"
        );

        let main_block_4 = main_branch
            .block_at(BlockHeight::new(4))
            .ok_or("main fixture must contain block 4")?;
        let alternate_block_4 = alternate_branch
            .block_at(BlockHeight::new(4))
            .ok_or("alternate fixture must contain block 4")?;
        assert_ne!(
            main_block_4.hash, alternate_block_4.hash,
            "fork branch must produce a distinct hash at every divergent height"
        );

        Ok(())
    }

    #[test]
    fn fork_at_zero_returns_genesis_error() {
        let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
        assert_eq!(
            fixture.fork_at(BlockHeight::new(0)),
            Err(ChainFixtureError::ForkAtGenesis)
        );
    }

    #[test]
    fn fork_at_above_tip_plus_one_returns_error() {
        let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
        assert_eq!(
            fixture.fork_at(BlockHeight::new(5)),
            Err(ChainFixtureError::ForkAboveTip {
                requested: BlockHeight::new(5),
                tip_plus_one: BlockHeight::new(4),
            })
        );
    }

    #[test]
    fn fork_on_empty_fixture_is_rejected() {
        let fixture = ChainFixture::new(Network::ZcashRegtest);
        assert_eq!(
            fixture.fork_at(BlockHeight::new(1)),
            Err(ChainFixtureError::ForkBeforeGenesis)
        );
    }

    #[test]
    fn chain_epoch_artifacts_cover_every_block() -> Result<(), Box<dyn Error>> {
        let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(4);
        let chain_epoch_artifacts = fixture
            .chain_epoch_artifacts(ChainEpochId::new(7))
            .ok_or("chain epoch artifacts should be available for a 4-block fixture")?;

        assert_eq!(chain_epoch_artifacts.finalized_blocks.len(), 4);
        assert_eq!(chain_epoch_artifacts.compact_blocks.len(), 4);
        assert_eq!(chain_epoch_artifacts.tree_states.len(), 4);
        assert_eq!(chain_epoch_artifacts.chain_epoch.tip_height.value(), 4);
        assert_eq!(
            chain_epoch_artifacts.chain_epoch.network,
            Network::ZcashRegtest
        );

        Ok(())
    }
}
