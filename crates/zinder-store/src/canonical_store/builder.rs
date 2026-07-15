use std::{ffi::OsString, fs, path::PathBuf};

use zinder_core::{CanonicalBlockReplayEnvelope, Network};

use crate::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget, open_bounded_rocksdb,
};

use super::{
    CanonicalStoreBuildError, CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload,
    block_replay::{
        BLOCK_REPLAY_SST_TARGET_LOGICAL_BYTES, CanonicalBlockReplayLoadEvidence,
        PreparedBlockReplayLoad, block_replay_is_empty, ingest_block_replay_ssts,
        validate_persisted_block_replays, write_block_replay_ssts_with_target,
    },
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
    block_replay_loaded: bool,
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
            block_replay_loaded: false,
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

    /// Returns the exact predecessor-to-tip source range persisted by this build.
    #[must_use]
    pub const fn build_plan(&self) -> CanonicalStoreBuildPlan {
        self.build_plan
    }

    /// Bulk-loads and validates the complete canonical replay family.
    ///
    /// The builder validates the first row before creating SSTs, propagates
    /// source errors without treating them as iterator exhaustion, rotates
    /// bounded sorted SST files, ingests them atomically, and performs a
    /// cache-bypassing semantic readback. This does not publish a READY store.
    pub fn bulk_load_block_replay<SourceError>(
        &mut self,
        replay_envelopes: impl IntoIterator<Item = Result<CanonicalBlockReplayEnvelope, SourceError>>,
    ) -> Result<CanonicalBlockReplayLoadEvidence, CanonicalStoreBuildError<SourceError>> {
        self.bulk_load_block_replay_with_sst_target(
            replay_envelopes,
            BLOCK_REPLAY_SST_TARGET_LOGICAL_BYTES,
        )
    }

    pub(super) fn bulk_load_block_replay_with_sst_target<SourceError>(
        &mut self,
        replay_envelopes: impl IntoIterator<Item = Result<CanonicalBlockReplayEnvelope, SourceError>>,
        sst_target_logical_bytes: u64,
    ) -> Result<CanonicalBlockReplayLoadEvidence, CanonicalStoreBuildError<SourceError>> {
        if self.block_replay_loaded || !block_replay_is_empty(&self.bounded_open.db)? {
            return Err(CanonicalStoreError::BlockReplayAlreadyLoaded.into());
        }
        let mut replay_envelopes = replay_envelopes.into_iter();
        let first_replay = match replay_envelopes.next() {
            Some(Ok(replay)) => replay,
            Some(Err(source)) => return Err(CanonicalStoreBuildError::Source { source }),
            None => {
                return Err(CanonicalStoreError::block_replay_sequence(
                    "a canonical replay load must contain at least one row",
                )
                .into());
            }
        };
        validate_first_replay(self.build_plan, &first_replay)?;

        let staging_path = block_replay_staging_path(&self.store_path);
        let staging = FreshBlockReplayStaging::create(staging_path)?;
        let sst_options =
            canonical_data_options(&self.bounded_open.block_cache, self.resource_budget);
        let replay_envelopes = std::iter::once(Ok(first_replay)).chain(replay_envelopes);
        let PreparedBlockReplayLoad {
            external_sst_paths,
            evidence: prepared_evidence,
        } = write_block_replay_ssts_with_target(
            staging.path(),
            &sst_options,
            sst_target_logical_bytes,
            replay_envelopes,
        )?;
        validate_replay_tip(
            self.build_plan,
            prepared_evidence.tip_height,
            prepared_evidence.tip_hash,
        )?;
        ingest_block_replay_ssts(&self.bounded_open.db, external_sst_paths)?;
        let persisted_evidence = validate_persisted_block_replays(&self.bounded_open.db, 0)?;
        if !persisted_evidence.has_same_sequence(prepared_evidence) {
            return Err(CanonicalStoreError::BlockReplayReadbackMismatch.into());
        }
        staging.remove()?;
        self.block_replay_loaded = true;
        Ok(prepared_evidence)
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

fn validate_first_replay(
    build_plan: CanonicalStoreBuildPlan,
    replay: &CanonicalBlockReplayEnvelope,
) -> Result<(), CanonicalStoreError> {
    let first_height = build_plan.history_bounds().first_available_height();
    if replay.block_height() != first_height {
        return Err(CanonicalStoreError::block_replay_sequence(format!(
            "expected first available height {}, observed {}",
            first_height.value(),
            replay.block_height().value()
        )));
    }
    if replay.parent_hash() != build_plan.history_predecessor().hash {
        return Err(CanonicalStoreError::block_replay_sequence(format!(
            "block {} parent does not match the persisted history predecessor",
            replay.block_height().value()
        )));
    }
    Ok(())
}

fn validate_replay_tip(
    build_plan: CanonicalStoreBuildPlan,
    observed_height: zinder_core::BlockHeight,
    observed_hash: zinder_core::BlockHash,
) -> Result<(), CanonicalStoreError> {
    let build_tip = build_plan.build_tip();
    if observed_height != build_tip.height || observed_hash != build_tip.hash {
        return Err(CanonicalStoreError::block_replay_sequence(format!(
            "expected build tip {build_tip:?}, observed height {} hash {observed_hash:?}",
            observed_height.value(),
        )));
    }
    Ok(())
}

pub(super) fn block_replay_staging_path(store_path: &std::path::Path) -> PathBuf {
    let mut staging_path = OsString::from(store_path.as_os_str());
    staging_path.push(".block-replay-staging");
    PathBuf::from(staging_path)
}

struct FreshBlockReplayStaging {
    path: PathBuf,
    remove_on_drop: bool,
}

impl FreshBlockReplayStaging {
    fn create(path: PathBuf) -> Result<Self, CanonicalStoreError> {
        match fs::create_dir(&path) {
            Ok(()) => Ok(Self {
                path,
                remove_on_drop: true,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CanonicalStoreError::BlockReplayStagingExists { path })
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

impl Drop for FreshBlockReplayStaging {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rust_rocksdb::{DB, Options};
    use tempfile::TempDir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, CanonicalBlockFacts,
        CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigestBuilder,
        CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayEnvelope,
        CanonicalBlockReplayFormatVersion, CanonicalHistoryBounds, SerializedBytesDigest,
        encode_canonical_block_replay,
    };

    use super::*;
    use crate::{
        RocksDbCanonicalStore,
        canonical_store::{block_replay::BLOCK_REPLAY_COLUMN_FAMILY, rocksdb::STORE_CONTROL_KEY},
    };

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
    fn bulk_load_block_replay_returns_persisted_evidence() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let replays = contiguous_replays();
        let expected_logical_bytes = u64::try_from(
            replays
                .iter()
                .map(|replay| replay.as_bytes().len())
                .sum::<usize>(),
        )?;
        let mut expected_digest = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::V1,
        );
        for replay in &replays {
            expected_digest.try_append(replay.reference_digest())?;
        }
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        store.bulk_load_block_replay(replay_results(replays))?;
        let evidence = validate_persisted_block_replays(&store.bounded_open.db, 0)?;
        assert_eq!(evidence.first_height, BlockHeight::new(1));
        assert_eq!(
            evidence.first_parent_hash,
            Network::ZcashTestnet.genesis_hash()
        );
        assert_eq!(evidence.first_hash, BlockHash::from_bytes([1; 32]));
        assert_eq!(evidence.tip_height, BlockHeight::new(2));
        assert_eq!(evidence.tip_hash, BlockHash::from_bytes([2; 32]));
        assert_eq!(evidence.block_count, 2);
        assert_eq!(evidence.logical_replay_bytes, expected_logical_bytes);
        assert_eq!(evidence.sst_file_bytes, 0);
        assert_eq!(
            evidence.replay_format_version,
            CanonicalBlockReplayFormatVersion::V1
        );
        assert_eq!(
            evidence.block_digest_version,
            CanonicalBlockFactsDigestVersion::V1
        );
        assert_eq!(
            evidence.sequence_digest_version,
            CanonicalBlockFactsSequenceDigestVersion::V1
        );
        assert_eq!(evidence.sequence_digest, expected_digest.finish());
        assert!(!block_replay_staging_path(&store.store_path).exists());
        let error = store
            .bulk_load_block_replay(replay_results(contiguous_replays()))
            .err()
            .ok_or("a non-empty replay family must not be rebuilt")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockReplayAlreadyLoaded)
        ));
        drop(store);

        let error = RocksDbCanonicalStore::open_ready(
            &store_path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("a BUILDING store must not be servable")?;
        assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_empty_input_without_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let error = store
            .bulk_load_block_replay(
                Vec::<Result<CanonicalBlockReplayEnvelope, std::io::Error>>::new(),
            )
            .err()
            .ok_or("an empty replay load must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockReplaySequenceInvalid { .. })
        ));
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_source_error_without_ingestion()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let source_rows = vec![
            Ok(replay(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            )),
            Err(std::io::Error::other("source stopped")),
            Ok(replay(BlockHeight::new(2), [2; 32], [1; 32])),
        ];
        let error = store
            .bulk_load_block_replay(source_rows)
            .err()
            .ok_or("a source error must abort replay construction")?;
        assert!(matches!(error, CanonicalStoreBuildError::Source { .. }));
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_truncated_source_before_ingestion()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let error = store
            .bulk_load_block_replay(replay_results(vec![replay(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            )]))
            .err()
            .ok_or("a contiguous prefix below the fixed tip must be rejected")?;
        assert!(error.to_string().contains("expected build tip"), "{error}");
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn invalid_first_replay_does_not_consume_the_remaining_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let remaining_source_reads = std::cell::Cell::new(0_u32);
        let first = std::iter::once(Ok::<_, std::io::Error>(replay(
            BlockHeight::new(2),
            [2; 32],
            [1; 32],
        )));
        let remaining = std::iter::from_fn(|| {
            remaining_source_reads.set(remaining_source_reads.get() + 1);
            None::<Result<CanonicalBlockReplayEnvelope, std::io::Error>>
        });

        let error = store
            .bulk_load_block_replay(first.chain(remaining))
            .err()
            .ok_or("an invalid first replay must be rejected")?;
        assert!(
            error
                .to_string()
                .contains("expected first available height")
        );
        assert_eq!(remaining_source_reads.get(), 0);
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_ingests_multiple_bounded_ssts_atomically()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let evidence = store
            .bulk_load_block_replay_with_sst_target(replay_results(contiguous_replays()), 1)?;
        assert_eq!(evidence.sst_file_count, 2);
        assert!(evidence.sst_file_bytes > 0);
        assert_eq!(evidence.block_count, 2);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        assert!(!block_replay_is_empty(&store.bounded_open.db)?);
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_discontinuous_input_before_ingestion()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let replays = vec![
            replay(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            ),
            replay(BlockHeight::new(3), [3; 32], [1; 32]),
        ];
        let error = store
            .bulk_load_block_replay(replay_results(replays))
            .err()
            .ok_or("discontinuous replay should be rejected")?;
        assert!(error.to_string().contains("expected height 2"), "{error}");
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_parent_mismatch_before_ingestion()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let replays = vec![
            replay(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            ),
            replay(BlockHeight::new(2), [2; 32], [9; 32]),
        ];
        let error = store
            .bulk_load_block_replay(replay_results(replays))
            .err()
            .ok_or("a mismatched replay parent must be rejected")?;
        assert!(
            error
                .to_string()
                .contains("parent does not match the preceding block hash"),
            "{error}"
        );
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert!(!block_replay_staging_path(&store.store_path).exists());
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_requires_first_available_height()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let error = store
            .bulk_load_block_replay(replay_results(vec![replay(
                BlockHeight::new(2),
                [2; 32],
                [1; 32],
            )]))
            .err()
            .ok_or("a wrong first height must be rejected")?;
        assert!(
            error
                .to_string()
                .contains("expected first available height 1, observed 2"),
            "{error}"
        );
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_accepts_checkpoint_successor()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let checkpoint = BlockId::new(BlockHeight::new(99), BlockHash::from_bytes([9; 32]));
        let history_bounds = CanonicalHistoryBounds::checkpointed(checkpoint)?;
        let mut store = create_building_store(&store_path, history_bounds)?;
        store.bulk_load_block_replay(replay_results(vec![replay(
            BlockHeight::new(100),
            [10; 32],
            [9; 32],
        )]))?;
        let evidence = validate_persisted_block_replays(&store.bounded_open.db, 0)?;
        assert_eq!(evidence.first_height, BlockHeight::new(100));
        assert_eq!(evidence.first_parent_hash, checkpoint.hash);
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_rejects_wrong_checkpoint_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let checkpoint = BlockId::new(BlockHeight::new(99), BlockHash::from_bytes([9; 32]));
        let history_bounds = CanonicalHistoryBounds::checkpointed(checkpoint)?;
        let mut store = create_building_store(&store_path, history_bounds)?;
        let error = store
            .bulk_load_block_replay(replay_results(vec![replay(
                BlockHeight::new(100),
                [10; 32],
                [8; 32],
            )]))
            .err()
            .ok_or("a wrong checkpoint parent must be rejected")?;
        assert!(
            error
                .to_string()
                .contains("does not match the persisted history predecessor"),
            "{error}"
        );
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        Ok(())
    }

    #[test]
    fn bulk_load_block_replay_preserves_existing_staging_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let store_path = temporary.path().join("canonical");
        let mut store = create_building_store(&store_path, CanonicalHistoryBounds::complete())?;
        let control_before = read_control(&store_path)?;
        let staging_path = block_replay_staging_path(&store.store_path);
        fs::create_dir(&staging_path)?;
        let sentinel_path = staging_path.join("sentinel");
        fs::write(&sentinel_path, b"do not repair")?;

        let error = store
            .bulk_load_block_replay(replay_results(contiguous_replays()))
            .err()
            .ok_or("existing staging must be refused")?;
        assert!(matches!(
            error,
            CanonicalStoreBuildError::Store(CanonicalStoreError::BlockReplayStagingExists { .. })
        ));
        assert_eq!(fs::read(&sentinel_path)?, b"do not repair");
        assert!(block_replay_is_empty(&store.bounded_open.db)?);
        assert_eq!(read_control(&store_path)?, control_before);
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

        let error = validate_persisted_block_replays(&store.bounded_open.db, 0)
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

        let error = validate_persisted_block_replays(&store.bounded_open.db, 0)
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

        let error = validate_persisted_block_replays(&store.bounded_open.db, 0)
            .err()
            .ok_or("a non-version-one replay must be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::BlockReplayInvalid { height: 1, .. }
        ));
        Ok(())
    }

    fn read_control(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let column_families = DB::list_cf(&Options::default(), path)?;
        let db = DB::open_cf_for_read_only(&Options::default(), path, &column_families, false)?;
        db.get(STORE_CONTROL_KEY)?
            .ok_or_else(|| "store control should exist".into())
    }

    fn contiguous_replays() -> Vec<CanonicalBlockReplayEnvelope> {
        vec![
            replay(
                BlockHeight::new(1),
                [1; 32],
                Network::ZcashTestnet.genesis_hash().as_bytes(),
            ),
            replay(BlockHeight::new(2), [2; 32], [1; 32]),
        ]
    }

    fn create_building_store(
        path: &Path,
        history_bounds: CanonicalHistoryBounds,
    ) -> Result<RocksDbCanonicalBuilder, Box<dyn std::error::Error>> {
        let build_plan = match history_bounds.preceding_checkpoint() {
            None => complete_build_plan()?,
            Some(checkpoint) => CanonicalStoreBuildPlan::checkpointed(
                Network::ZcashTestnet,
                checkpoint,
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
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
        )?)
    }

    fn replay_results(
        replays: Vec<CanonicalBlockReplayEnvelope>,
    ) -> Vec<Result<CanonicalBlockReplayEnvelope, std::io::Error>> {
        replays.into_iter().map(Ok).collect()
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
}
