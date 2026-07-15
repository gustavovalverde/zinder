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
        write_canonical_block_ssts,
    },
    block_replay::validate_persisted_block_replays,
    rocksdb::{
        canonical_column_family_descriptors, canonical_data_options, canonical_store_path,
        create_fresh_directory, initialize_store_identity, validate_resource_budget,
    },
};

/// Exclusive owner of one fresh, unpublished canonical version-1 build.
///
/// A builder cannot open an existing path. Any crash residue or partially
/// populated family requires deletion of the whole build before retrying.
pub struct RocksDbCanonicalBuilder {
    pub(super) store_path: PathBuf,
    pub(super) bounded_open: BoundedRocksDbOpen,
    resource_budget: RocksDbResourceBudget,
    network: Network,
    workload: CanonicalStoreWorkload,
    build_plan: CanonicalStoreBuildPlan,
    canonical_blocks_loaded: bool,
    _cursor_auth_key: [u8; 32],
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
        initialize_store_identity(path, workload, build_plan, cursor_auth_key)?;
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
            canonical_blocks_loaded: false,
            _cursor_auth_key: cursor_auth_key,
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
    pub const fn build_plan(&self) -> CanonicalStoreBuildPlan {
        self.build_plan
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
        if self.canonical_blocks_loaded
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
                sst_target_logical_bytes,
                reverse_index_sort_memory_bytes,
            },
            blocks,
        )?;
        validate_block_load_range(self.build_plan, &prepared.evidence)?;
        let evidence = ingest_canonical_block_ssts(&self.bounded_open.db, prepared)?;
        let persisted_replays = validate_persisted_block_replays(&self.bounded_open.db)?;
        if !persisted_replays.has_same_sequence(&evidence) {
            return Err(CanonicalStoreError::BlockLoadReadbackMismatch.into());
        }
        staging.remove()?;
        self.canonical_blocks_loaded = true;
        Ok(evidence)
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

fn validate_block_load_range(
    build_plan: CanonicalStoreBuildPlan,
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
    if evidence.first_parent_hash != build_plan.history_predecessor().hash {
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
    use std::path::Path;

    use prost::Message;
    use tempfile::TempDir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, CanonicalBlockFacts,
        CanonicalBlockFactsDigestVersion, CanonicalBlockReplayEnvelope,
        CanonicalBlockReplayFormatVersion, CanonicalHistoryBounds, CanonicalTransactionFacts,
        ChainTipMetadata, CompactBlockArtifact, LockTime, NetworkUpgradeActivationsFingerprint,
        NetworkUpgradeActivationsFingerprintVersion, PrivacyShape, SerializedBytesDigest,
        TransactionBlobArtifact, TransactionComponentCounts, TransactionId,
        TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, UnsupportedSection, encode_canonical_block_replay,
    };

    use super::*;
    use crate::canonical_store::{
        block_load::canonical_block_families_are_empty, block_replay::BLOCK_REPLAY_COLUMN_FAMILY,
        rocksdb::BLOCK_HASH_INDEX_COLUMN_FAMILY,
    };

    const ACTIVATIONS_FINGERPRINT: NetworkUpgradeActivationsFingerprint =
        NetworkUpgradeActivationsFingerprint::from_bytes(
            NetworkUpgradeActivationsFingerprintVersion::V1,
            [17; 32],
        );

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

        let evidence = store.bulk_load_blocks(blocks)?;

        assert_eq!(evidence.block_count, 2);
        assert_eq!(evidence.block_header_count, 2);
        assert_eq!(evidence.block_hash_index_count, 2);
        assert_eq!(evidence.block_replay_count, 2);
        assert_eq!(evidence.compact_block_count, 2);
        assert_eq!(evidence.transaction_location_count, 0);
        assert_eq!(evidence.transaction_blob_count, 0);
        assert_eq!(evidence.block_blob_count, 0);
        assert_eq!(evidence.tip_metadata, ChainTipMetadata::new(2, 4, 6));
        Ok(())
    }

    #[test]
    fn bulk_load_blocks_reports_exact_per_family_logical_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let build_plan = CanonicalStoreBuildPlan::complete(
            Network::ZcashTestnet,
            ACTIVATIONS_FINGERPRINT,
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
        )?;
        let mut store = RocksDbCanonicalBuilder::create_fresh(
            &store_path,
            CanonicalStoreWorkload::Explorer,
            build_plan,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let block = canonical_build_block_with_raw_blobs(
            BlockHeight::new(1),
            [1; 32],
            Network::ZcashTestnet.genesis_hash().as_bytes(),
        );
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
        let checked_family_logical_bytes = [
            evidence.block_header_logical_bytes,
            evidence.block_hash_index_logical_bytes,
            evidence.block_replay_logical_bytes,
            evidence.compact_block_logical_bytes,
            evidence.transaction_location_logical_bytes,
            evidence.transaction_blob_logical_bytes,
            evidence.block_blob_logical_bytes,
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
        let build_plan = CanonicalStoreBuildPlan::complete(
            Network::ZcashTestnet,
            ACTIVATIONS_FINGERPRINT,
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
            Some(checkpoint) => CanonicalStoreBuildPlan::checkpointed(
                Network::ZcashTestnet,
                ACTIVATIONS_FINGERPRINT,
                checkpoint,
                zinder_core::ChainTipMetadata::new(1, 2, 3),
                BlockId::new(BlockHeight::new(100), BlockHash::from_bytes([10; 32])),
            )?,
        };
        Ok(RocksDbCanonicalBuilder::create_fresh(
            path,
            CanonicalStoreWorkload::Wallet,
            build_plan,
            RocksDbResourceBudget::for_local_tests(),
        )?)
    }

    fn complete_build_plan() -> Result<CanonicalStoreBuildPlan, Box<dyn std::error::Error>> {
        Ok(CanonicalStoreBuildPlan::complete(
            Network::ZcashTestnet,
            ACTIVATIONS_FINGERPRINT,
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
            hash: facts.block_header.block_hash.as_bytes().to_vec(),
            prev_hash: facts.block_header.parent_hash.as_bytes().to_vec(),
            chain_metadata: Some(zinder_proto::compat::lightwalletd::ChainMetadata {
                sapling_commitment_tree_size: height.value(),
                orchard_commitment_tree_size: height.value().saturating_mul(2),
                ironwood_commitment_tree_size: height.value().saturating_mul(3),
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
            tip_metadata: ChainTipMetadata::new(
                height.value(),
                height.value().saturating_mul(2),
                height.value().saturating_mul(3),
            ),
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
}
