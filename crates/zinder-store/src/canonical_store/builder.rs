use std::{ffi::OsString, fs, path::PathBuf};

use zinder_core::{Network, NetworkUpgradeActivationsFingerprint};

use crate::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget, open_bounded_rocksdb,
};

use super::{
    CanonicalStoreBuildError, CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload,
    block_load::{
        CANONICAL_BLOCK_SST_TARGET_LOGICAL_BYTES, CanonicalBlockLoadEvidence,
        CanonicalBlockSstConfig, CanonicalBuildBlock, REVERSE_INDEX_SORT_MEMORY_BYTES,
        canonical_block_families_are_empty, ingest_canonical_block_ssts,
        validate_source_tip_checkpoint, write_canonical_block_ssts,
    },
    rocksdb::{
        canonical_column_family_descriptors, canonical_data_options, canonical_store_path,
        create_fresh_directory, initialize_store_identity, validate_resource_budget,
    },
    subtree_load::{
        CanonicalBuildSubtreeRoot, CanonicalSubtreeRootLoadEvidence, load_subtree_roots,
        required_subtree_root_ranges,
    },
};

/// Exclusive owner of one fresh, unpublished canonical version-1 build.
///
/// A builder cannot open an existing path. Any crash residue or partially
/// populated family requires deletion of the whole build before retrying.
pub struct RocksDbCanonicalBuilder {
    pub(super) store_path: PathBuf,
    pub(super) bounded_open: BoundedRocksDbOpen,
    pub(super) resource_budget: RocksDbResourceBudget,
    pub(super) network: Network,
    pub(super) workload: CanonicalStoreWorkload,
    pub(super) build_plan: CanonicalStoreBuildPlan,
    pub(super) canonical_block_evidence: Option<CanonicalBlockLoadEvidence>,
    pub(super) subtree_root_evidence: Option<CanonicalSubtreeRootLoadEvidence>,
    pub(super) confirmed_source_tip_checkpoint: Option<zinder_core::CommitmentTreeCheckpoint>,
    pub(super) cursor_auth_key: [u8; 32],
}

impl RocksDbCanonicalBuilder {
    /// Creates an unpublished canonical store at a path that does not exist.
    ///
    /// Identity, schema version, network, workload, source range, and cursor
    /// authentication material are synced before any data family is created.
    pub fn create_fresh(
        path: impl AsRef<std::path::Path>,
        workload: CanonicalStoreWorkload,
        build_plan: CanonicalStoreBuildPlan,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        let path = path.as_ref();
        let network = build_plan.network();
        validate_resource_budget(resource_budget)?;
        let mut cursor_auth_key = [0; 32];
        getrandom::fill(&mut cursor_auth_key)
            .map_err(|source| CanonicalStoreError::EntropyUnavailable { source })?;
        create_fresh_directory(path)?;
        let store_path = canonical_store_path(path)?;
        initialize_store_identity(path, workload, &build_plan, cursor_auth_key)?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary { path },
            resource_budget,
            canonical_column_family_descriptors,
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "create builder",
            source,
        })?;
        Ok(Self {
            store_path,
            bounded_open,
            resource_budget,
            network,
            workload,
            build_plan,
            canonical_block_evidence: None,
            subtree_root_evidence: None,
            confirmed_source_tip_checkpoint: None,
            cursor_auth_key,
        })
    }

    /// Returns the immutable network persisted by this build.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Returns the immutable consumer workload persisted by this build.
    #[must_use]
    pub const fn workload(&self) -> CanonicalStoreWorkload {
        self.workload
    }

    /// Returns the immutable activation-table identity persisted by this build.
    #[must_use]
    pub const fn network_upgrade_activations_fingerprint(
        &self,
    ) -> NetworkUpgradeActivationsFingerprint {
        self.build_plan.network_upgrade_activations_fingerprint()
    }

    /// Returns the exact predecessor-to-tip source range persisted by this build.
    #[must_use]
    pub const fn build_plan(&self) -> &CanonicalStoreBuildPlan {
        &self.build_plan
    }

    /// Bulk-loads every source-derived version-1 canonical block family.
    ///
    /// All seven families are completely staged before any column family is
    /// ingested. Reverse indexes use fixed-record bounded sort runs and a
    /// capped merge fan-in; raw transaction bytes remain source-position keyed
    /// and never enter the random-key sort.
    /// The returned evidence describes the prepared rows accepted by `RocksDB`;
    /// READY publication additionally requires cache-bypassing family readback.
    pub fn bulk_load_blocks<SourceError>(
        &mut self,
        blocks: impl IntoIterator<Item = Result<CanonicalBuildBlock, SourceError>>,
    ) -> Result<CanonicalBlockLoadEvidence, CanonicalStoreBuildError<SourceError>> {
        self.bulk_load_blocks_with_limits(
            blocks,
            CANONICAL_BLOCK_SST_TARGET_LOGICAL_BYTES,
            REVERSE_INDEX_SORT_MEMORY_BYTES,
        )
    }

    pub(super) fn bulk_load_blocks_with_limits<SourceError>(
        &mut self,
        blocks: impl IntoIterator<Item = Result<CanonicalBuildBlock, SourceError>>,
        sst_target_logical_bytes: u64,
        reverse_index_sort_memory_bytes: usize,
    ) -> Result<CanonicalBlockLoadEvidence, CanonicalStoreBuildError<SourceError>> {
        if self.canonical_block_evidence.is_some()
            || !canonical_block_families_are_empty(&self.bounded_open.db)?
        {
            return Err(CanonicalStoreError::BlockLoadAlreadyLoaded.into());
        }
        let staging_path = canonical_block_staging_path(&self.store_path);
        let staging = FreshCanonicalBlockStaging::create(staging_path)?;
        let sst_options =
            canonical_data_options(&self.bounded_open.block_cache, self.resource_budget);
        let prepared = write_canonical_block_ssts(
            CanonicalBlockSstConfig {
                staging_path: staging.path(),
                options: &sst_options,
                workload: self.workload,
                build_plan: &self.build_plan,
                sst_target_logical_bytes,
                reverse_index_sort_memory_bytes,
            },
            blocks,
        )?;
        validate_block_load_range(&self.build_plan, &prepared.evidence)?;
        let evidence = ingest_canonical_block_ssts(&self.bounded_open.db, prepared)?;
        staging.remove()?;
        self.canonical_block_evidence = Some(evidence);
        Ok(evidence)
    }

    /// Returns the exact non-empty source ranges required after the block load.
    pub fn required_subtree_root_ranges(
        &self,
    ) -> Result<Vec<zinder_core::SubtreeRootRange>, CanonicalStoreError> {
        let block_evidence = self
            .canonical_block_evidence
            .as_ref()
            .ok_or(CanonicalStoreError::CanonicalBlocksNotLoaded)?;
        required_subtree_root_ranges(
            self.build_plan.history_predecessor().tip_metadata(),
            block_evidence.tip_metadata,
        )
    }

    /// Atomically loads every source-authenticated completed subtree root.
    pub fn load_subtree_roots(
        &mut self,
        subtree_roots: impl IntoIterator<Item = CanonicalBuildSubtreeRoot>,
    ) -> Result<CanonicalSubtreeRootLoadEvidence, CanonicalStoreError> {
        if self.subtree_root_evidence.is_some() {
            return Err(CanonicalStoreError::SubtreeRootLoadAlreadyLoaded);
        }
        let block_evidence = self
            .canonical_block_evidence
            .as_ref()
            .ok_or(CanonicalStoreError::CanonicalBlocksNotLoaded)?;
        let evidence = load_subtree_roots(
            &self.bounded_open.db,
            &self.build_plan,
            block_evidence,
            subtree_roots,
        )?;
        self.subtree_root_evidence = Some(evidence);
        Ok(evidence)
    }

    /// Confirms that the persisted exact-tip frontier matches a final source observation.
    pub fn confirm_source_tip_checkpoint(
        &mut self,
        source_checkpoint: &zinder_core::CommitmentTreeCheckpoint,
    ) -> Result<(), CanonicalStoreError> {
        let block_evidence = self
            .canonical_block_evidence
            .as_ref()
            .ok_or(CanonicalStoreError::CanonicalBlocksNotLoaded)?;
        validate_source_tip_checkpoint(
            &self.bounded_open.db,
            &self.build_plan,
            block_evidence,
            source_checkpoint,
        )?;
        self.confirmed_source_tip_checkpoint = Some(source_checkpoint.clone());
        Ok(())
    }

    /// Returns whether the exact fixed tip was authenticated against the source.
    #[must_use]
    pub const fn is_source_tip_checkpoint_confirmed(&self) -> bool {
        self.confirmed_source_tip_checkpoint.is_some()
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

fn validate_block_load_range(
    build_plan: &CanonicalStoreBuildPlan,
    evidence: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    let expected_first_height = build_plan.history_bounds().first_available_height();
    if evidence.first_height != expected_first_height {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "expected first available height {}, observed {}",
            expected_first_height.value(),
            evidence.first_height.value()
        )));
    }
    if evidence.first_parent_hash != build_plan.history_predecessor().block_id.hash {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "block {} parent does not match the persisted history predecessor",
            evidence.first_height.value()
        )));
    }
    let expected_tip = build_plan.build_tip();
    if evidence.tip_height != expected_tip.height || evidence.tip_hash != expected_tip.hash {
        return Err(CanonicalStoreError::block_load_sequence(format!(
            "expected build tip {expected_tip:?}, observed height {} hash {:?}",
            evidence.tip_height.value(),
            evidence.tip_hash
        )));
    }
    Ok(())
}

pub(super) fn canonical_block_staging_path(store_path: &std::path::Path) -> PathBuf {
    let mut staging_path = OsString::from(store_path.as_os_str());
    staging_path.push(".block-load-staging");
    PathBuf::from(staging_path)
}

struct FreshCanonicalBlockStaging {
    path: PathBuf,
    remove_on_drop: bool,
}

impl FreshCanonicalBlockStaging {
    fn create(path: PathBuf) -> Result<Self, CanonicalStoreError> {
        match fs::create_dir(&path) {
            Ok(()) => Ok(Self {
                path,
                remove_on_drop: true,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CanonicalStoreError::BlockLoadStagingExists { path })
            }
            Err(source) => Err(CanonicalStoreError::PathUnavailable { path, source }),
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn remove(mut self) -> Result<(), CanonicalStoreError> {
        fs::remove_dir_all(&self.path).map_err(|source| CanonicalStoreError::PathUnavailable {
            path: self.path.clone(),
            source,
        })?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for FreshCanonicalBlockStaging {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::Path,
        process::{Command, Stdio},
    };

    use prost::Message;
    use rust_rocksdb::DB;
    use tempfile::TempDir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, CanonicalBlockFacts,
        CanonicalBlockFactsDigestVersion, CanonicalBlockReplayEnvelope,
        CanonicalBlockReplayFormatVersion, CanonicalHistoryBounds, CanonicalTransactionFacts,
        ChainEpochId, ChainTipMetadata, CompactBlockArtifact, LockTime, PrivacyShape,
        SerializedBytesDigest, TransactionBlobArtifact, TransactionComponentCounts, TransactionId,
        TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, UnsupportedSection, encode_canonical_block_replay,
        wire::encode_internal_block_hash,
    };

    use super::*;
    use crate::canonical_store::{
        block_load::canonical_block_families_are_empty,
        block_replay::{BLOCK_REPLAY_COLUMN_FAMILY, validate_persisted_block_replays},
        control::{decode_store_control, encode_ready_store_control},
        rocksdb::{
            BLOCK_HASH_INDEX_COLUMN_FAMILY, CANONICAL_DATA_COLUMN_FAMILIES,
            CHAIN_EPOCH_COLUMN_FAMILY, CHAIN_EVENT_COLUMN_FAMILY, STORE_CONTROL_KEY,
        },
    };
    use crate::{CanonicalStoreBuildState, RocksDbCanonicalStore};

    const PUBLICATION_FAILPOINT_ENV: &str = "ZINDER_TEST_CANONICAL_PUBLICATION_FAILPOINT";
    const PUBLICATION_STORE_PATH_ENV: &str = "ZINDER_TEST_CANONICAL_PUBLICATION_STORE_PATH";

    #[test]
    fn builder_refuses_every_existing_path() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let error = RocksDbCanonicalBuilder::create_fresh(
            temporary.path(),
            CanonicalStoreWorkload::Wallet,
            complete_build_plan()?,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("existing path should be rejected")?;
        assert!(matches!(error, CanonicalStoreError::PathNotFresh { .. }));
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_stages_and_ingests_every_wallet_block_family()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let blocks = vec![
            Ok::<_, std::io::Error>(canonical_build_block(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            )),
            Ok(canonical_build_block(BlockHeight::new(2), [2; 32], [1; 32])),
        ];
        let mut blocks = blocks;
        if let Some(Ok(tip)) = blocks.last_mut() {
            add_tree_state_checkpoint(tip)?;
        }

        let evidence = store.bulk_load_blocks(blocks)?;

        assert_eq!(evidence.block_count, 2);
        assert_eq!(evidence.block_header_count, 2);
        assert_eq!(evidence.block_hash_index_count, 2);
        assert_eq!(evidence.block_replay_count, 2);
        assert_eq!(evidence.compact_block_count, 2);
        assert_eq!(evidence.transaction_location_count, 0);
        assert_eq!(evidence.transaction_blob_count, 0);
        assert_eq!(evidence.block_blob_count, 0);
        assert_eq!(evidence.tip_metadata, ChainTipMetadata::new(0, 0, 0));
        assert_eq!(evidence.tree_state_checkpoint_count, 2);
        Ok(())
    }

    #[test]
    fn subtree_root_load_requires_blocks_and_is_atomic_and_single_use()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        assert!(matches!(
            store.required_subtree_root_ranges(),
            Err(CanonicalStoreError::CanonicalBlocksNotLoaded)
        ));

        let first = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        let mut tip = canonical_build_block(BlockHeight::new(2), [2; 32], [1; 32]);
        add_tree_state_checkpoint(&mut tip)?;
        store.bulk_load_blocks([Ok::<_, std::io::Error>(first), Ok(tip)])?;
        assert!(store.required_subtree_root_ranges()?.is_empty());

        let unexpected_root = CanonicalBuildSubtreeRoot {
            protocol: zinder_core::ShieldedProtocol::Sapling,
            subtree_index: zinder_core::SubtreeRootIndex::new(0),
            root_hash: zinder_core::SubtreeRootHash::from_bytes([7; 32]),
            completing_block_height: BlockHeight::new(1),
        };
        assert!(matches!(
            store.load_subtree_roots([unexpected_root]),
            Err(CanonicalStoreError::SubtreeRootSequenceInvalid { .. })
        ));

        let evidence = store.load_subtree_roots(std::iter::empty())?;
        assert_eq!(evidence.subtree_root_count, 0);
        assert_eq!(evidence.subtree_root_logical_bytes, 0);
        let wrong_tip = zinder_core::CommitmentTreeCheckpoint::new(
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([9; 32])),
            2,
            zinder_core::CommitmentTreeFrontiers::default(),
        );
        assert!(matches!(
            store.confirm_source_tip_checkpoint(&wrong_tip),
            Err(CanonicalStoreError::SourceTipCheckpointMismatch { .. })
        ));
        assert!(!store.is_source_tip_checkpoint_confirmed());
        let source_tip = zinder_core::CommitmentTreeCheckpoint::new(
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
            2,
            zinder_core::CommitmentTreeFrontiers::default(),
        );
        store.confirm_source_tip_checkpoint(&source_tip)?;
        assert!(store.is_source_tip_checkpoint_confirmed());
        assert!(matches!(
            store.load_subtree_roots(std::iter::empty()),
            Err(CanonicalStoreError::SubtreeRootLoadAlreadyLoaded)
        ));
        Ok(())
    }

    #[test]
    fn publication_requires_every_authenticated_build_phase()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;

        let error = store
            .validate_for_publication()
            .err()
            .ok_or("an empty BUILDING store must not validate")?;

        assert!(matches!(
            error,
            CanonicalStoreError::PublicationRefused { .. }
        ));
        let error = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("failed validation must leave the store BUILDING")?;
        assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));
        Ok(())
    }

    #[test]
    fn explorer_publication_waits_for_required_daily_family_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store = RocksDbCanonicalBuilder::create_fresh(
            temporary.path().join("explorer"),
            CanonicalStoreWorkload::Explorer,
            complete_build_plan()?,
            RocksDbResourceBudget::for_local_tests(),
        )?;

        let error = store
            .validate_for_publication()
            .err()
            .ok_or("explorer READY must require daily value-pool evidence")?;

        assert!(error.to_string().contains("daily value-pool evidence"));
        Ok(())
    }

    #[test]
    fn cold_validation_publishes_and_reopens_exact_baseline()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = complete_loaded_builder(&store_path)?;

        let validated = store.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let published = validated.publish_baseline(publication)?;

        assert_eq!(published.ready_evidence().visible_epoch.value(), 1);
        assert_eq!(published.ready_evidence().visible_block_count, 2);
        assert_eq!(
            published.ready_evidence().visible_tip.height,
            BlockHeight::new(2)
        );
        drop(published);
        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(reopened.ready_evidence().visible_epoch.value(), 1);
        assert_eq!(
            reopened.ready_evidence().visible_tip.hash,
            BlockHash::from_bytes([2; 32])
        );
        let replayed_blocks = reopened
            .scan_canonical_replay()?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(replayed_blocks.len(), 2);
        assert_eq!(
            replayed_blocks[0].facts().block_header.block_hash,
            BlockHash::from_bytes([1; 32])
        );
        assert_eq!(
            replayed_blocks[1].facts().block_header.block_hash,
            BlockHash::from_bytes([2; 32])
        );
        Ok(())
    }

    #[test]
    fn ready_store_atomically_appends_one_live_canonical_epoch_and_reopens()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let validated = complete_loaded_builder(&store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let mut store = validated.publish_baseline(publication)?;
        let mut live_block = canonical_build_block(BlockHeight::new(3), [3; 32], [2; 32]);
        add_tree_state_checkpoint(&mut live_block)?;
        let expected_fence = store.event_fence();

        let (next_store, outcome) = store.commit_live_append(crate::CanonicalLiveAppend::new(
            expected_fence,
            live_block,
            Vec::new(),
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_001_000),
        ))?;
        store = next_store;

        assert_eq!(outcome.chain_epoch_id(), zinder_core::ChainEpochId::new(2));
        assert_eq!(outcome.chain_event_sequence(), 2);
        assert_eq!(outcome.visible_tip().height, BlockHeight::new(3));
        assert_eq!(outcome.sequence_digest().block_count(), 3);
        assert_ne!(
            outcome.sequence_digest().as_bytes(),
            expected_fence.sequence_digest().as_bytes()
        );
        assert_eq!(store.ready_evidence().visible_tip, outcome.visible_tip());
        drop(store);

        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(
            reopened.ready_evidence().visible_epoch,
            outcome.chain_epoch_id()
        );
        assert_eq!(
            reopened
                .scan_canonical_replay()?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            3
        );
        let expected_fence = reopened.event_fence();
        let mut block_four = canonical_build_block(BlockHeight::new(4), [4; 32], [3; 32]);
        add_tree_state_checkpoint(&mut block_four)?;
        let (store, second_outcome) =
            reopened.commit_live_append(crate::CanonicalLiveAppend::new(
                expected_fence,
                block_four,
                Vec::new(),
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_002_000),
            ))?;
        assert_eq!(second_outcome.chain_epoch_id(), ChainEpochId::new(3));
        assert_eq!(second_outcome.sequence_digest().block_count(), 4);
        drop(store);
        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(
            reopened
                .scan_canonical_replay()?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            4
        );
        Ok(())
    }

    #[test]
    fn ready_store_rejects_incomplete_live_wallet_artifacts_without_advancing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let validated = complete_loaded_builder(&store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let store = validated.publish_baseline(publication)?;
        let expected_fence = store.event_fence();
        let error = store
            .commit_live_append(crate::CanonicalLiveAppend::new(
                expected_fence,
                canonical_build_block(BlockHeight::new(3), [3; 32], [2; 32]),
                Vec::new(),
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_001_000),
            ))
            .err()
            .ok_or("a live tip without its tree state must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::BlockLoadSequenceInvalid { .. }
        ));

        let store = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let mut block_three = canonical_build_block(BlockHeight::new(3), [3; 32], [2; 32]);
        add_tree_state_checkpoint(&mut block_three)?;
        let expected_fence = store.event_fence();
        let unexpected_root = CanonicalBuildSubtreeRoot {
            protocol: zinder_core::ShieldedProtocol::Sapling,
            subtree_index: zinder_core::SubtreeRootIndex::new(0),
            root_hash: zinder_core::SubtreeRootHash::from_bytes([7; 32]),
            completing_block_height: BlockHeight::new(3),
        };
        let error = store
            .commit_live_append(crate::CanonicalLiveAppend::new(
                expected_fence,
                block_three,
                vec![unexpected_root],
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_001_000),
            ))
            .err()
            .ok_or("an unexpected live subtree root must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::LiveCommitRefused { .. }
        ));

        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(
            reopened.ready_evidence().visible_epoch,
            ChainEpochId::new(1)
        );
        assert_eq!(
            reopened
                .scan_canonical_replay()?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn ready_store_rejects_disconnected_live_append_without_advancing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let validated = complete_loaded_builder(&store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let store = validated.publish_baseline(publication)?;
        let expected_fence = store.event_fence();

        let mut disconnected_block = canonical_build_block(BlockHeight::new(3), [3; 32], [9; 32]);
        add_tree_state_checkpoint(&mut disconnected_block)?;
        let error = store
            .commit_live_append(crate::CanonicalLiveAppend::new(
                expected_fence,
                disconnected_block,
                Vec::new(),
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_001_000),
            ))
            .err()
            .ok_or("a disconnected live block must be rejected")?;

        assert!(matches!(
            error,
            CanonicalStoreError::LiveCommitRefused { .. }
        ));
        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(
            reopened.ready_evidence().visible_epoch,
            ChainEpochId::new(1)
        );
        assert_eq!(
            reopened
                .scan_canonical_replay()?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn ready_store_rejects_stale_live_fence_without_advancing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let validated = complete_loaded_builder(&store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let mut store = validated.publish_baseline(publication)?;
        let stale_fence = store.event_fence();
        let mut block_three = canonical_build_block(BlockHeight::new(3), [3; 32], [2; 32]);
        add_tree_state_checkpoint(&mut block_three)?;
        let (next_store, _) = store.commit_live_append(crate::CanonicalLiveAppend::new(
            stale_fence,
            block_three,
            Vec::new(),
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_001_000),
        ))?;
        store = next_store;

        let error = store
            .commit_live_append(crate::CanonicalLiveAppend::new(
                stale_fence,
                canonical_build_block(BlockHeight::new(4), [4; 32], [3; 32]),
                Vec::new(),
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_002_000),
            ))
            .err()
            .ok_or("a stale canonical event fence must be rejected")?;

        assert!(matches!(
            error,
            CanonicalStoreError::LiveCommitRefused { .. }
        ));
        let reopened = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(
            reopened
                .scan_canonical_replay()?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            3
        );
        Ok(())
    }

    #[test]
    fn cold_validation_rejects_replay_corruption_after_bulk_load()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = complete_loaded_builder(&store_path)?;
        let replay_family = store
            .bounded_open
            .db
            .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
            .ok_or("block replay family should exist")?;
        store.bounded_open.db.put_cf(
            &replay_family,
            zinder_core::wire::encode_height_key_ascending(BlockHeight::new(1)),
            [0_u8],
        )?;
        drop(replay_family);

        let error = store
            .validate_for_publication()
            .err()
            .ok_or("cold publication must reject replay corruption")?;

        assert!(matches!(
            error,
            CanonicalStoreError::BlockReplayInvalid { height: 1, .. }
        ));
        let error = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("corrupt BUILDING store must not open READY")?;
        assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));
        Ok(())
    }
    #[test]
    fn publication_crash_child_process() -> Result<(), Box<dyn std::error::Error>> {
        let Some(store_path) = env::var_os(PUBLICATION_STORE_PATH_ENV) else {
            return Ok(());
        };
        let store_path = Path::new(&store_path);
        let validated = complete_loaded_builder(store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let _published = validated.publish_baseline(publication)?;
        Err("publication failpoint did not abort the child process".into())
    }

    #[test]
    fn publication_crashes_preserve_the_atomic_build_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        for failpoint in [
            "after_flush",
            "after_builder_drop",
            "after_cold_validation",
            "before_atomic_write",
        ] {
            let store_path = temporary.path().join(failpoint);
            run_publication_crash_child(&store_path, failpoint)?;
            assert_building_without_baseline(&store_path)?;
        }

        let committed_path = temporary.path().join("after_atomic_write");
        run_publication_crash_child(&committed_path, "after_atomic_write")?;
        let reopened = RocksDbCanonicalStore::open_ready(
            &committed_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(reopened.ready_evidence().visible_epoch.value(), 1);
        assert_eq!(reopened.ready_evidence().visible_event_sequence, 1);
        Ok(())
    }

    #[test]
    fn ready_open_rejects_missing_atomic_baseline_member() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = complete_loaded_builder(&store_path)?;
        let validated = store.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let published = validated.publish_baseline(publication)?;
        drop(published);

        let column_families = DB::list_cf(&rust_rocksdb::Options::default(), &store_path)?;
        assert_eq!(
            column_families.len(),
            CANONICAL_DATA_COLUMN_FAMILIES.len() + 1
        );
        let db = DB::open_cf(
            &rust_rocksdb::Options::default(),
            &store_path,
            &column_families,
        )?;
        let event_family = db
            .cf_handle(CHAIN_EVENT_COLUMN_FAMILY)
            .ok_or("chain event family must exist")?;
        db.delete_cf(&event_family, 1_u64.to_be_bytes())?;
        drop(event_family);
        db.flush_wal(true)?;
        drop(db);

        let error = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("READY without event 1 must fail admission")?;
        assert!(matches!(
            error,
            CanonicalStoreError::PublicationRefused { .. }
        ));
        Ok(())
    }

    #[test]
    fn ready_open_rejects_wrong_first_retained_block_hash() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let validated = complete_loaded_builder(&store_path)?.validate_for_publication()?;
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        drop(validated.publish_baseline(publication)?);

        let column_families = DB::list_cf(&rust_rocksdb::Options::default(), &store_path)?;
        let db = DB::open_cf(
            &rust_rocksdb::Options::default(),
            &store_path,
            &column_families,
        )?;
        let encoded_control = db
            .get(STORE_CONTROL_KEY)?
            .ok_or("READY control must exist")?;
        let decoded_control = decode_store_control(&store_path, &encoded_control)?;
        let CanonicalStoreBuildState::Ready(mut wrong_evidence) = decoded_control.build_state
        else {
            return Err("published control must be READY".into());
        };
        wrong_evidence.first_retained_block =
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([9; 32]));
        db.put(
            STORE_CONTROL_KEY,
            encode_ready_store_control(
                decoded_control.workload,
                &decoded_control.build_plan,
                decoded_control.cursor_auth_key,
                wrong_evidence,
            )?,
        )?;
        db.flush_wal(true)?;
        drop(db);

        let error = RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("wrong first retained block hash must fail admission")?;
        assert!(error.to_string().contains("first retained block"));
        Ok(())
    }

    #[test]
    fn invalid_baseline_input_can_be_corrected_without_rebuilding()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = complete_loaded_builder(&store_path)?;

        let validated = store.validate_for_publication()?;
        let error = validated
            .prepare_baseline(crate::CanonicalBaselinePublication::new(
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([9; 32])),
                zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
            ))
            .err()
            .ok_or("a settled tip with the wrong hash must be rejected")?;

        assert!(matches!(
            error,
            CanonicalStoreError::PublicationRefused { .. }
        ));
        let publication = validated.prepare_baseline(crate::CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            zinder_core::UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        let published = validated.publish_baseline(publication)?;
        drop(published);
        RocksDbCanonicalStore::open_ready(
            &store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_reports_exact_per_family_logical_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        let build_plan = CanonicalStoreBuildPlan::complete(
            &activations,
            0,
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
        )?;
        let mut store = RocksDbCanonicalBuilder::create_fresh(
            &store_path,
            CanonicalStoreWorkload::Explorer,
            build_plan,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let mut block = canonical_build_block_with_raw_blobs(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        add_tree_state_checkpoint(&mut block)?;
        let expected_block_replay_logical_bytes =
            4_u64 + u64::try_from(block.replay_envelope.as_bytes().len())?;
        let expected_compact_block_logical_bytes =
            4_u64 + u64::try_from(block.compact_block.payload_bytes.len())?;

        let evidence = store.bulk_load_blocks(vec![Ok::<_, std::io::Error>(block)])?;

        assert_eq!(evidence.block_header_logical_bytes, 4 + 184);
        assert_eq!(evidence.block_hash_index_logical_bytes, 32 + 4);
        assert_eq!(
            evidence.block_replay_logical_bytes,
            expected_block_replay_logical_bytes
        );
        assert_eq!(
            evidence.compact_block_logical_bytes,
            expected_compact_block_logical_bytes
        );
        assert_eq!(evidence.transaction_location_logical_bytes, 32 + 40);
        assert_eq!(evidence.transaction_blob_logical_bytes, 8 + 5);
        assert_eq!(evidence.block_blob_logical_bytes, 4 + 3);
        assert!(evidence.tree_state_checkpoint_logical_bytes > 0);
        let checked_family_logical_bytes = [
            evidence.block_header_logical_bytes,
            evidence.block_hash_index_logical_bytes,
            evidence.block_replay_logical_bytes,
            evidence.compact_block_logical_bytes,
            evidence.transaction_location_logical_bytes,
            evidence.transaction_blob_logical_bytes,
            evidence.block_blob_logical_bytes,
            evidence.tree_state_checkpoint_logical_bytes,
            evidence.block_final_note_commitment_roots_logical_bytes,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
        .ok_or("family logical byte sum must fit in u64")?;
        assert_eq!(evidence.logical_bytes, checked_family_logical_bytes);
        assert_eq!(evidence.transaction_count, 1);
        assert_eq!(evidence.transaction_location_count, 1);
        assert_eq!(evidence.transaction_blob_count, 1);
        assert_eq!(evidence.block_blob_count, 1);
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_merges_tiny_reverse_index_runs_in_key_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        let build_plan = CanonicalStoreBuildPlan::complete(
            &activations,
            0,
            BlockId::new(BlockHeight::new(3), BlockHash::from_bytes([2; 32])),
        )?;
        let mut store = RocksDbCanonicalBuilder::create_fresh(
            &store_path,
            CanonicalStoreWorkload::Wallet,
            build_plan,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let blocks = vec![
            Ok::<_, std::io::Error>(canonical_build_block(
                BlockHeight::new(1),
                [3; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            )),
            Ok(canonical_build_block(BlockHeight::new(2), [1; 32], [3; 32])),
            Ok(canonical_build_block(BlockHeight::new(3), [2; 32], [1; 32])),
        ];
        let mut blocks = blocks;
        if let Some(Ok(tip)) = blocks.last_mut() {
            add_tree_state_checkpoint(tip)?;
        }

        let evidence = store.bulk_load_blocks_with_limits(blocks, 1, 72)?;

        assert_eq!(evidence.block_hash_index_count, 3);
        assert!(evidence.sst_file_count > 7);
        let block_hash_index = store
            .bounded_open
            .db
            .cf_handle(BLOCK_HASH_INDEX_COLUMN_FAMILY)
            .ok_or("block hash index must exist")?;
        let keys = store
            .bounded_open
            .db
            .iterator_cf(&block_hash_index, rust_rocksdb::IteratorMode::Start)
            .map(|row| row.map(|(key, _)| key[0]))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(keys, vec![1, 2, 3]);
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_source_error_leaves_every_family_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let blocks = vec![
            Ok(canonical_build_block(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            )),
            Err(std::io::Error::other("source stopped")),
        ];

        let error = store
            .bulk_load_blocks(blocks)
            .err()
            .ok_or("source failure must abort every family")?;

        assert!(matches!(error, CanonicalStoreBuildError::Source { .. }));
        assert!(canonical_block_families_are_empty(&store.bounded_open.db)?);
        assert!(!canonical_block_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_rejects_workload_and_compact_payload_mismatches()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let wallet_path = temporary.path().join("wallet");
        let mut wallet = create_building_store(&wallet_path, CanonicalHistoryBounds::complete())?;
        let mut wallet_block = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        wallet_block.block_blob = Some(zinder_core::BlockBlobArtifact::new(
            wallet_block.facts.block_header.height,
            wallet_block.facts.block_header.block_hash,
            wallet_block.facts.block_header.parent_hash,
            [],
        ));
        let error = wallet
            .bulk_load_blocks(vec![Ok::<_, std::io::Error>(wallet_block)])
            .err()
            .ok_or("wallet raw block must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockLoadSequenceInvalid { .. })
        ));
        assert!(canonical_block_families_are_empty(&wallet.bounded_open.db)?);

        let compact_path = temporary.path().join("compact");
        let mut compact_store =
            create_building_store(&compact_path, CanonicalHistoryBounds::complete())?;
        let mut compact_block = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        compact_block.compact_block.payload_bytes = vec![0xff];
        let error = compact_store
            .bulk_load_blocks(vec![Ok::<_, std::io::Error>(compact_block)])
            .err()
            .ok_or("divergent compact payload must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockLoadSequenceInvalid { .. })
        ));
        assert!(canonical_block_families_are_empty(
            &compact_store.bounded_open.db
        )?);
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_rejects_missing_tip_checkpoint_and_wallet_roots()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        let tip = BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32]));

        let missing_checkpoint_path = temporary.path().join("missing-checkpoint");
        let mut missing_checkpoint_store = RocksDbCanonicalBuilder::create_fresh(
            &missing_checkpoint_path,
            CanonicalStoreWorkload::Wallet,
            CanonicalStoreBuildPlan::complete(&activations, 0, tip)?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let missing_checkpoint = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        let error = missing_checkpoint_store
            .bulk_load_blocks(vec![Ok::<_, std::io::Error>(missing_checkpoint)])
            .err()
            .ok_or("tip without a tree-state checkpoint must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockLoadSequenceInvalid { .. })
        ));

        let wallet_roots_path = temporary.path().join("wallet-roots");
        let mut wallet_roots_store = RocksDbCanonicalBuilder::create_fresh(
            &wallet_roots_path,
            CanonicalStoreWorkload::Wallet,
            CanonicalStoreBuildPlan::complete(&activations, 0, tip)?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let mut wallet_roots = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        add_tree_state_checkpoint(&mut wallet_roots)?;
        wallet_roots.block_final_note_commitment_roots =
            Some(zinder_core::BlockFinalNoteCommitmentRoots::unavailable(
                BlockHeight::new(1),
                BlockHash::from_bytes([1; 32]),
            ));
        let error = wallet_roots_store
            .bulk_load_blocks(vec![Ok::<_, std::io::Error>(wallet_roots)])
            .err()
            .ok_or("wallet workload roots must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockLoadSequenceInvalid { .. })
        ));
        Ok(())
    }

    #[test]
    fn persisted_readback_rejects_miskeyed_replay() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let block_replay = store
            .bounded_open
            .db
            .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
            .ok_or("block replay family should exist")?;
        let replay = replay(BlockHeight::new(1), [1; 32], [0; 32]);
        store.bounded_open.db.put_cf(
            &block_replay,
            zinder_core::wire::encode_height_key_ascending(BlockHeight::new(2)),
            replay.as_bytes(),
        )?;

        let error = validate_persisted_block_replays(&store.bounded_open.db)
            .err()
            .ok_or("a miskeyed replay must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::BlockReplayInvalid { height: 2, .. }
        ));
        Ok(())
    }

    #[test]
    fn persisted_readback_rejects_malformed_height_key() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let block_replay = store
            .bounded_open
            .db
            .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
            .ok_or("block replay family should exist")?;
        let replay = replay(BlockHeight::new(1), [1; 32], [0; 32]);
        store
            .bounded_open
            .db
            .put_cf(&block_replay, b"bad", replay.as_bytes())?;

        let error = validate_persisted_block_replays(&store.bounded_open.db)
            .err()
            .ok_or("a malformed replay key must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::BlockReplayKeyInvalid { .. }
        ));
        Ok(())
    }

    #[test]
    fn persisted_readback_rejects_non_version_one_replay() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let block_replay = store
            .bounded_open
            .db
            .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
            .ok_or("block replay family should exist")?;
        let replay = replay(BlockHeight::new(1), [1; 32], [0; 32]);
        let mut encoded = replay.into_bytes();
        if !encoded.starts_with(&[0x08, 0x01]) {
            return Err("the version-1 replay fixture has an unexpected envelope prefix".into());
        }
        encoded[1] = 0x02;
        store.bounded_open.db.put_cf(
            &block_replay,
            zinder_core::wire::encode_height_key_ascending(BlockHeight::new(1)),
            encoded,
        )?;

        let error = validate_persisted_block_replays(&store.bounded_open.db)
            .err()
            .ok_or("a non-version-one replay must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::BlockReplayInvalid { height: 1, .. }
        ));
        Ok(())
    }

    fn create_building_store(
        path: &Path,
        history_bounds: CanonicalHistoryBounds,
    ) -> Result<RocksDbCanonicalBuilder, Box<dyn std::error::Error>> {
        let build_plan = match history_bounds.preceding_checkpoint() {
            None => complete_build_plan()?,
            Some(checkpoint) => {
                let activations = crate::canonical_store::test_network_upgrade_activations(
                    Network::ZcashTestnet,
                )?;
                CanonicalStoreBuildPlan::checkpointed(
                    &activations,
                    zinder_core::CommitmentTreeCheckpoint::new(
                        checkpoint,
                        0,
                        crate::canonical_store::test_checkpoint_frontiers(
                            &activations,
                            checkpoint.height,
                        ),
                    ),
                    BlockId::new(BlockHeight::new(100), BlockHash::from_bytes([10; 32])),
                )?
            }
        };
        Ok(RocksDbCanonicalBuilder::create_fresh(
            path,
            CanonicalStoreWorkload::Wallet,
            build_plan,
            RocksDbResourceBudget::for_local_tests(),
        )?)
    }

    fn complete_loaded_builder(
        path: &Path,
    ) -> Result<RocksDbCanonicalBuilder, Box<dyn std::error::Error>> {
        let mut store = create_building_store(path, CanonicalHistoryBounds::complete())?;
        let first = canonical_build_block(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
        let mut tip = canonical_build_block(BlockHeight::new(2), [2; 32], [1; 32]);
        add_tree_state_checkpoint(&mut tip)?;
        store.bulk_load_blocks([Ok::<_, std::io::Error>(first), Ok(tip)])?;
        store.load_subtree_roots(std::iter::empty())?;
        store.confirm_source_tip_checkpoint(&zinder_core::CommitmentTreeCheckpoint::new(
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
            2,
            zinder_core::CommitmentTreeFrontiers::default(),
        ))?;
        Ok(store)
    }

    fn run_publication_crash_child(
        store_path: &Path,
        failpoint: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let status = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("canonical_store::builder::tests::publication_crash_child_process")
            .arg("--nocapture")
            .env(PUBLICATION_STORE_PATH_ENV, store_path)
            .env(PUBLICATION_FAILPOINT_ENV, failpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            return Err(format!("publication failpoint {failpoint} did not crash").into());
        }
        Ok(())
    }

    fn assert_building_without_baseline(
        store_path: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let error = RocksDbCanonicalStore::open_ready(
            store_path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("pre-publication crash must not open READY")?;
        if !matches!(error, CanonicalStoreError::StoreNotReady { .. }) {
            return Err(error.into());
        }
        let column_families = DB::list_cf(&rust_rocksdb::Options::default(), store_path)?;
        let db = DB::open_cf_for_read_only(
            &rust_rocksdb::Options::default(),
            store_path,
            &column_families,
            false,
        )?;
        for name in [CHAIN_EPOCH_COLUMN_FAMILY, CHAIN_EVENT_COLUMN_FAMILY] {
            let family = db.cf_handle(name).ok_or("baseline family must exist")?;
            if db
                .iterator_cf(&family, rust_rocksdb::IteratorMode::Start)
                .next()
                .is_some()
            {
                return Err(format!("{name} must remain empty before atomic publication").into());
            }
        }
        Ok(())
    }

    fn complete_build_plan() -> Result<CanonicalStoreBuildPlan, Box<dyn std::error::Error>> {
        let activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        Ok(CanonicalStoreBuildPlan::complete(
            &activations,
            0,
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
        )?)
    }

    fn replay(
        height: BlockHeight,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
    ) -> CanonicalBlockReplayEnvelope {
        let facts = CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                height,
                BlockHash::from_bytes(block_hash),
                BlockHash::from_bytes(parent_hash),
                [3; 32],
                [4; 32],
                i64::from(height.value()),
                0x1d00_ffff,
                [5; 32],
                4,
                128,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[]),
            transactions: Vec::new(),
        };
        encode_canonical_block_replay(
            &facts,
            CanonicalBlockReplayFormatVersion::V1,
            CanonicalBlockFactsDigestVersion::V1,
        )
    }

    fn canonical_build_block(
        height: BlockHeight,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
    ) -> crate::CanonicalBuildBlock {
        let facts = CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                height,
                BlockHash::from_bytes(block_hash),
                BlockHash::from_bytes(parent_hash),
                [3; 32],
                [4; 32],
                i64::from(height.value()),
                0x1d00_ffff,
                [5; 32],
                4,
                128,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[]),
            transactions: Vec::new(),
        };
        let replay_envelope = encode_canonical_block_replay(
            &facts,
            CanonicalBlockReplayFormatVersion::V1,
            CanonicalBlockFactsDigestVersion::V1,
        );
        let compact_payload = zinder_proto::compat::lightwalletd::CompactBlock {
            height: u64::from(height.value()),
            hash: encode_internal_block_hash(facts.block_header.block_hash).to_vec(),
            prev_hash: encode_internal_block_hash(facts.block_header.parent_hash).to_vec(),
            chain_metadata: Some(zinder_proto::compat::lightwalletd::ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            }),
            ..Default::default()
        }
        .encode_to_vec();
        crate::CanonicalBuildBlock {
            compact_block: CompactBlockArtifact::new(
                height,
                facts.block_header.block_hash,
                compact_payload,
            ),
            replay_envelope,
            tip_metadata: ChainTipMetadata::new(0, 0, 0),
            tree_state_checkpoint: None,
            block_final_note_commitment_roots: None,
            transaction_blobs: Vec::new(),
            block_blob: None,
            facts,
        }
    }

    fn canonical_build_block_with_raw_blobs(
        height: BlockHeight,
        block_hash: [u8; 32],
        parent_hash: [u8; 32],
    ) -> crate::CanonicalBuildBlock {
        let mut block = canonical_build_block(height, block_hash, parent_hash);
        let raw_transaction_bytes = vec![1, 2, 3, 4, 5];
        let raw_block_bytes = vec![6, 7, 8];
        let transaction_id = TransactionId::from_bytes([9; 32]);
        block.facts.transactions.push(CanonicalTransactionFacts {
            public_facts: TransactionPublicFacts {
                transaction_id,
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::Unsupported {
                    effective_version: 0,
                    version_group_id: None,
                },
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: u32::try_from(raw_transaction_bytes.len()).unwrap_or(u32::MAX),
                counts: TransactionComponentCounts::EMPTY,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                privacy_shape: PrivacyShape::Unclassified,
                is_coinbase: false,
                unsupported_sections: vec![UnsupportedSection::FutureVersionHeader],
            },
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
                &raw_transaction_bytes,
            ),
            intrinsic_value_balances: TransactionIntrinsicValueBalances::default(),
            transparent_inputs: Vec::new(),
            transparent_outputs: Vec::new(),
        });
        block.facts.serialized_bytes_digest =
            SerializedBytesDigest::from_serialized_bytes(&raw_block_bytes);
        block.transaction_blobs.push(TransactionBlobArtifact::new(
            TransactionLocation::new(
                transaction_id,
                height,
                block.facts.block_header.block_hash,
                0,
            ),
            raw_transaction_bytes,
        ));
        block.block_blob = Some(zinder_core::BlockBlobArtifact::new(
            height,
            block.facts.block_header.block_hash,
            block.facts.block_header.parent_hash,
            raw_block_bytes,
        ));
        block.replay_envelope = encode_canonical_block_replay(
            &block.facts,
            CanonicalBlockReplayFormatVersion::V1,
            CanonicalBlockFactsDigestVersion::V1,
        );
        block
    }

    fn add_tree_state_checkpoint(
        block: &mut crate::CanonicalBuildBlock,
    ) -> Result<(), std::num::TryFromIntError> {
        let header = &block.facts.block_header;
        block.tree_state_checkpoint = Some(zinder_core::CommitmentTreeCheckpoint::new(
            BlockId::new(header.height, header.block_hash),
            u32::try_from(header.block_time)?,
            zinder_core::CommitmentTreeFrontiers::default(),
        ));
        Ok(())
    }
}
