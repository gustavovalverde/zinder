//! Consistent canonical-plus-projection checkpoint creation and manifesting.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write as _,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use zinder_core::{
    BlockHeight, BlockId, CanonicalHistoryBounds, ChainEpoch, Network,
    wire::{decode_rpc_block_hash_hex, encode_rpc_block_hash_hex, encode_zinder_native_chain_name},
};
use zinder_derive::{
    DeriveStore, DeriveStoreOptions, ProjectionDefinition, ProjectionPreset,
    ProjectionRecoverySource, ProjectionRole, TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
    bundled_projection_definitions,
};
use zinder_store::{
    ChainEpochReadApi, ChainEvent, ChainEventHistoryRequest, ChainStoreOptions, PrimaryChainStore,
    RocksDbResourceBudget, SecondaryChainStore, StoreError, StreamCursorTokenV1,
};

use crate::{IngestError, config::BackupCommandConfig};

pub(crate) const BACKUP_MANIFEST_FILE_NAME: &str = "zinder-backup-manifest.json";
pub(crate) const RESTORE_ADMISSION_FILE_NAME: &str = "zinder-restore-admission.json";
const BACKUP_MANIFEST_FORMAT_VERSION: u32 = 2;
const BACKUP_MANIFEST_TEMP_FILE_NAME: &str = ".zinder-backup-manifest.json.tmp";

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    format_version: u32,
    network: String,
    projection_preset: String,
    canonical_position: Option<CanonicalBackupPosition>,
    projections: Vec<ProjectionBackupPosition>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CanonicalBackupPosition {
    chain_epoch_id: u64,
    visible_tip_height: u32,
    visible_tip_hash: String,
    artifact_schema_version: u16,
    history_bounds: CanonicalHistoryBackupBounds,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum CanonicalHistoryBackupBounds {
    Complete {
        first_available_height: u32,
    },
    Checkpointed {
        first_available_height: u32,
        checkpoint_height: u32,
        checkpoint_hash: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ProjectionBackupState {
    Exact,
    Behind,
    Omitted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChainProjectionPosition {
    Exact,
    RecoverableBehind(ProjectionReplayStart),
    Unrecoverable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProjectionReplayStart {
    HistoryFloor,
    AfterCursor(Box<ChainEvent>),
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionBackupPosition {
    identity: String,
    schema_version: u16,
    cursor: Option<String>,
    materialized_height: Option<u32>,
    state: ProjectionBackupState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestoreAdmission {
    NotApplicable,
    NewlyAdmitted,
    PreviouslyAdmitted,
}

trait CanonicalBackupRead: ChainEpochReadApi {
    fn current_backup_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError>;
}

impl CanonicalBackupRead for PrimaryChainStore {
    fn current_backup_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError> {
        self.current_chain_epoch()
    }
}

impl CanonicalBackupRead for SecondaryChainStore {
    fn current_backup_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError> {
        self.current_chain_epoch()
    }
}

pub(crate) fn detect_backup_projection_preset(
    canonical_storage_path: &Path,
) -> Result<ProjectionPreset, IngestError> {
    let derive_storage_path = DeriveStore::path_for_canonical(canonical_storage_path);
    DeriveStore::detect_projection_preset_at_path(&derive_storage_path)?.ok_or(
        IngestError::DeriveStoreMissing {
            path: derive_storage_path,
        },
    )
}

pub(crate) fn create_backup_checkpoints(
    backup_config: &BackupCommandConfig,
    canonical_store: &PrimaryChainStore,
    projection_preset: ProjectionPreset,
) -> Result<(), IngestError> {
    let derive_storage_path = DeriveStore::path_for_canonical(&backup_config.storage_path);
    if !derive_storage_path.exists() {
        return Err(IngestError::DeriveStoreMissing {
            path: derive_storage_path,
        });
    }
    let derive_store = DeriveStore::open_with_projection_preset(
        &derive_storage_path,
        projection_preset,
        DeriveStoreOptions {
            rocksdb_resource_budget: backup_config.derive_rocksdb_budget,
            ..DeriveStoreOptions::default()
        },
    )?;
    if backup_config.to_path.exists() {
        return Err(IngestError::BackupCheckpointDestinationExists {
            path: backup_config.to_path.clone(),
        });
    }
    let bundle_staging_path = backup_checkpoint_staging_path(&backup_config.to_path);
    if bundle_staging_path.exists() {
        return Err(IngestError::BackupCheckpointStagingExists {
            path: bundle_staging_path,
        });
    }
    let derive_checkpoint_path = DeriveStore::path_for_canonical(&bundle_staging_path);

    canonical_store.create_checkpoint(&bundle_staging_path)?;
    derive_store.create_checkpoint(&derive_checkpoint_path)?;
    let manifest = build_backup_manifest(
        backup_config,
        canonical_store,
        &derive_store,
        projection_preset,
    )?;
    write_manifest_atomically(&bundle_staging_path, &manifest)?;
    validate_staged_backup_bundle(backup_config, &bundle_staging_path, projection_preset)?;
    fs::rename(&bundle_staging_path, &backup_config.to_path).map_err(|source| {
        IngestError::BackupCheckpointInstall {
            from_path: bundle_staging_path,
            to_path: backup_config.to_path.clone(),
            source,
        }
    })?;
    sync_parent_directory(&backup_config.to_path)
}

fn build_backup_manifest(
    backup_config: &BackupCommandConfig,
    canonical_store: &impl CanonicalBackupRead,
    derive_store: &DeriveStore,
    projection_preset: ProjectionPreset,
) -> Result<BackupManifest, IngestError> {
    let canonical_epoch = canonical_store.current_backup_chain_epoch()?;
    let canonical_history_bounds = canonical_store.canonical_history_bounds()?;
    let canonical_position = match (canonical_epoch, canonical_history_bounds) {
        (None, None) => None,
        (Some(epoch), Some(history_bounds)) => Some(CanonicalBackupPosition {
            chain_epoch_id: epoch.id.value(),
            visible_tip_height: epoch.visible_tip_height.value(),
            visible_tip_hash: encode_rpc_block_hash_hex(epoch.visible_tip_hash),
            artifact_schema_version: epoch.artifact_schema_version.value(),
            history_bounds: canonical_history_backup_bounds(history_bounds),
        }),
        (Some(_), None) => {
            return Err(IngestError::Store(
                StoreError::CanonicalHistoryBoundsMissing,
            ));
        }
        (None, Some(_)) => {
            return Err(backup_validation_error(
                &backup_config.storage_path,
                "canonical history bounds exist without a canonical position",
            ));
        }
    };
    let projections = bundled_projection_definitions()
        .iter()
        .map(|definition| {
            projection_backup_position(
                canonical_store,
                derive_store,
                projection_preset,
                definition,
                &backup_config.storage_path,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BackupManifest {
        format_version: BACKUP_MANIFEST_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(backup_config.network).to_owned(),
        projection_preset: projection_preset.as_str().to_owned(),
        canonical_position,
        projections,
    })
}

fn canonical_history_backup_bounds(bounds: CanonicalHistoryBounds) -> CanonicalHistoryBackupBounds {
    bounds.preceding_checkpoint().map_or_else(
        || CanonicalHistoryBackupBounds::Complete {
            first_available_height: bounds.first_available_height().value(),
        },
        |checkpoint| CanonicalHistoryBackupBounds::Checkpointed {
            first_available_height: bounds.first_available_height().value(),
            checkpoint_height: checkpoint.height.value(),
            checkpoint_hash: encode_rpc_block_hash_hex(checkpoint.hash),
        },
    )
}

fn projection_backup_position(
    canonical_store: &impl CanonicalBackupRead,
    derive_store: &DeriveStore,
    projection_preset: ProjectionPreset,
    definition: &ProjectionDefinition,
    backup_path: &Path,
) -> Result<ProjectionBackupPosition, IngestError> {
    let schema = definition.schema;
    if !definition.included_in(projection_preset) {
        return Ok(ProjectionBackupPosition {
            identity: schema.name.as_str().to_owned(),
            schema_version: schema.schema_version,
            cursor: None,
            materialized_height: None,
            state: ProjectionBackupState::Omitted,
        });
    }

    let has_chain_event_cursor = matches!(
        definition.recovery_source,
        ProjectionRecoverySource::CanonicalChainEvents
            | ProjectionRecoverySource::CanonicalBackfillAndChainEvents
            | ProjectionRecoverySource::CanonicalSnapshotAndChainEvents
    );
    let cursor = if has_chain_event_cursor {
        derive_store.get_chain_event_cursor(schema.name)?
    } else {
        None
    };
    let materialized_height = projection_materialized_height(derive_store, definition)?;
    let state = match definition.recovery_source {
        ProjectionRecoverySource::CanonicalChainEvents => {
            let cursor_state =
                classify_chain_projection_position(canonical_store, cursor.as_deref())?;
            if cursor_state == ChainProjectionPosition::Unrecoverable {
                return Err(unrecoverable_projection_error(
                    backup_path,
                    schema.name.as_str(),
                ));
            }
            if schema.name == TRANSACTION_HISTORY_CONSUMER_NAME {
                classify_transaction_history_position(
                    canonical_store,
                    derive_store,
                    &cursor_state,
                    backup_path,
                )?
            } else if matches!(
                definition.role,
                ProjectionRole::WalletCorrectness | ProjectionRole::WalletServing
            ) {
                classify_wallet_projection_position(
                    canonical_store,
                    derive_store,
                    WalletProjectionPosition {
                        identity: schema.name.as_str(),
                        materialized_height,
                        cursor_state,
                        backup_path,
                    },
                )?
            } else {
                chain_projection_backup_state(&cursor_state, backup_path, schema.name.as_str())?
            }
        }
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents
        | ProjectionRecoverySource::CanonicalSnapshotAndChainEvents
        | ProjectionRecoverySource::CanonicalBackfill
        | ProjectionRecoverySource::MempoolEvents => {
            // A live-tail cursor alone cannot prove historical backfill or
            // snapshot coverage. Mempool and pure-backfill projections also
            // have no authenticated canonical cursor. Until each projection
            // exposes a typed backup-position reader, record these states
            // conservatively instead of overstating restore readiness.
            ProjectionBackupState::Behind
        }
    };
    Ok(ProjectionBackupPosition {
        identity: schema.name.as_str().to_owned(),
        schema_version: schema.schema_version,
        cursor: cursor.map(|bytes| URL_SAFE_NO_PAD.encode(bytes)),
        materialized_height: materialized_height.map(BlockHeight::value),
        state,
    })
}

fn projection_materialized_height(
    derive_store: &DeriveStore,
    definition: &ProjectionDefinition,
) -> Result<Option<BlockHeight>, IngestError> {
    match definition.schema.name {
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME => derive_store
            .last_materialized_height_ascending(
                TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            )
            .map_err(IngestError::from),
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME => derive_store
            .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)
            .map_err(IngestError::from),
        _ => Ok(None),
    }
}

struct WalletProjectionPosition<'path> {
    identity: &'path str,
    materialized_height: Option<BlockHeight>,
    cursor_state: ChainProjectionPosition,
    backup_path: &'path Path,
}

fn classify_wallet_projection_position(
    canonical_store: &impl CanonicalBackupRead,
    derive_store: &DeriveStore,
    position: WalletProjectionPosition<'_>,
) -> Result<ProjectionBackupState, IngestError> {
    let WalletProjectionPosition {
        identity,
        materialized_height,
        cursor_state,
        backup_path,
    } = position;
    let canonical_epoch = canonical_store.current_backup_chain_epoch()?;
    let history_bounds = canonical_store.canonical_history_bounds()?;
    match cursor_state {
        ChainProjectionPosition::Exact => {
            let Some(epoch) = canonical_epoch else {
                return Err(unrecoverable_projection_error(backup_path, identity));
            };
            let retention_coverage_is_exact =
                if identity == TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str() {
                    let Some(bounds) = history_bounds else {
                        return Err(unrecoverable_projection_error(backup_path, identity));
                    };
                    transparent_outpoint_coverage_matches_tip(
                        derive_store,
                        epoch,
                        bounds.first_available_height(),
                    )?
                } else {
                    true
                };
            let exact = materialized_height == Some(epoch.visible_tip_height)
                && retention_coverage_is_exact;
            if exact {
                Ok(ProjectionBackupState::Exact)
            } else {
                Err(unrecoverable_projection_error(backup_path, identity))
            }
        }
        ChainProjectionPosition::RecoverableBehind(replay_start) => {
            if identity == TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str()
                && let ProjectionReplayStart::AfterCursor(first_pending_event) = replay_start
            {
                let Some(first_available_height) =
                    history_bounds.map(CanonicalHistoryBounds::first_available_height)
                else {
                    return Err(unrecoverable_projection_error(backup_path, identity));
                };
                if !transparent_outpoint_coverage_connects_to_event(
                    derive_store,
                    materialized_height,
                    first_available_height,
                    first_pending_event.as_ref(),
                )? {
                    return Err(unrecoverable_projection_error(backup_path, identity));
                }
            }
            Ok(ProjectionBackupState::Behind)
        }
        ChainProjectionPosition::Unrecoverable => {
            Err(unrecoverable_projection_error(backup_path, identity))
        }
    }
}

fn classify_transaction_history_position(
    canonical_store: &impl CanonicalBackupRead,
    derive_store: &DeriveStore,
    cursor_state: &ChainProjectionPosition,
    backup_path: &Path,
) -> Result<ProjectionBackupState, IngestError> {
    if matches!(cursor_state, ChainProjectionPosition::RecoverableBehind(_)) {
        return Ok(ProjectionBackupState::Behind);
    }
    if matches!(cursor_state, ChainProjectionPosition::Unrecoverable) {
        return Err(unrecoverable_projection_error(
            backup_path,
            TRANSACTION_HISTORY_CONSUMER_NAME.as_str(),
        ));
    }
    let Some(chain_epoch) = canonical_store.current_backup_chain_epoch()? else {
        return Err(unrecoverable_projection_error(
            backup_path,
            TRANSACTION_HISTORY_CONSUMER_NAME.as_str(),
        ));
    };
    let Some(projection_state) =
        derive_store.consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?
    else {
        return Err(unrecoverable_projection_error(
            backup_path,
            TRANSACTION_HISTORY_CONSUMER_NAME.as_str(),
        ));
    };
    let history_bounds = canonical_store
        .canonical_history_bounds()?
        .ok_or(StoreError::CanonicalHistoryBoundsMissing)?;
    let complete = projection_state.coverage.is_some_and(|coverage| {
        projection_state.projection_epoch_id == chain_epoch.id
            && projection_state.projection_tip_height == chain_epoch.visible_tip_height
            && projection_state.projection_tip_hash == chain_epoch.visible_tip_hash
            && coverage.complete_from_height <= history_bounds.first_available_height()
            && coverage.complete_through_height == chain_epoch.visible_tip_height
            && coverage.complete_through_hash == chain_epoch.visible_tip_hash
    });
    if complete {
        Ok(ProjectionBackupState::Exact)
    } else {
        Err(unrecoverable_projection_error(
            backup_path,
            TRANSACTION_HISTORY_CONSUMER_NAME.as_str(),
        ))
    }
}

fn classify_chain_projection_position(
    canonical_store: &impl CanonicalBackupRead,
    cursor_bytes: Option<&[u8]>,
) -> Result<ChainProjectionPosition, IngestError> {
    let Some(cursor_bytes) = cursor_bytes else {
        let Some(chain_epoch) = canonical_store.current_backup_chain_epoch()? else {
            return Ok(ChainProjectionPosition::RecoverableBehind(
                ProjectionReplayStart::HistoryFloor,
            ));
        };
        let history_bounds = canonical_store
            .canonical_history_bounds()?
            .ok_or(StoreError::CanonicalHistoryBoundsMissing)?;
        if chain_epoch.visible_tip_height < history_bounds.first_available_height() {
            return Ok(ChainProjectionPosition::RecoverableBehind(
                ProjectionReplayStart::HistoryFloor,
            ));
        }
        let events = canonical_store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;
        let first_pending_event = events
            .iter()
            .find(|envelope| chain_event_committed_start(&envelope.event).is_some())
            .map(|envelope| envelope.event.clone());
        let replays_from_history_floor = first_pending_event.as_ref().is_some_and(|event| {
            chain_event_committed_start(event) == Some(history_bounds.first_available_height())
        });
        return Ok(if replays_from_history_floor {
            ChainProjectionPosition::RecoverableBehind(ProjectionReplayStart::HistoryFloor)
        } else {
            ChainProjectionPosition::Unrecoverable
        });
    };
    let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes.to_vec());
    let request = ChainEventHistoryRequest::new(Some(&cursor), NonZeroU32::MIN);
    match canonical_store.chain_event_history(request) {
        Ok(events) => {
            let Some(first_pending_event) = events.first() else {
                return Ok(ChainProjectionPosition::Exact);
            };
            Ok(ChainProjectionPosition::RecoverableBehind(
                ProjectionReplayStart::AfterCursor(Box::new(first_pending_event.event.clone())),
            ))
        }
        Err(StoreError::ChainEventCursorExpired { .. }) => {
            Ok(ChainProjectionPosition::Unrecoverable)
        }
        Err(error) => Err(IngestError::Store(error)),
    }
}

fn chain_projection_backup_state(
    position: &ChainProjectionPosition,
    backup_path: &Path,
    identity: &str,
) -> Result<ProjectionBackupState, IngestError> {
    match position {
        ChainProjectionPosition::Exact => Ok(ProjectionBackupState::Exact),
        ChainProjectionPosition::RecoverableBehind(_) => Ok(ProjectionBackupState::Behind),
        ChainProjectionPosition::Unrecoverable => {
            Err(unrecoverable_projection_error(backup_path, identity))
        }
    }
}

fn chain_event_committed_start(event: &ChainEvent) -> Option<BlockHeight> {
    let (ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. }) =
        event
    else {
        return None;
    };
    (committed.block_range.start <= committed.block_range.end)
        .then_some(committed.block_range.start)
}

fn transparent_outpoint_coverage_matches_tip(
    derive_store: &DeriveStore,
    canonical_epoch: ChainEpoch,
    first_available_height: BlockHeight,
) -> Result<bool, IngestError> {
    let Some(state) =
        derive_store.consumer_projection_state(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?
    else {
        return Ok(false);
    };
    Ok(state.coverage.is_some_and(|coverage| {
        state.projection_epoch_id == canonical_epoch.id
            && state.projection_tip_height == canonical_epoch.visible_tip_height
            && state.projection_tip_hash == canonical_epoch.visible_tip_hash
            && coverage.complete_from_height <= first_available_height
            && coverage.complete_through_height == canonical_epoch.visible_tip_height
            && coverage.complete_through_hash == canonical_epoch.visible_tip_hash
    }))
}

fn transparent_outpoint_coverage_connects_to_event(
    derive_store: &DeriveStore,
    materialized_height: Option<BlockHeight>,
    first_available_height: BlockHeight,
    first_pending_event: &ChainEvent,
) -> Result<bool, IngestError> {
    let Some(state) =
        derive_store.consumer_projection_state(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?
    else {
        return Ok(false);
    };
    let Some(coverage) = state.coverage else {
        return Ok(false);
    };
    let state_is_contiguous = materialized_height == Some(state.projection_tip_height)
        && coverage.complete_from_height <= first_available_height
        && coverage.complete_through_height == state.projection_tip_height
        && coverage.complete_through_hash == state.projection_tip_hash;
    if !state_is_contiguous {
        return Ok(false);
    }
    let connects = match first_pending_event {
        ChainEvent::ChainCommitted { committed } => {
            committed.block_range.start > committed.block_range.end
                || coverage.complete_through_height.next() == Some(committed.block_range.start)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            coverage.complete_through_height >= reverted.block_range.start
                && committed.block_range.start == reverted.block_range.start
        }
        _ => false,
    };
    Ok(connects)
}

fn unrecoverable_projection_error(path: &Path, identity: &str) -> IngestError {
    backup_validation_error(
        path,
        format!(
            "included projection {identity} is behind canonical history without provable replay coverage"
        ),
    )
}

fn write_manifest_atomically(
    checkpoint_path: &Path,
    manifest: &BackupManifest,
) -> Result<(), IngestError> {
    let temporary_path = checkpoint_path.join(BACKUP_MANIFEST_TEMP_FILE_NAME);
    let final_path = checkpoint_path.join(BACKUP_MANIFEST_FILE_NAME);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|source| manifest_io("create", &temporary_path, source))?;
    serde_json::to_writer_pretty(&mut file, manifest)
        .map_err(|source| IngestError::BackupManifestEncode { source })?;
    file.write_all(b"\n")
        .map_err(|source| manifest_io("write", &temporary_path, source))?;
    file.sync_all()
        .map_err(|source| manifest_io("sync", &temporary_path, source))?;
    fs::rename(&temporary_path, &final_path)
        .map_err(|source| manifest_io("install", &final_path, source))?;
    File::open(checkpoint_path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| manifest_io("sync directory", checkpoint_path, source))?;
    Ok(())
}

fn validate_staged_backup_bundle(
    backup_config: &BackupCommandConfig,
    bundle_staging_path: &Path,
    projection_preset: ProjectionPreset,
) -> Result<(), IngestError> {
    let manifest_path = bundle_staging_path.join(BACKUP_MANIFEST_FILE_NAME);
    let manifest_bytes =
        fs::read(&manifest_path).map_err(|source| manifest_io("read", &manifest_path, source))?;
    let recorded_manifest: BackupManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| {
            IngestError::BackupManifestDecode {
                path: manifest_path,
                source,
            }
        })?;
    validate_manifest_structure(
        &recorded_manifest,
        backup_config.network,
        projection_preset,
        bundle_staging_path,
    )?;

    let scratch = BackupValidationScratch::create(bundle_staging_path)?;
    let canonical_store = SecondaryChainStore::open(
        bundle_staging_path,
        scratch.canonical_path(),
        ChainStoreOptions {
            rocksdb_resource_budget: backup_config.canonical_rocksdb_budget,
            ..ChainStoreOptions::for_network(backup_config.network)
        },
    )?;
    let derive_store = DeriveStore::open_secondary_with_projection_preset(
        DeriveStore::path_for_canonical(bundle_staging_path),
        scratch.derive_path(),
        projection_preset,
        DeriveStoreOptions {
            rocksdb_resource_budget: backup_config.derive_rocksdb_budget,
            ..DeriveStoreOptions::default()
        },
    )?;
    let recomputed_manifest = build_backup_manifest(
        backup_config,
        &canonical_store,
        &derive_store,
        projection_preset,
    )?;
    if recorded_manifest != recomputed_manifest {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest positions do not match the reopened staged checkpoints",
        ));
    }
    drop(derive_store);
    drop(canonical_store);
    scratch.remove()
}

pub(crate) fn admit_restore_bundle_if_present(
    bundle_path: &Path,
    network: Network,
    projection_preset: ProjectionPreset,
    canonical_rocksdb_budget: RocksDbResourceBudget,
    derive_rocksdb_budget: RocksDbResourceBudget,
) -> Result<RestoreAdmission, IngestError> {
    let pending_manifest_path = bundle_path.join(BACKUP_MANIFEST_FILE_NAME);
    let admission_path = bundle_path.join(RESTORE_ADMISSION_FILE_NAME);
    let pending_manifest_exists = pending_manifest_path.exists();
    let admission_exists = admission_path.exists();

    if pending_manifest_exists && admission_exists {
        return Err(backup_validation_error(
            bundle_path,
            "restore bundle contains both a pending manifest and an admission record",
        ));
    }
    if admission_exists {
        let admitted_manifest = read_backup_manifest(&admission_path)?;
        validate_manifest_structure(&admitted_manifest, network, projection_preset, bundle_path)?;
        return Ok(RestoreAdmission::PreviouslyAdmitted);
    }
    if !pending_manifest_exists {
        return Ok(RestoreAdmission::NotApplicable);
    }

    let validation_config = BackupCommandConfig {
        network,
        storage_path: bundle_path.to_path_buf(),
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
        to_path: bundle_path.to_path_buf(),
    };
    validate_staged_backup_bundle(&validation_config, bundle_path, projection_preset)?;
    fs::rename(&pending_manifest_path, &admission_path)
        .map_err(|source| manifest_io("admit restore", &admission_path, source))?;
    sync_directory(bundle_path)?;
    Ok(RestoreAdmission::NewlyAdmitted)
}

fn read_backup_manifest(manifest_path: &Path) -> Result<BackupManifest, IngestError> {
    let manifest_bytes =
        fs::read(manifest_path).map_err(|source| manifest_io("read", manifest_path, source))?;
    serde_json::from_slice(&manifest_bytes).map_err(|source| IngestError::BackupManifestDecode {
        path: manifest_path.to_path_buf(),
        source,
    })
}

fn validate_manifest_structure(
    manifest: &BackupManifest,
    expected_network: Network,
    expected_preset: ProjectionPreset,
    bundle_staging_path: &Path,
) -> Result<(), IngestError> {
    if manifest.format_version != BACKUP_MANIFEST_FORMAT_VERSION {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest format version is unsupported",
        ));
    }
    if manifest.network != encode_zinder_native_chain_name(expected_network) {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest network does not match the requested backup network",
        ));
    }
    if manifest.projection_preset != expected_preset.as_str() {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest projection preset does not match the requested backup preset",
        ));
    }
    if let Some(canonical) = &manifest.canonical_position {
        decode_rpc_block_hash_hex(&canonical.visible_tip_hash).map_err(|_| {
            backup_validation_error(bundle_staging_path, "canonical tip hash is malformed")
        })?;
        validate_canonical_history_backup_bounds(canonical, bundle_staging_path)?;
    }

    let definitions = bundled_projection_definitions();
    if manifest.projections.len() != definitions.len() {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest projection count does not match the bundled catalog",
        ));
    }
    let mut identities = HashSet::with_capacity(manifest.projections.len());
    for projection in &manifest.projections {
        if !identities.insert(projection.identity.as_str()) {
            return Err(backup_validation_error(
                bundle_staging_path,
                "manifest contains a duplicate projection identity",
            ));
        }
        let Some(definition) = definitions
            .iter()
            .find(|definition| definition.schema.name.as_str() == projection.identity)
        else {
            return Err(backup_validation_error(
                bundle_staging_path,
                "manifest contains an unknown projection identity",
            ));
        };
        validate_projection_position(
            manifest.canonical_position.as_ref(),
            projection,
            definition,
            expected_preset,
            bundle_staging_path,
        )?;
    }
    Ok(())
}

fn validate_canonical_history_backup_bounds(
    canonical: &CanonicalBackupPosition,
    bundle_staging_path: &Path,
) -> Result<(), IngestError> {
    let bounds = match &canonical.history_bounds {
        CanonicalHistoryBackupBounds::Complete {
            first_available_height,
        } => {
            if *first_available_height != 1 {
                return Err(backup_validation_error(
                    bundle_staging_path,
                    "complete canonical history must begin at height 1",
                ));
            }
            CanonicalHistoryBounds::complete()
        }
        CanonicalHistoryBackupBounds::Checkpointed {
            first_available_height,
            checkpoint_height,
            checkpoint_hash,
        } => {
            let checkpoint_hash = decode_rpc_block_hash_hex(checkpoint_hash).map_err(|_| {
                backup_validation_error(
                    bundle_staging_path,
                    "canonical checkpoint hash is malformed",
                )
            })?;
            let bounds = CanonicalHistoryBounds::checkpointed(BlockId::new(
                BlockHeight::new(*checkpoint_height),
                checkpoint_hash,
            ))
            .map_err(|_| {
                backup_validation_error(
                    bundle_staging_path,
                    "canonical checkpoint height has no retained successor",
                )
            })?;
            if *first_available_height != bounds.first_available_height().value() {
                return Err(backup_validation_error(
                    bundle_staging_path,
                    "canonical first available height does not follow its checkpoint",
                ));
            }
            bounds
        }
    };
    if canonical.visible_tip_height < bounds.first_available_height().value().saturating_sub(1) {
        return Err(backup_validation_error(
            bundle_staging_path,
            "canonical tip precedes its recorded history boundary",
        ));
    }
    Ok(())
}

fn validate_projection_position(
    canonical: Option<&CanonicalBackupPosition>,
    projection: &ProjectionBackupPosition,
    definition: &ProjectionDefinition,
    projection_preset: ProjectionPreset,
    bundle_staging_path: &Path,
) -> Result<(), IngestError> {
    if projection.schema_version != definition.schema.schema_version {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest projection schema version does not match the bundled catalog",
        ));
    }
    let included = definition.included_in(projection_preset);
    if included == (projection.state == ProjectionBackupState::Omitted) {
        return Err(backup_validation_error(
            bundle_staging_path,
            "manifest projection inclusion does not match its preset",
        ));
    }
    if projection.state == ProjectionBackupState::Omitted
        && (projection.cursor.is_some() || projection.materialized_height.is_some())
    {
        return Err(backup_validation_error(
            bundle_staging_path,
            "omitted projection contains position evidence",
        ));
    }
    if let Some(cursor) = &projection.cursor {
        URL_SAFE_NO_PAD.decode(cursor).map_err(|_| {
            backup_validation_error(bundle_staging_path, "projection cursor is malformed")
        })?;
    }
    if let Some(materialized_height) = projection.materialized_height {
        let Some(canonical) = canonical else {
            return Err(backup_validation_error(
                bundle_staging_path,
                "projection materialized height exists without a canonical position",
            ));
        };
        if materialized_height > canonical.visible_tip_height {
            return Err(backup_validation_error(
                bundle_staging_path,
                "projection materialized height exceeds the canonical tip",
            ));
        }
    }
    if projection.state == ProjectionBackupState::Exact {
        if projection.cursor.is_none() {
            return Err(backup_validation_error(
                bundle_staging_path,
                "exact projection is missing its authenticated cursor",
            ));
        }
        if matches!(
            definition.role,
            ProjectionRole::WalletCorrectness | ProjectionRole::WalletServing
        ) {
            let canonical_tip = canonical.map(|position| position.visible_tip_height);
            if projection.materialized_height != canonical_tip {
                return Err(backup_validation_error(
                    bundle_staging_path,
                    "exact wallet projection is missing materialized canonical-tip evidence",
                ));
            }
        }
    }
    Ok(())
}

fn backup_validation_error(path: &Path, reason: impl Into<String>) -> IngestError {
    IngestError::BackupCheckpointValidation {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

struct BackupValidationScratch {
    path: PathBuf,
    removed: bool,
}

impl BackupValidationScratch {
    fn create(bundle_staging_path: &Path) -> Result<Self, IngestError> {
        let path = backup_validation_scratch_path(bundle_staging_path);
        fs::create_dir(&path)
            .map_err(|source| manifest_io("create validation scratch", &path, source))?;
        Ok(Self {
            path,
            removed: false,
        })
    }

    fn canonical_path(&self) -> PathBuf {
        self.path.join("canonical-secondary")
    }

    fn derive_path(&self) -> PathBuf {
        self.path.join("derive-secondary")
    }

    fn remove(mut self) -> Result<(), IngestError> {
        fs::remove_dir_all(&self.path)
            .map_err(|source| manifest_io("remove validation scratch", &self.path, source))?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for BackupValidationScratch {
    fn drop(&mut self) {
        if !self.removed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), IngestError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), IngestError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| manifest_io("sync directory", path, source))
}

fn manifest_io(operation: &'static str, path: &Path, source: std::io::Error) -> IngestError {
    IngestError::BackupManifestIo {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn backup_checkpoint_staging_path(checkpoint_path: &Path) -> PathBuf {
    let mut extension = checkpoint_path
        .extension()
        .map_or_else(|| OsString::from("staging"), OsString::from);
    if checkpoint_path.extension().is_some() {
        extension.push(".staging");
    }
    let mut staging_path = checkpoint_path.to_path_buf();
    staging_path.set_extension(extension);
    staging_path
}

fn backup_validation_scratch_path(bundle_staging_path: &Path) -> PathBuf {
    let mut extension = bundle_staging_path
        .extension()
        .map_or_else(|| OsString::from("validation"), OsString::from);
    if bundle_staging_path.extension().is_some() {
        extension.push(".validation");
    }
    let mut validation_path = bundle_staging_path.to_path_buf();
    validation_path.set_extension(extension);
    validation_path
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use zinder_core::{BlockHeightRange, ChainEpochId, Network, UnixTimestampMillis};
    use zinder_derive::{
        BLOCK_SUMMARY_CONSUMER_NAME, ConsumerProjectionCoverage, ConsumerProjectionState,
        TRANSACTION_HISTORY_CONSUMER_NAME,
    };
    use zinder_store::{ChainStoreOptions, RocksDbResourceBudget};
    use zinder_testkit::ChainFixture;

    use super::*;

    #[test]
    fn live_tail_cursor_does_not_overstate_backfill_projection_as_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let commit = canonical_store.commit_chain_epoch(artifacts)?;
        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Complete,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        derive_store.put_chain_event_cursor(
            zinder_derive::PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
            commit.event_envelope.cursor.as_bytes(),
        )?;
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: tempdir.path().join("checkpoint"),
        };

        let manifest = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Complete,
        )?;
        let paid_fee = manifest
            .projections
            .iter()
            .find(|projection| {
                projection.identity == zinder_derive::PAID_FEE_DISTRIBUTION_CONSUMER_NAME.as_str()
            })
            .ok_or("paid-fee projection missing")?;
        assert!(paid_fee.cursor.is_some());
        assert_eq!(paid_fee.state, ProjectionBackupState::Behind);
        Ok(())
    }

    #[test]
    fn wallet_backup_records_selected_and_omitted_projection_positions()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let commit = canonical_store.commit_chain_epoch(artifacts)?;
        drop(DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?);
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };

        create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Wallet)?;

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(checkpoint_path.join(BACKUP_MANIFEST_FILE_NAME))?)?;
        assert_eq!(manifest["format_version"], BACKUP_MANIFEST_FORMAT_VERSION);
        assert_eq!(
            manifest["canonical_position"]["visible_tip_hash"],
            encode_rpc_block_hash_hex(commit.chain_epoch.visible_tip_hash)
        );
        assert_eq!(
            manifest["canonical_position"]["artifact_schema_version"],
            commit.chain_epoch.artifact_schema_version.value()
        );
        assert_complete_history_bounds(&manifest)?;
        let projections = manifest["projections"]
            .as_array()
            .ok_or("projection manifest must be an array")?;
        for projection in projections {
            let identity = projection["identity"]
                .as_str()
                .ok_or("projection identity must be a string")?;
            let selected = ProjectionPreset::Wallet
                .consumer_schemas()
                .iter()
                .any(|schema| schema.name.as_str() == identity);
            if selected {
                assert_eq!(projection["state"], "behind", "{identity}");
                assert!(projection["cursor"].is_null(), "{identity}");
                assert!(projection["materialized_height"].is_null(), "{identity}");
            } else {
                assert_eq!(projection["state"], "omitted", "{identity}");
                assert!(projection["cursor"].is_null(), "{identity}");
            }
        }
        DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&checkpoint_path),
            ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        assert!(
            !backup_validation_scratch_path(&backup_checkpoint_staging_path(&checkpoint_path))
                .exists()
        );
        Ok(())
    }

    #[test]
    fn backup_discovers_the_persisted_wallet_workload() -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        drop(DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?);

        assert_eq!(
            detect_backup_projection_preset(&storage_path)?,
            ProjectionPreset::Wallet
        );
        Ok(())
    }

    fn assert_complete_history_bounds(
        manifest: &serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let history = &manifest["canonical_position"]["history_bounds"];
        if history["kind"] != "complete" || history["first_available_height"] != 1 {
            return Err("backup manifest does not record complete canonical history".into());
        }
        Ok(())
    }

    #[test]
    fn checkpointed_history_manifest_records_and_validates_the_durable_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        let checkpoint = BlockId::new(
            BlockHeight::new(500),
            zinder_core::BlockHash::from_bytes([0x42; 32]),
        );
        let history_bounds =
            canonical_history_backup_bounds(CanonicalHistoryBounds::checkpointed(checkpoint)?);
        let canonical = CanonicalBackupPosition {
            chain_epoch_id: 1,
            visible_tip_height: 500,
            visible_tip_hash: encode_rpc_block_hash_hex(checkpoint.hash),
            artifact_schema_version: 12,
            history_bounds,
        };

        validate_canonical_history_backup_bounds(&canonical, Path::new("checkpoint"))?;
        let CanonicalHistoryBackupBounds::Checkpointed {
            first_available_height,
            checkpoint_height,
            checkpoint_hash,
        } = canonical.history_bounds
        else {
            return Err("expected checkpointed canonical history".into());
        };
        assert_eq!(first_available_height, 501);
        assert_eq!(checkpoint_height, 500);
        assert_eq!(checkpoint_hash, encode_rpc_block_hash_hex(checkpoint.hash));
        Ok(())
    }

    #[test]
    fn wallet_projection_exact_state_requires_a_materialized_canonical_tip()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let commit = canonical_store.commit_chain_epoch(artifacts)?;
        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        for schema in ProjectionPreset::Wallet.consumer_schemas() {
            derive_store
                .put_chain_event_cursor(schema.name, commit.event_envelope.cursor.as_bytes())?;
        }
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: tempdir.path().join("checkpoint"),
        };

        let cursor_only = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Wallet,
        );
        assert!(matches!(
            cursor_only,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));

        put_wallet_tip_indexes(&derive_store, commit.chain_epoch.visible_tip_height)?;
        let materialized_without_coverage = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Wallet,
        );
        assert!(matches!(
            materialized_without_coverage,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        derive_store.put_consumer_projection_state(
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
            ConsumerProjectionState {
                projection_epoch_id: commit.chain_epoch.id,
                projection_tip_height: commit.chain_epoch.visible_tip_height,
                projection_tip_hash: commit.chain_epoch.visible_tip_hash,
                revision: 1,
                coverage: Some(ConsumerProjectionCoverage {
                    complete_from_height: BlockHeight::new(1),
                    complete_through_height: commit.chain_epoch.visible_tip_height,
                    complete_through_hash: commit.chain_epoch.visible_tip_hash,
                }),
            },
        )?;
        let materialized = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Wallet,
        )?;
        assert_wallet_positions(
            &materialized,
            ProjectionBackupState::Exact,
            Some(commit.chain_epoch.visible_tip_height.value()),
        )?;
        Ok(())
    }

    fn put_wallet_tip_indexes(
        derive_store: &DeriveStore,
        height: BlockHeight,
    ) -> Result<(), zinder_derive::DeriveStoreError> {
        let height_key = zinder_core::wire::encode_height_key_ascending(height);
        derive_store.put_consumer(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            &height_key,
            &[],
        )?;
        derive_store.put_consumer(
            TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            &height_key,
            &[],
        )
    }

    #[test]
    fn expired_wallet_projection_cursor_prevents_backup_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let stale_cursor = commit_three_events_and_prune(&canonical_store)?;
        {
            let derive_store = DeriveStore::open_with_projection_preset(
                DeriveStore::path_for_canonical(&storage_path),
                ProjectionPreset::Wallet,
                DeriveStoreOptions {
                    rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                    ..DeriveStoreOptions::default()
                },
            )?;
            let height_key = zinder_core::wire::encode_height_key_ascending(BlockHeight::new(1));
            for (identity, index_column_family) in [
                (
                    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
                    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
                ),
                (
                    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
                    TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
                ),
            ] {
                derive_store.put_chain_event_cursor(identity, stale_cursor.as_bytes())?;
                derive_store.put_consumer(index_column_family, &height_key, &[])?;
            }
        }
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };

        let outcome =
            create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Wallet);

        assert!(matches!(
            outcome,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        assert!(!checkpoint_path.exists());
        Ok(())
    }

    #[test]
    fn behind_outpoint_cursor_without_contiguous_coverage_prevents_backup_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let first_cursor = commit_event_at(&canonical_store, 1)?;
        commit_event_at(&canonical_store, 2)?;
        {
            let derive_store = DeriveStore::open_with_projection_preset(
                DeriveStore::path_for_canonical(&storage_path),
                ProjectionPreset::Wallet,
                DeriveStoreOptions {
                    rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                    ..DeriveStoreOptions::default()
                },
            )?;
            let height_key = zinder_core::wire::encode_height_key_ascending(BlockHeight::new(1));
            for (identity, index_column_family) in [
                (
                    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
                    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
                ),
                (
                    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
                    TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
                ),
            ] {
                derive_store.put_chain_event_cursor(identity, first_cursor.as_bytes())?;
                derive_store.put_consumer(index_column_family, &height_key, &[])?;
            }
        }
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };

        let outcome =
            create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Wallet);

        assert!(matches!(
            outcome,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        assert!(!checkpoint_path.exists());
        Ok(())
    }

    #[test]
    fn missing_wallet_cursor_without_full_event_history_prevents_backup_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        commit_three_events_and_prune(&canonical_store)?;
        drop(DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?);
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };

        let outcome =
            create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Wallet);

        assert!(matches!(
            outcome,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        assert!(!checkpoint_path.exists());
        Ok(())
    }

    fn commit_three_events_and_prune(
        canonical_store: &PrimaryChainStore,
    ) -> Result<StreamCursorTokenV1, Box<dyn std::error::Error>> {
        let stale_cursor = commit_event_at(canonical_store, 1)?;
        commit_event_at(canonical_store, 2)?;
        commit_event_at(canonical_store, 3)?;
        canonical_store.prune_chain_events_before(UnixTimestampMillis::new(u64::MAX))?;
        Ok(stale_cursor)
    }

    fn commit_event_at(
        canonical_store: &PrimaryChainStore,
        height: u32,
    ) -> Result<StreamCursorTokenV1, Box<dyn std::error::Error>> {
        let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(height);
        let block = fixture
            .block_at(BlockHeight::new(height))
            .ok_or("fixture block missing")?;
        let chain_epoch = fixture
            .chain_epoch(ChainEpochId::new(u64::from(height)))
            .ok_or("fixture epoch missing")?;
        let artifacts = zinder_store::ChainEpochArtifacts::new(
            chain_epoch,
            vec![block.block_header_artifact()],
            vec![block.compact_block_artifact()],
        )
        .with_reorg_window_change(zinder_store::ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(block.height, block.height),
        });
        Ok(canonical_store
            .commit_chain_epoch(artifacts)?
            .event_envelope
            .cursor)
    }

    fn assert_wallet_positions(
        manifest: &BackupManifest,
        expected_state: ProjectionBackupState,
        expected_materialized_height: Option<u32>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for identity in [
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
        ] {
            let position = manifest
                .projections
                .iter()
                .find(|position| position.identity == identity.as_str())
                .ok_or("wallet projection position missing")?;
            assert_eq!(position.state, expected_state, "{}", identity.as_str());
            assert_eq!(
                position.materialized_height,
                expected_materialized_height,
                "{}",
                identity.as_str()
            );
        }
        Ok(())
    }

    #[test]
    fn transaction_history_requires_verified_tip_coverage_for_exact_backup_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let commit = canonical_store.commit_chain_epoch(artifacts)?;
        let chain_epoch = canonical_store
            .current_chain_epoch()?
            .ok_or("canonical epoch missing")?;
        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Complete,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        derive_store.put_chain_event_cursor(
            TRANSACTION_HISTORY_CONSUMER_NAME,
            commit.event_envelope.cursor.as_bytes(),
        )?;
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: tempdir.path().join("checkpoint"),
        };

        let incomplete = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Complete,
        );
        assert!(matches!(
            incomplete,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));

        derive_store.put_consumer_projection_state(
            TRANSACTION_HISTORY_CONSUMER_NAME,
            ConsumerProjectionState {
                projection_epoch_id: chain_epoch.id,
                projection_tip_height: chain_epoch.visible_tip_height,
                projection_tip_hash: chain_epoch.visible_tip_hash,
                revision: 1,
                coverage: Some(ConsumerProjectionCoverage {
                    complete_from_height: zinder_core::BlockHeight::new(1),
                    complete_through_height: chain_epoch.visible_tip_height,
                    complete_through_hash: chain_epoch.visible_tip_hash,
                }),
            },
        )?;
        let manifest = build_backup_manifest(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Complete,
        )?;
        let transaction_history = manifest
            .projections
            .iter()
            .find(|projection| projection.identity == TRANSACTION_HISTORY_CONSUMER_NAME.as_str())
            .ok_or("transaction-history projection missing")?;
        assert_eq!(transaction_history.state, ProjectionBackupState::Exact);
        Ok(())
    }

    #[test]
    fn manifest_failure_never_publishes_a_partial_backup_bundle()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        canonical_store.commit_chain_epoch(artifacts)?;
        {
            let derive_store = DeriveStore::open_with_projection_preset(
                DeriveStore::path_for_canonical(&storage_path),
                ProjectionPreset::Complete,
                DeriveStoreOptions {
                    rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                    ..DeriveStoreOptions::default()
                },
            )?;
            derive_store.put_chain_event_cursor(
                zinder_derive::BLOCK_SUMMARY_CONSUMER_NAME,
                b"malformed-cursor",
            )?;
        }
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };

        assert!(
            create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Complete)
                .is_err()
        );
        assert!(!checkpoint_path.exists());
        assert!(backup_checkpoint_staging_path(&checkpoint_path).exists());
        Ok(())
    }

    #[test]
    fn structurally_tampered_manifest_is_rejected_before_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        canonical_store.commit_chain_epoch(artifacts)?;
        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Complete,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };
        let staging_path = stage_backup_bundle_for_test(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Complete,
        )?;
        drop(derive_store);
        let manifest_path = staging_path.join(BACKUP_MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["canonical_position"]["visible_tip_hash"] = "not-a-block-hash".into();
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        let outcome =
            validate_staged_backup_bundle(&config, &staging_path, ProjectionPreset::Complete);

        assert!(matches!(
            outcome,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        assert!(!checkpoint_path.exists());
        assert!(staging_path.exists());
        Ok(())
    }

    #[test]
    fn mismatched_projection_checkpoint_is_rejected_before_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let checkpoint_path = tempdir.path().join("checkpoint");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let commit = canonical_store.commit_chain_epoch(artifacts)?;
        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            ProjectionPreset::Complete,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        derive_store.put_chain_event_cursor(
            zinder_derive::BLOCK_SUMMARY_CONSUMER_NAME,
            commit.event_envelope.cursor.as_bytes(),
        )?;
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: checkpoint_path.clone(),
        };
        let staging_path = stage_backup_bundle_for_test(
            &config,
            &canonical_store,
            &derive_store,
            ProjectionPreset::Complete,
        )?;
        drop(derive_store);
        {
            let staged_derive = DeriveStore::open_with_projection_preset(
                DeriveStore::path_for_canonical(&staging_path),
                ProjectionPreset::Complete,
                DeriveStoreOptions {
                    rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                    ..DeriveStoreOptions::default()
                },
            )?;
            staged_derive.put_chain_event_cursor(
                zinder_derive::BLOCK_SUMMARY_CONSUMER_NAME,
                b"mismatched-checkpoint-cursor",
            )?;
        }

        assert!(
            validate_staged_backup_bundle(&config, &staging_path, ProjectionPreset::Complete)
                .is_err()
        );
        assert!(!checkpoint_path.exists());
        assert!(staging_path.exists());
        Ok(())
    }

    #[test]
    fn valid_restore_bundle_is_admitted_once_before_writer_use()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_tempdir, config, canonical_store) = backup_fixture(ProjectionPreset::Complete)?;
        create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Complete)?;

        let first_admission = admit_restore_bundle_if_present(
            &config.to_path,
            config.network,
            ProjectionPreset::Complete,
            config.canonical_rocksdb_budget,
            config.derive_rocksdb_budget,
        )?;

        assert_eq!(first_admission, RestoreAdmission::NewlyAdmitted);
        assert!(!config.to_path.join(BACKUP_MANIFEST_FILE_NAME).exists());
        assert!(config.to_path.join(RESTORE_ADMISSION_FILE_NAME).exists());

        let derive_store = DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&config.to_path),
            ProjectionPreset::Complete,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        derive_store
            .put_chain_event_cursor(BLOCK_SUMMARY_CONSUMER_NAME, b"advanced-after-restore")?;
        drop(derive_store);

        let second_admission = admit_restore_bundle_if_present(
            &config.to_path,
            config.network,
            ProjectionPreset::Complete,
            config.canonical_rocksdb_budget,
            config.derive_rocksdb_budget,
        )?;
        assert_eq!(second_admission, RestoreAdmission::PreviouslyAdmitted);
        Ok(())
    }

    #[test]
    fn tampered_restore_manifest_fails_without_consuming_pending_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_tempdir, config, canonical_store) = backup_fixture(ProjectionPreset::Complete)?;
        create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Complete)?;
        let manifest_path = config.to_path.join(BACKUP_MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["network"] = "zcash-testnet".into();
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        let admission = admit_restore_bundle_if_present(
            &config.to_path,
            config.network,
            ProjectionPreset::Complete,
            config.canonical_rocksdb_budget,
            config.derive_rocksdb_budget,
        );

        assert!(matches!(
            admission,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        assert!(manifest_path.exists());
        assert!(!config.to_path.join(RESTORE_ADMISSION_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    fn conflicting_restore_evidence_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let (_tempdir, config, canonical_store) = backup_fixture(ProjectionPreset::Complete)?;
        create_backup_checkpoints(&config, &canonical_store, ProjectionPreset::Complete)?;
        assert_eq!(
            admit_restore_bundle_if_present(
                &config.to_path,
                config.network,
                ProjectionPreset::Complete,
                config.canonical_rocksdb_budget,
                config.derive_rocksdb_budget,
            )?,
            RestoreAdmission::NewlyAdmitted
        );
        fs::copy(
            config.to_path.join(RESTORE_ADMISSION_FILE_NAME),
            config.to_path.join(BACKUP_MANIFEST_FILE_NAME),
        )?;

        let admission = admit_restore_bundle_if_present(
            &config.to_path,
            config.network,
            ProjectionPreset::Complete,
            config.canonical_rocksdb_budget,
            config.derive_rocksdb_budget,
        );
        assert!(matches!(
            admission,
            Err(IngestError::BackupCheckpointValidation { .. })
        ));
        Ok(())
    }

    #[test]
    fn malformed_admission_record_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        fs::write(tempdir.path().join(RESTORE_ADMISSION_FILE_NAME), b"{}")?;

        let admission = admit_restore_bundle_if_present(
            tempdir.path(),
            Network::ZcashRegtest,
            ProjectionPreset::Complete,
            RocksDbResourceBudget::for_local_tests(),
            RocksDbResourceBudget::for_local_tests(),
        );
        assert!(matches!(
            admission,
            Err(IngestError::BackupManifestDecode { .. })
        ));
        Ok(())
    }

    #[test]
    fn ordinary_store_without_restore_evidence_needs_no_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        assert_eq!(
            admit_restore_bundle_if_present(
                tempdir.path(),
                Network::ZcashRegtest,
                ProjectionPreset::Complete,
                RocksDbResourceBudget::for_local_tests(),
                RocksDbResourceBudget::for_local_tests(),
            )?,
            RestoreAdmission::NotApplicable
        );
        Ok(())
    }

    fn backup_fixture(
        projection_preset: ProjectionPreset,
    ) -> Result<
        (tempfile::TempDir, BackupCommandConfig, PrimaryChainStore),
        Box<dyn std::error::Error>,
    > {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("source");
        let canonical_store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let artifacts = ChainFixture::new(Network::ZcashRegtest)
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        canonical_store.commit_chain_epoch(artifacts)?;
        drop(DeriveStore::open_with_projection_preset(
            DeriveStore::path_for_canonical(&storage_path),
            projection_preset,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?);
        let config = BackupCommandConfig {
            network: Network::ZcashRegtest,
            storage_path,
            canonical_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: RocksDbResourceBudget::for_local_tests(),
            to_path: tempdir.path().join("checkpoint"),
        };
        Ok((tempdir, config, canonical_store))
    }

    fn stage_backup_bundle_for_test(
        config: &BackupCommandConfig,
        canonical_store: &PrimaryChainStore,
        derive_store: &DeriveStore,
        projection_preset: ProjectionPreset,
    ) -> Result<PathBuf, IngestError> {
        let staging_path = backup_checkpoint_staging_path(&config.to_path);
        canonical_store.create_checkpoint(&staging_path)?;
        derive_store.create_checkpoint(DeriveStore::path_for_canonical(&staging_path))?;
        let manifest =
            build_backup_manifest(config, canonical_store, derive_store, projection_preset)?;
        write_manifest_atomically(&staging_path, &manifest)?;
        Ok(staging_path)
    }
}
