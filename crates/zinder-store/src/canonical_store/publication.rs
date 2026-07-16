use std::sync::Arc;

use rust_rocksdb::{
    BoundColumnFamily, DB, DEFAULT_COLUMN_FAMILY_NAME, FlushOptions, ReadOptions, WriteBatch,
    WriteOptions,
};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, ChainEpochId, ChainTipMetadata,
    ShieldedProtocol, UnixTimestampMillis,
};

use crate::{BoundedRocksDbOpen, RocksDbOpenRole, open_bounded_rocksdb};

use super::{
    CanonicalStoreBuildState, CanonicalStoreError, CanonicalStoreReadyEvidence,
    RocksDbCanonicalBuilder, RocksDbCanonicalStore,
    block_load::{
        CanonicalBlockLoadEvidence, read_persisted_tip_metadata,
        validate_persisted_commitment_tree_families, validate_source_tip_checkpoint,
    },
    block_replay::validate_persisted_block_replays,
    control::{DecodedStoreControl, decode_store_control, encode_ready_store_control},
    rocksdb::{
        BLOCK_BLOB_COLUMN_FAMILY, BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
        BLOCK_HASH_INDEX_COLUMN_FAMILY, BLOCK_HEADER_COLUMN_FAMILY, CANONICAL_DATA_COLUMN_FAMILIES,
        CHAIN_EPOCH_COLUMN_FAMILY, CHAIN_EVENT_COLUMN_FAMILY, COMPACT_BLOCK_COLUMN_FAMILY,
        DAILY_VALUE_POOL_BALANCE_COLUMN_FAMILY, DISPLACED_BLOCK_FACTS_COLUMN_FAMILY,
        MEMPOOL_EVENT_COLUMN_FAMILY, STORE_CONTROL_KEY, TRANSACTION_BLOB_COLUMN_FAMILY,
        TRANSACTION_LOCATION_COLUMN_FAMILY, TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
        admit_existing_store, canonical_column_family_descriptors, validate_open_store_control,
    },
    subtree_load::{retained_block_hash, validate_persisted_subtree_root_family},
};

const BASELINE_EPOCH_ID: ChainEpochId = ChainEpochId::new(1);
const BASELINE_EVENT_SEQUENCE: u64 = 1;
const VERSION_ONE: u8 = 1;
const COMMITTED_EVENT: u8 = 1;
const REORG_EVENT: u8 = 2;
const REVERTED_RANGE_ABSENT: u8 = 0;
const REVERTED_RANGE_PRESENT: u8 = 1;
const EPOCH_VALUE_LENGTH: usize = 1 + 4 + 32 + 4 + 32 + 12 + 8;
const EVENT_VALUE_LENGTH: usize = 1 + 1 + 8 + 8 + 1 + 4 + 4 + 4 + 4;

/// Explicit finality and observation time for the first visible canonical epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalBaselinePublication {
    /// Highest block whose canonical identity is safe under the caller's reorg policy.
    pub settled_tip: BlockId,
    /// Wall-clock time at which epoch 1 became visible.
    pub created_at: UnixTimestampMillis,
}

impl CanonicalBaselinePublication {
    /// Creates the explicit baseline publication input.
    #[must_use]
    pub const fn new(settled_tip: BlockId, created_at: UnixTimestampMillis) -> Self {
        Self {
            settled_tip,
            created_at,
        }
    }
}

/// Exclusive owner of a cold-reopened and fully validated canonical v1 build.
///
/// This type can only be produced by [`RocksDbCanonicalBuilder::validate_for_publication`].
pub struct ValidatedRocksDbCanonicalBuild {
    bounded_open: BoundedRocksDbOpen,
    workload: super::CanonicalStoreWorkload,
    build_plan: super::CanonicalStoreBuildPlan,
    cursor_auth_key: [u8; 32],
    ready_evidence: CanonicalStoreReadyEvidence,
    tip_metadata: ChainTipMetadata,
}

/// Baseline input checked against one specific cold-validated canonical build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparedCanonicalBaselinePublication {
    publication: CanonicalBaselinePublication,
    build_tip: BlockId,
    source_sequence_digest: [u8; 32],
}

struct EncodedBaselinePublication {
    epoch: [u8; EPOCH_VALUE_LENGTH],
    event: [u8; EVENT_VALUE_LENGTH],
    control: Vec<u8>,
}

struct PublicationContext {
    store_path: std::path::PathBuf,
    resource_budget: crate::RocksDbResourceBudget,
    network: zinder_core::Network,
    workload: super::CanonicalStoreWorkload,
    build_plan: super::CanonicalStoreBuildPlan,
    cursor_auth_key: [u8; 32],
    block_evidence: CanonicalBlockLoadEvidence,
    subtree_root_evidence: super::CanonicalSubtreeRootLoadEvidence,
    source_tip_checkpoint: zinder_core::CommitmentTreeCheckpoint,
}

impl RocksDbCanonicalBuilder {
    /// Flushes, closes, cold-reopens, and independently validates a complete v1 build.
    pub fn validate_for_publication(
        self,
    ) -> Result<ValidatedRocksDbCanonicalBuild, CanonicalStoreError> {
        let context = PublicationContext::from_builder(&self)?;
        flush_complete_build(&self.bounded_open.db)?;
        let built_database_identity = self.bounded_open.db.get_db_identity().map_err(|source| {
            CanonicalStoreError::RocksDbOperation {
                operation: "publication database identity read",
                source,
            }
        })?;
        abort_at_publication_failpoint("after_flush");
        drop(self);
        abort_at_publication_failpoint("after_builder_drop");
        context.validate_cold_reopen(&built_database_identity)
    }
}

impl PublicationContext {
    fn from_builder(builder: &RocksDbCanonicalBuilder) -> Result<Self, CanonicalStoreError> {
        if builder.workload == super::CanonicalStoreWorkload::Explorer {
            return Err(CanonicalStoreError::publication(
                "explorer publication requires authenticated daily value-pool evidence",
            ));
        }
        let block_evidence = builder.canonical_block_evidence.ok_or_else(|| {
            CanonicalStoreError::publication("canonical block families were not loaded")
        })?;
        let subtree_root_evidence = builder.subtree_root_evidence.ok_or_else(|| {
            CanonicalStoreError::publication("canonical subtree-root ranges were not loaded")
        })?;
        let source_tip_checkpoint =
            builder
                .confirmed_source_tip_checkpoint
                .clone()
                .ok_or_else(|| {
                    CanonicalStoreError::publication(
                        "the fixed canonical tip was not confirmed by the node source",
                    )
                })?;
        Ok(Self {
            store_path: builder.store_path.clone(),
            resource_budget: builder.resource_budget,
            network: builder.network,
            workload: builder.workload,
            build_plan: builder.build_plan.clone(),
            cursor_auth_key: builder.cursor_auth_key,
            block_evidence,
            subtree_root_evidence,
            source_tip_checkpoint,
        })
    }

    fn validate_cold_reopen(
        self,
        built_database_identity: &[u8],
    ) -> Result<ValidatedRocksDbCanonicalBuild, CanonicalStoreError> {
        let expected_control = self.expected_building_control();
        let (admitted_database_identity, admitted_control) = admit_existing_store(
            &self.store_path,
            self.network,
            self.build_plan.network_upgrade_activations_fingerprint(),
            self.workload,
        )?;
        if admitted_database_identity.as_slice() != built_database_identity
            || admitted_control != expected_control
        {
            return Err(CanonicalStoreError::publication(
                "database identity or BUILDING control changed before cold validation",
            ));
        }
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::ExistingPrimary {
                path: &self.store_path,
            },
            self.resource_budget,
            canonical_column_family_descriptors,
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "cold publication reopen",
            source,
        })?;
        validate_reopened_identity_and_control(
            &bounded_open.db,
            &self.store_path,
            built_database_identity,
            &expected_control,
        )?;
        validate_cold_block_families(
            &bounded_open.db,
            self.workload,
            &self.build_plan,
            &self.block_evidence,
        )?;
        let persisted_subtree_evidence = validate_persisted_subtree_root_family(
            &bounded_open.db,
            &self.build_plan,
            &self.block_evidence,
        )?;
        if persisted_subtree_evidence != self.subtree_root_evidence {
            return Err(CanonicalStoreError::publication(
                "cold subtree-root evidence differs from the authenticated source load",
            ));
        }
        validate_source_tip_checkpoint(
            &bounded_open.db,
            &self.build_plan,
            &self.block_evidence,
            &self.source_tip_checkpoint,
        )?;
        let replay_evidence = validate_persisted_block_replays(&bounded_open.db)?;
        let ready_evidence = CanonicalStoreReadyEvidence {
            first_retained_block: BlockId::new(
                replay_evidence.first_height,
                replay_evidence.first_hash,
            ),
            visible_tip: BlockId::new(replay_evidence.tip_height, replay_evidence.tip_hash),
            visible_epoch: BASELINE_EPOCH_ID,
            visible_event_sequence: BASELINE_EVENT_SEQUENCE,
            baseline_block_count: replay_evidence.block_count,
            block_digest_version: replay_evidence.block_digest_version,
            replay_format_version: replay_evidence.replay_format_version,
            sequence_digest_version: replay_evidence.sequence_digest_version,
            baseline_sequence_digest: replay_evidence.sequence_digest.as_bytes(),
            baseline_logical_fact_bytes: replay_evidence.logical_replay_bytes,
        };
        abort_at_publication_failpoint("after_cold_validation");
        Ok(ValidatedRocksDbCanonicalBuild {
            bounded_open,
            workload: self.workload,
            build_plan: self.build_plan,
            cursor_auth_key: self.cursor_auth_key,
            ready_evidence,
            tip_metadata: self.block_evidence.tip_metadata,
        })
    }

    fn expected_building_control(&self) -> DecodedStoreControl {
        DecodedStoreControl {
            network: self.network,
            workload: self.workload,
            build_plan: self.build_plan.clone(),
            cursor_auth_key: self.cursor_auth_key,
            build_state: CanonicalStoreBuildState::Building,
        }
    }
}

impl ValidatedRocksDbCanonicalBuild {
    /// Validates baseline finality and time without consuming the expensive build.
    pub fn prepare_baseline(
        &self,
        publication: CanonicalBaselinePublication,
    ) -> Result<PreparedCanonicalBaselinePublication, CanonicalStoreError> {
        validate_settled_tip(
            &self.bounded_open.db,
            self.ready_evidence,
            publication.settled_tip,
        )?;
        require_empty_family(&self.bounded_open.db, CHAIN_EPOCH_COLUMN_FAMILY)?;
        require_empty_family(&self.bounded_open.db, CHAIN_EVENT_COLUMN_FAMILY)?;
        Ok(PreparedCanonicalBaselinePublication {
            publication,
            build_tip: self.build_plan.build_tip(),
            source_sequence_digest: self.ready_evidence.baseline_sequence_digest,
        })
    }

    /// Atomically publishes epoch 1, event 1, and READY with a synced WAL write.
    pub fn publish_baseline(
        self,
        prepared: PreparedCanonicalBaselinePublication,
    ) -> Result<RocksDbCanonicalStore, CanonicalStoreError> {
        if prepared.build_tip != self.build_plan.build_tip()
            || prepared.source_sequence_digest != self.ready_evidence.baseline_sequence_digest
        {
            return Err(CanonicalStoreError::publication(
                "prepared baseline belongs to a different canonical build",
            ));
        }

        let epoch_key = BASELINE_EPOCH_ID.value().to_be_bytes();
        let event_key = BASELINE_EVENT_SEQUENCE.to_be_bytes();
        let encoded_publication = EncodedBaselinePublication {
            epoch: encode_baseline_epoch(
                self.ready_evidence,
                prepared.publication,
                self.tip_metadata,
            ),
            event: encode_baseline_event(self.ready_evidence),
            control: encode_ready_store_control(
                self.workload,
                &self.build_plan,
                self.cursor_auth_key,
                self.ready_evidence,
            )
            .map_err(|source| CanonicalStoreError::publication(source.to_string()))?,
        };
        let epoch_family = column_family(&self.bounded_open.db, CHAIN_EPOCH_COLUMN_FAMILY)?;
        let event_family = column_family(&self.bounded_open.db, CHAIN_EVENT_COLUMN_FAMILY)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&epoch_family, epoch_key, encoded_publication.epoch);
        batch.put_cf(&event_family, event_key, encoded_publication.event);
        batch.put(STORE_CONTROL_KEY, &encoded_publication.control);
        if batch.len() != 3 {
            return Err(CanonicalStoreError::publication(
                "baseline publication batch must contain exactly three writes",
            ));
        }
        let mut write_options = WriteOptions::default();
        write_options.disable_wal(false);
        write_options.set_sync(true);
        abort_at_publication_failpoint("before_atomic_write");
        self.bounded_open
            .db
            .write_opt(&batch, &write_options)
            .map_err(
                |source| CanonicalStoreError::PublicationWriteOutcomeUnknown {
                    path: self.bounded_open.db.path().to_path_buf(),
                    source,
                },
            )?;
        abort_at_publication_failpoint("after_atomic_write");
        validate_exact_publication_readback(
            &self.bounded_open.db,
            &self.build_plan,
            self.ready_evidence,
            &encoded_publication,
        )
        .map_err(
            |source| CanonicalStoreError::PublicationCommittedButUnverified {
                path: self.bounded_open.db.path().to_path_buf(),
                reason: source.to_string(),
            },
        )?;
        drop(epoch_family);
        drop(event_family);
        Ok(RocksDbCanonicalStore::from_published(
            self.bounded_open,
            self.workload,
            self.build_plan,
            self.cursor_auth_key,
            self.ready_evidence,
        ))
    }
}

pub(super) fn validate_ready_publication(
    db: &DB,
    build_plan: &super::CanonicalStoreBuildPlan,
    ready_evidence: CanonicalStoreReadyEvidence,
) -> Result<(), CanonicalStoreError> {
    validate_first_retained_block(db, ready_evidence.first_retained_block)?;
    let baseline_tip = build_plan.build_tip();
    if ready_evidence.visible_epoch != BASELINE_EPOCH_ID
        || ready_evidence.visible_event_sequence != BASELINE_EVENT_SEQUENCE
        || ready_evidence.visible_tip != baseline_tip
    {
        return Err(CanonicalStoreError::publication(
            "READY live transitions require the canonical live-commit API",
        ));
    }
    let baseline_epoch = decode_chain_epoch(&read_family_row(
        db,
        CHAIN_EPOCH_COLUMN_FAMILY,
        &BASELINE_EPOCH_ID.value().to_be_bytes(),
    )?)?;
    let baseline_event = decode_chain_event(&read_family_row(
        db,
        CHAIN_EVENT_COLUMN_FAMILY,
        &BASELINE_EVENT_SEQUENCE.to_be_bytes(),
    )?)?;
    validate_epoch_record(
        db,
        ready_evidence.first_retained_block.height,
        baseline_tip,
        baseline_epoch,
    )?;
    validate_visible_baseline(build_plan, ready_evidence, baseline_epoch, baseline_event)
}

fn validate_first_retained_block(
    db: &DB,
    first_retained_block: BlockId,
) -> Result<(), CanonicalStoreError> {
    if retained_block_hash(db, first_retained_block.height)? != first_retained_block.hash {
        return Err(CanonicalStoreError::publication(
            "first retained block does not match the current canonical rows",
        ));
    }
    Ok(())
}

fn validate_visible_baseline(
    build_plan: &super::CanonicalStoreBuildPlan,
    ready_evidence: CanonicalStoreReadyEvidence,
    baseline_epoch: DecodedChainEpoch,
    baseline_event: DecodedChainEvent,
) -> Result<(), CanonicalStoreError> {
    let baseline_tip = build_plan.build_tip();
    let expected_range = BlockHeightRange::inclusive(
        ready_evidence.first_retained_block.height,
        baseline_tip.height,
    );
    if ready_evidence.visible_event_sequence != BASELINE_EVENT_SEQUENCE
        || ready_evidence.visible_tip != baseline_tip
        || baseline_epoch.visible_tip != baseline_tip
        || baseline_event.kind != COMMITTED_EVENT
        || baseline_event.resulting_epoch_id != BASELINE_EPOCH_ID
        || baseline_event.previous_epoch_id != 0
        || baseline_event.reverted_range.is_some()
        || baseline_event.committed_range != expected_range
    {
        return Err(CanonicalStoreError::publication(
            "visible baseline epoch 1 or event 1 is inconsistent with the fixed build",
        ));
    }
    Ok(())
}

fn validate_epoch_record(
    db: &DB,
    first_height: BlockHeight,
    expected_visible_tip: BlockId,
    epoch: DecodedChainEpoch,
) -> Result<(), CanonicalStoreError> {
    if epoch.visible_tip != expected_visible_tip
        || !epoch_bounds_are_valid(first_height, epoch)
        || retained_block_hash(db, expected_visible_tip.height)? != expected_visible_tip.hash
        || retained_block_hash(db, epoch.settled_tip.height)? != epoch.settled_tip.hash
        || epoch.tip_metadata != read_persisted_tip_metadata(db, expected_visible_tip)?
    {
        return Err(CanonicalStoreError::publication(
            "chain epoch does not match its retained canonical rows",
        ));
    }
    Ok(())
}

fn epoch_bounds_are_valid(first_retained_height: BlockHeight, epoch: DecodedChainEpoch) -> bool {
    epoch.settled_tip.height >= first_retained_height
        && epoch.settled_tip.height <= epoch.visible_tip.height
}

fn flush_complete_build(db: &DB) -> Result<(), CanonicalStoreError> {
    let mut families = Vec::with_capacity(CANONICAL_DATA_COLUMN_FAMILIES.len() + 1);
    for name in std::iter::once(DEFAULT_COLUMN_FAMILY_NAME).chain(CANONICAL_DATA_COLUMN_FAMILIES) {
        families.push(column_family(db, name)?);
    }
    let family_refs = families.iter().collect::<Vec<_>>();
    let mut flush_options = FlushOptions::default();
    flush_options.set_wait(true);
    db.flush_cfs_opt(&family_refs, &flush_options)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "publication column-family flush",
            source,
        })?;
    db.flush_wal(true)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "publication WAL sync",
            source,
        })
}

fn validate_reopened_identity_and_control(
    db: &DB,
    store_path: &std::path::Path,
    expected_database_identity: &[u8],
    expected_control: &DecodedStoreControl,
) -> Result<(), CanonicalStoreError> {
    let database_identity =
        db.get_db_identity()
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "cold publication database identity read",
                source,
            })?;
    let control = validate_open_store_control(
        db,
        store_path,
        expected_control.network,
        expected_control
            .build_plan
            .network_upgrade_activations_fingerprint(),
        expected_control.workload,
    )?;
    if database_identity.as_slice() != expected_database_identity || &control != expected_control {
        return Err(CanonicalStoreError::publication(
            "database identity or BUILDING control changed during cold reopen",
        ));
    }
    Ok(())
}

fn validate_cold_block_families(
    db: &DB,
    workload: super::CanonicalStoreWorkload,
    build_plan: &super::CanonicalStoreBuildPlan,
    expected: &CanonicalBlockLoadEvidence,
) -> Result<(), CanonicalStoreError> {
    let replay = validate_persisted_block_replays(db)?;
    if !replay.has_same_sequence(expected) {
        return Err(CanonicalStoreError::BlockLoadReadbackMismatch);
    }
    validate_persisted_commitment_tree_families(db, workload, build_plan, expected)?;
    let expected_families = [
        (
            BLOCK_HEADER_COLUMN_FAMILY,
            expected.block_header_count,
            expected.block_header_logical_bytes,
        ),
        (
            BLOCK_HASH_INDEX_COLUMN_FAMILY,
            expected.block_hash_index_count,
            expected.block_hash_index_logical_bytes,
        ),
        (
            super::block_replay::BLOCK_REPLAY_COLUMN_FAMILY,
            expected.block_replay_count,
            expected.block_replay_logical_bytes,
        ),
        (
            COMPACT_BLOCK_COLUMN_FAMILY,
            expected.compact_block_count,
            expected.compact_block_logical_bytes,
        ),
        (
            TRANSACTION_LOCATION_COLUMN_FAMILY,
            expected.transaction_location_count,
            expected.transaction_location_logical_bytes,
        ),
        (
            TRANSACTION_BLOB_COLUMN_FAMILY,
            expected.transaction_blob_count,
            expected.transaction_blob_logical_bytes,
        ),
        (
            BLOCK_BLOB_COLUMN_FAMILY,
            expected.block_blob_count,
            expected.block_blob_logical_bytes,
        ),
        (
            TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
            expected.tree_state_checkpoint_count,
            expected.tree_state_checkpoint_logical_bytes,
        ),
        (
            BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
            expected.block_final_note_commitment_roots_count,
            expected.block_final_note_commitment_roots_logical_bytes,
        ),
    ];
    for (name, expected_count, expected_logical_bytes) in expected_families {
        let observed = scan_family(db, name)?;
        if observed != (expected_count, expected_logical_bytes) {
            return Err(CanonicalStoreError::publication(format!(
                "cold {name} count or logical bytes differ from the source load"
            )));
        }
    }
    require_empty_family(db, DAILY_VALUE_POOL_BALANCE_COLUMN_FAMILY)?;
    for name in [
        CHAIN_EPOCH_COLUMN_FAMILY,
        CHAIN_EVENT_COLUMN_FAMILY,
        MEMPOOL_EVENT_COLUMN_FAMILY,
        DISPLACED_BLOCK_FACTS_COLUMN_FAMILY,
    ] {
        require_empty_family(db, name)?;
    }
    Ok(())
}

fn validate_settled_tip(
    db: &DB,
    ready_evidence: CanonicalStoreReadyEvidence,
    settled_tip: BlockId,
) -> Result<(), CanonicalStoreError> {
    if settled_tip.height < ready_evidence.first_retained_block.height
        || settled_tip.height > ready_evidence.visible_tip.height
        || retained_block_hash(db, settled_tip.height)? != settled_tip.hash
    {
        return Err(CanonicalStoreError::publication(
            "settled tip is outside retained history or has the wrong canonical hash",
        ));
    }
    Ok(())
}

fn encode_baseline_epoch(
    ready_evidence: CanonicalStoreReadyEvidence,
    publication: CanonicalBaselinePublication,
    tip_metadata: ChainTipMetadata,
) -> [u8; EPOCH_VALUE_LENGTH] {
    let mut encoded = [0; EPOCH_VALUE_LENGTH];
    encoded[0] = VERSION_ONE;
    encoded[1..5].copy_from_slice(&ready_evidence.visible_tip.height.value().to_le_bytes());
    encoded[5..37].copy_from_slice(&ready_evidence.visible_tip.hash.as_bytes());
    encoded[37..41].copy_from_slice(&publication.settled_tip.height.value().to_le_bytes());
    encoded[41..73].copy_from_slice(&publication.settled_tip.hash.as_bytes());
    encoded[73..77].copy_from_slice(
        &tip_metadata
            .commitment_tree_size(ShieldedProtocol::Sapling)
            .to_le_bytes(),
    );
    encoded[77..81].copy_from_slice(
        &tip_metadata
            .commitment_tree_size(ShieldedProtocol::Orchard)
            .to_le_bytes(),
    );
    encoded[81..85].copy_from_slice(
        &tip_metadata
            .commitment_tree_size(ShieldedProtocol::Ironwood)
            .to_le_bytes(),
    );
    encoded[85..].copy_from_slice(&publication.created_at.value().to_le_bytes());
    encoded
}

fn encode_baseline_event(ready_evidence: CanonicalStoreReadyEvidence) -> [u8; EVENT_VALUE_LENGTH] {
    let mut encoded = [0; EVENT_VALUE_LENGTH];
    encoded[0] = VERSION_ONE;
    encoded[1] = COMMITTED_EVENT;
    encoded[2..10].copy_from_slice(&BASELINE_EPOCH_ID.value().to_le_bytes());
    encoded[10..18].copy_from_slice(&0_u64.to_le_bytes());
    encoded[18] = REVERTED_RANGE_ABSENT;
    encoded[19..27].fill(0);
    encode_event_range(
        &mut encoded,
        27,
        BlockHeightRange::inclusive(
            ready_evidence.first_retained_block.height,
            ready_evidence.visible_tip.height,
        ),
    );
    encoded
}

fn encode_event_range(
    encoded: &mut [u8; EVENT_VALUE_LENGTH],
    offset: usize,
    block_range: BlockHeightRange,
) {
    encoded[offset..offset + 4].copy_from_slice(&block_range.start.value().to_le_bytes());
    encoded[offset + 4..offset + 8].copy_from_slice(&block_range.end.value().to_le_bytes());
}

fn validate_exact_publication_readback(
    db: &DB,
    build_plan: &super::CanonicalStoreBuildPlan,
    ready_evidence: CanonicalStoreReadyEvidence,
    expected: &EncodedBaselinePublication,
) -> Result<(), CanonicalStoreError> {
    let epoch_family = column_family(db, CHAIN_EPOCH_COLUMN_FAMILY)?;
    let event_family = column_family(db, CHAIN_EVENT_COLUMN_FAMILY)?;
    let epoch = db
        .get_cf(&epoch_family, BASELINE_EPOCH_ID.value().to_be_bytes())
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "baseline epoch readback",
            source,
        })?;
    let event = db
        .get_cf(&event_family, BASELINE_EVENT_SEQUENCE.to_be_bytes())
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "baseline event readback",
            source,
        })?;
    let control =
        db.get(STORE_CONTROL_KEY)
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "READY control readback",
                source,
            })?;
    if epoch.as_deref() != Some(expected.epoch.as_slice())
        || event.as_deref() != Some(expected.event.as_slice())
        || control.as_deref() != Some(expected.control.as_slice())
    {
        return Err(CanonicalStoreError::publication(
            "atomic baseline publication readback differs from its three writes",
        ));
    }
    let decoded_control = decode_store_control(db.path(), &expected.control)?;
    if decoded_control.build_state != CanonicalStoreBuildState::Ready(ready_evidence) {
        return Err(CanonicalStoreError::publication(
            "READY control does not contain the validated baseline evidence",
        ));
    }
    validate_ready_publication(db, build_plan, ready_evidence)
}

fn decode_chain_epoch(encoded: &[u8]) -> Result<DecodedChainEpoch, CanonicalStoreError> {
    if encoded.len() != EPOCH_VALUE_LENGTH || encoded[0] != VERSION_ONE {
        return Err(CanonicalStoreError::publication(
            "baseline epoch is not the exact version-1 record",
        ));
    }
    Ok(DecodedChainEpoch {
        visible_tip: BlockId::new(
            BlockHeight::new(read_u32(encoded, 1)?),
            BlockHash::from_bytes(read_array(encoded, 5)?),
        ),
        settled_tip: BlockId::new(
            BlockHeight::new(read_u32(encoded, 37)?),
            BlockHash::from_bytes(read_array(encoded, 41)?),
        ),
        tip_metadata: ChainTipMetadata::new(
            read_u32(encoded, 73)?,
            read_u32(encoded, 77)?,
            read_u32(encoded, 81)?,
        ),
    })
}

fn decode_chain_event(encoded: &[u8]) -> Result<DecodedChainEvent, CanonicalStoreError> {
    if encoded.len() != EVENT_VALUE_LENGTH || encoded[0] != VERSION_ONE {
        return Err(CanonicalStoreError::publication(
            "chain event is not the exact version-1 record",
        ));
    }
    let event_kind = encoded[1];
    let resulting_epoch_id = ChainEpochId::new(read_u64(encoded, 2)?);
    let previous_epoch_id = read_u64(encoded, 10)?;
    if resulting_epoch_id.value() == 0 || previous_epoch_id >= resulting_epoch_id.value() {
        return Err(CanonicalStoreError::publication(
            "chain event epoch transition is not monotonic",
        ));
    }
    let reverted_range = decode_optional_reverted_range(encoded)?;
    let committed_range = decode_event_range(encoded, 27)?;
    match event_kind {
        COMMITTED_EVENT if reverted_range.is_none() => {}
        REORG_EVENT if previous_epoch_id > 0 && reverted_range.is_some() => {}
        _ => {
            return Err(CanonicalStoreError::publication(
                "chain event kind and version-1 ranges are inconsistent",
            ));
        }
    }
    Ok(DecodedChainEvent {
        kind: event_kind,
        resulting_epoch_id,
        previous_epoch_id,
        reverted_range,
        committed_range,
    })
}

fn decode_optional_reverted_range(
    encoded: &[u8],
) -> Result<Option<BlockHeightRange>, CanonicalStoreError> {
    match encoded[18] {
        REVERTED_RANGE_ABSENT if encoded[19..27].iter().all(|byte| *byte == 0) => Ok(None),
        REVERTED_RANGE_ABSENT => Err(CanonicalStoreError::publication(
            "absent reverted range contains nonzero heights",
        )),
        REVERTED_RANGE_PRESENT => {
            let block_range = decode_event_range(encoded, 19)?;
            if block_range.start > block_range.end {
                return Err(CanonicalStoreError::publication(
                    "reverted chain event range must not be empty",
                ));
            }
            Ok(Some(block_range))
        }
        presence => Err(CanonicalStoreError::publication(format!(
            "chain event contains unknown reverted-range presence {presence}"
        ))),
    }
}

fn decode_event_range(
    encoded: &[u8],
    offset: usize,
) -> Result<BlockHeightRange, CanonicalStoreError> {
    let start = BlockHeight::new(read_u32(encoded, offset)?);
    let end = BlockHeight::new(read_u32(encoded, offset + 4)?);
    let is_nonempty = start <= end;
    let is_anchored_empty = end.next() == Some(start);
    if !is_nonempty && !is_anchored_empty {
        return Err(CanonicalStoreError::publication(
            "chain event range is neither inclusive nor anchored empty",
        ));
    }
    Ok(BlockHeightRange::inclusive(start, end))
}

fn read_u32(encoded: &[u8], offset: usize) -> Result<u32, CanonicalStoreError> {
    Ok(u32::from_le_bytes(read_array(encoded, offset)?))
}

fn read_u64(encoded: &[u8], offset: usize) -> Result<u64, CanonicalStoreError> {
    Ok(u64::from_le_bytes(read_array(encoded, offset)?))
}

fn read_array<const N: usize>(
    encoded: &[u8],
    offset: usize,
) -> Result<[u8; N], CanonicalStoreError> {
    encoded
        .get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| CanonicalStoreError::publication("version-1 record is truncated"))
}

fn require_empty_family(db: &DB, name: &'static str) -> Result<(), CanonicalStoreError> {
    if scan_family(db, name)? != (0, 0) {
        return Err(CanonicalStoreError::publication(format!(
            "{name} must be empty before baseline publication"
        )));
    }
    Ok(())
}

fn scan_family(db: &DB, name: &'static str) -> Result<(u64, u64), CanonicalStoreError> {
    let family = column_family(db, name)?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    read_options.set_readahead_size(2 * 1024 * 1024);
    let mut iterator = db.raw_iterator_cf_opt(&family, read_options);
    iterator.seek_to_first();
    let mut count = 0_u64;
    let mut logical_bytes = 0_u64;
    while iterator.valid() {
        let Some((key, encoded_row)) = iterator.item() else {
            break;
        };
        count = count.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::publication(format!("{name} row count exceeds u64::MAX"))
        })?;
        let row_bytes = u64::try_from(key.len() + encoded_row.len()).map_err(|_| {
            CanonicalStoreError::publication(format!("{name} row size exceeds u64::MAX"))
        })?;
        logical_bytes = logical_bytes.checked_add(row_bytes).ok_or_else(|| {
            CanonicalStoreError::publication(format!("{name} logical bytes exceed u64::MAX"))
        })?;
        iterator.next();
    }
    iterator
        .status()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "cold column-family readback",
            source,
        })?;
    Ok((count, logical_bytes))
}

fn read_family_row(
    db: &DB,
    name: &'static str,
    key: &[u8],
) -> Result<Vec<u8>, CanonicalStoreError> {
    let family = column_family(db, name)?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    db.get_cf_opt(&family, key, &read_options)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "READY publication point read",
            source,
        })?
        .ok_or_else(|| CanonicalStoreError::publication(format!("{name} row is absent")))
}

fn column_family<'db>(
    db: &'db DB,
    name: &'static str,
) -> Result<Arc<BoundColumnFamily<'db>>, CanonicalStoreError> {
    db.cf_handle(name)
        .ok_or_else(|| CanonicalStoreError::publication(format!("{name} column family is absent")))
}

#[derive(Clone, Copy)]
struct DecodedChainEpoch {
    visible_tip: BlockId,
    settled_tip: BlockId,
    tip_metadata: ChainTipMetadata,
}

#[derive(Clone, Copy)]
struct DecodedChainEvent {
    kind: u8,
    resulting_epoch_id: ChainEpochId,
    previous_epoch_id: u64,
    reverted_range: Option<BlockHeightRange>,
    committed_range: BlockHeightRange,
}

#[cfg(test)]
fn abort_at_publication_failpoint(expected: &str) {
    if std::env::var("ZINDER_TEST_CANONICAL_PUBLICATION_FAILPOINT").as_deref() == Ok(expected) {
        std::process::abort();
    }
}

#[cfg(not(test))]
const fn abort_at_publication_failpoint(_expected: &str) {}

#[cfg(test)]
mod tests {
    use zinder_core::{
        CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigestVersion,
        CanonicalBlockReplayFormatVersion,
    };

    use super::*;

    #[test]
    fn baseline_epoch_and_event_have_exact_version_one_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let evidence = ready_evidence();
        let publication = CanonicalBaselinePublication::new(
            BlockId::new(BlockHeight::new(15), BlockHash::from_bytes([3; 32])),
            UnixTimestampMillis::new(7),
        );
        let epoch = encode_baseline_epoch(evidence, publication, ChainTipMetadata::new(4, 5, 6));
        assert_eq!(epoch.len(), 93);
        assert_eq!(epoch[0], 1);
        assert_eq!(&epoch[1..5], &20_u32.to_le_bytes());
        assert_eq!(&epoch[5..37], &[2; 32]);
        assert_eq!(&epoch[37..41], &15_u32.to_le_bytes());
        assert_eq!(&epoch[41..73], &[3; 32]);
        assert_eq!(&epoch[73..77], &4_u32.to_le_bytes());
        assert_eq!(&epoch[77..81], &5_u32.to_le_bytes());
        assert_eq!(&epoch[81..85], &6_u32.to_le_bytes());
        assert_eq!(&epoch[85..], &7_u64.to_le_bytes());
        let decoded_epoch = decode_chain_epoch(&epoch)?;
        assert_eq!(decoded_epoch.visible_tip.height, BlockHeight::new(20));
        assert_eq!(decoded_epoch.settled_tip, publication.settled_tip);
        assert_eq!(decoded_epoch.tip_metadata, ChainTipMetadata::new(4, 5, 6));

        let event = encode_baseline_event(evidence);
        assert_eq!(event.len(), 35);
        assert_eq!(event[0], 1);
        assert_eq!(event[1], COMMITTED_EVENT);
        assert_eq!(&event[2..10], &1_u64.to_le_bytes());
        assert_eq!(&event[10..27], &[0; 17]);
        assert_eq!(&event[27..31], &10_u32.to_le_bytes());
        assert_eq!(&event[31..], &20_u32.to_le_bytes());
        let decoded_event = decode_chain_event(&event)?;
        assert_eq!(decoded_event.resulting_epoch_id, ChainEpochId::new(1));
        assert_eq!(
            decoded_event.committed_range,
            BlockHeightRange::inclusive(BlockHeight::new(10), BlockHeight::new(20))
        );

        let mut wrong_epoch_version = epoch;
        wrong_epoch_version[0] = 2;
        assert!(decode_chain_epoch(&wrong_epoch_version).is_err());
        let mut wrong_event_version = event;
        wrong_event_version[0] = 2;
        assert!(decode_chain_event(&wrong_event_version).is_err());
        Ok(())
    }

    #[test]
    fn event_version_one_preserves_reorg_and_empty_ranges() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut reorg_event = [0; EVENT_VALUE_LENGTH];
        reorg_event[0] = VERSION_ONE;
        reorg_event[1] = REORG_EVENT;
        reorg_event[2..10].copy_from_slice(&3_u64.to_le_bytes());
        reorg_event[10..18].copy_from_slice(&2_u64.to_le_bytes());
        reorg_event[18] = REVERTED_RANGE_PRESENT;
        let replacement_range =
            BlockHeightRange::inclusive(BlockHeight::new(19), BlockHeight::new(20));
        encode_event_range(&mut reorg_event, 19, replacement_range);
        encode_event_range(&mut reorg_event, 27, replacement_range);
        let decoded_reorg = decode_chain_event(&reorg_event)?;
        assert_eq!(decoded_reorg.resulting_epoch_id, ChainEpochId::new(3));
        assert_eq!(decoded_reorg.previous_epoch_id, 2);
        assert_eq!(decoded_reorg.reverted_range, Some(replacement_range));

        let mut safe_tip_event = [0; EVENT_VALUE_LENGTH];
        safe_tip_event[0] = VERSION_ONE;
        safe_tip_event[1] = COMMITTED_EVENT;
        safe_tip_event[2..10].copy_from_slice(&4_u64.to_le_bytes());
        safe_tip_event[10..18].copy_from_slice(&3_u64.to_le_bytes());
        let empty_range = BlockHeightRange::empty_at(BlockHeight::new(20));
        encode_event_range(&mut safe_tip_event, 27, empty_range);
        assert_eq!(
            decode_chain_event(&safe_tip_event)?.committed_range,
            empty_range
        );

        let mut genesis_event = safe_tip_event;
        let genesis_range = BlockHeightRange::inclusive(BlockHeight::new(0), BlockHeight::new(0));
        encode_event_range(&mut genesis_event, 27, genesis_range);
        assert_eq!(
            decode_chain_event(&genesis_event)?.committed_range,
            genesis_range
        );
        Ok(())
    }

    fn ready_evidence() -> CanonicalStoreReadyEvidence {
        CanonicalStoreReadyEvidence {
            first_retained_block: BlockId::new(
                BlockHeight::new(10),
                BlockHash::from_bytes([1; 32]),
            ),
            visible_tip: BlockId::new(BlockHeight::new(20), BlockHash::from_bytes([2; 32])),
            visible_epoch: ChainEpochId::new(1),
            visible_event_sequence: 1,
            baseline_block_count: 11,
            block_digest_version: CanonicalBlockFactsDigestVersion::V1,
            replay_format_version: CanonicalBlockReplayFormatVersion::V1,
            sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::V1,
            baseline_sequence_digest: [4; 32],
            baseline_logical_fact_bytes: 1,
        }
    }
}
