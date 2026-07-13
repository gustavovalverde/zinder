//! Resumable canonical verification for the transaction-history projection.

use std::{num::NonZeroU32, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::wire::decode_rpc_block_hash_hex;
use zinder_core::{BlockHash, BlockHeight, ChainEpoch};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BlockSummaryConsumer, ConsumerProjectionCoverage,
    ConsumerProjectionState, DeriveStore, TRANSACTION_HISTORY_CONSUMER_NAME,
    TransactionHistoryConsumer, decode_stored_record,
};
use zinder_runtime::Readiness;
use zinder_store::PrimaryChainStore;

use crate::{
    IngestError, derive_consumers::derive_projection_write_guard,
    ingest_loop::wait_until_tip_follow_or_cancelled,
    transaction_component_backfill::read_canonical_context_batch,
};

const VERIFICATION_START_HEIGHT: BlockHeight = BlockHeight::new(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Bounded controls for transaction-history verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionHistoryVerifierConfig {
    /// Whether the verifier runs.
    pub enabled: bool,
    /// Maximum canonical blocks checked per durable coverage update.
    pub batch_blocks: NonZeroU32,
}

/// Existing stores used by the verifier.
#[derive(Clone)]
pub struct TransactionHistoryVerifierContext {
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl TransactionHistoryVerifierContext {
    /// Groups the canonical and derive stores used by verification.
    #[must_use]
    pub fn new(chain_store: PrimaryChainStore, derive_store: DeriveStore) -> Self {
        Self {
            chain_store,
            derive_store,
        }
    }
}

/// Spawns the non-readiness-blocking transaction-history verifier.
#[must_use = "await the handle during shutdown"]
pub fn spawn_transaction_history_verifier_task(
    config: TransactionHistoryVerifierConfig,
    context: TransactionHistoryVerifierContext,
    readiness: Readiness,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "transaction_history_verifier_disabled",
            "transaction-history verification is disabled"
        );
        return None;
    }
    Some(tokio::spawn(run_verifier(
        config, context, readiness, cancel,
    )))
}

async fn run_verifier(
    config: TransactionHistoryVerifierConfig,
    context: TransactionHistoryVerifierContext,
    readiness: Readiness,
    cancel: CancellationToken,
) {
    tracing::info!(
        target: "zinder::ingest",
        event = "transaction_history_verifier_started",
        from_height = VERIFICATION_START_HEIGHT.value(),
        batch_blocks = config.batch_blocks.get(),
        "transaction-history verification started"
    );
    loop {
        if wait_until_tip_follow_or_cancelled(&readiness, &cancel).await {
            return;
        }
        let verification = verify_next_batch(config, context.clone());
        let progress = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_history_verifier_cancelled",
                    "transaction-history verification cancelled"
                );
                return;
            }
            progress = verification => progress,
        };
        match progress {
            Ok(VerificationProgress::Advanced {
                from_height,
                through_height,
                transaction_count,
            }) => tracing::info!(
                target: "zinder::ingest",
                event = "transaction_history_verifier_progress",
                from_height = from_height.value(),
                through_height = through_height.value(),
                transaction_count,
                "transaction-history verified coverage advanced"
            ),
            Ok(VerificationProgress::CaughtUp { through_height }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_history_verifier_completed",
                    through_height = through_height.map(BlockHeight::value),
                    "transaction-history verification is caught up"
                );
                if sleep_or_cancel(CAUGHT_UP_POLL_INTERVAL, &cancel).await {
                    return;
                }
            }
            Ok(VerificationProgress::ProjectionBehind {
                projection_height,
                canonical_height,
            }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_history_verifier_waiting_for_projection",
                    projection_height = projection_height.map(BlockHeight::value),
                    canonical_height = canonical_height.value(),
                    "transaction-history projection is behind canonical visibility"
                );
                if sleep_or_cancel(RETRY_INTERVAL, &cancel).await {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "transaction_history_verifier_retry",
                    error = %error,
                    retry_delay_seconds = RETRY_INTERVAL.as_secs(),
                    "transaction-history verification failed; retaining prior coverage"
                );
                if sleep_or_cancel(RETRY_INTERVAL, &cancel).await {
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerificationProgress {
    Advanced {
        from_height: BlockHeight,
        through_height: BlockHeight,
        transaction_count: usize,
    },
    CaughtUp {
        through_height: Option<BlockHeight>,
    },
    ProjectionBehind {
        projection_height: Option<BlockHeight>,
        canonical_height: BlockHeight,
    },
}

async fn verify_next_batch(
    config: TransactionHistoryVerifierConfig,
    context: TransactionHistoryVerifierContext,
) -> Result<VerificationProgress, IngestError> {
    tokio::task::spawn_blocking(move || verify_next_batch_blocking(config, &context))
        .await
        .map_err(|error| IngestError::BlockingTaskFailed {
            reason: error.to_string(),
        })?
}

fn verify_next_batch_blocking(
    config: TransactionHistoryVerifierConfig,
    context: &TransactionHistoryVerifierContext,
) -> Result<VerificationProgress, IngestError> {
    let state_before = context
        .derive_store
        .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?;
    let next_height = next_verification_height(state_before)?;
    let Some(chain_epoch_before) = context.chain_store.current_chain_epoch()? else {
        return Ok(VerificationProgress::CaughtUp {
            through_height: state_before.and_then(|state| {
                state
                    .coverage
                    .map(|coverage| coverage.complete_through_height)
            }),
        });
    };
    let projection_head = transaction_history_projection_head(&context.derive_store)?;
    let Some((projection_height, projection_hash)) = projection_head else {
        return Ok(VerificationProgress::ProjectionBehind {
            projection_height: None,
            canonical_height: chain_epoch_before.visible_tip_height,
        });
    };
    if projection_height < chain_epoch_before.visible_tip_height {
        return Ok(VerificationProgress::ProjectionBehind {
            projection_height: Some(projection_height),
            canonical_height: chain_epoch_before.visible_tip_height,
        });
    }
    if projection_height > chain_epoch_before.visible_tip_height
        || (projection_height == chain_epoch_before.visible_tip_height
            && projection_hash != chain_epoch_before.visible_tip_hash)
    {
        return Err(IngestError::DeriveDispatch(
            "transaction-history projection head does not match canonical visibility".to_owned(),
        ));
    }
    if next_height > projection_height {
        return Ok(VerificationProgress::CaughtUp {
            through_height: state_before.and_then(|state| {
                state
                    .coverage
                    .map(|coverage| coverage.complete_through_height)
            }),
        });
    }
    let through_height = BlockHeight::new(
        next_height
            .value()
            .saturating_add(config.batch_blocks.get().saturating_sub(1))
            .min(projection_height.value()),
    );
    let contexts = read_canonical_context_batch(&context.chain_store, next_height, through_height)?;
    let transaction_count = verify_context_rows(&context.derive_store, &contexts)?;
    let through_hash = contexts
        .last()
        .ok_or_else(|| {
            IngestError::DeriveDispatch(
                "transaction-history verifier hydrated an empty batch".to_owned(),
            )
        })?
        .block_hash;

    publish_verified_coverage(
        context,
        &VerifiedBatchPublication {
            state_before,
            chain_epoch_before,
            projection_head,
            through_height,
            through_hash,
        },
    )?;

    Ok(VerificationProgress::Advanced {
        from_height: next_height,
        through_height,
        transaction_count,
    })
}

#[derive(Clone, Copy)]
struct VerifiedBatchPublication {
    state_before: Option<ConsumerProjectionState>,
    chain_epoch_before: ChainEpoch,
    projection_head: Option<(BlockHeight, BlockHash)>,
    through_height: BlockHeight,
    through_hash: BlockHash,
}

fn publish_verified_coverage(
    context: &TransactionHistoryVerifierContext,
    publication: &VerifiedBatchPublication,
) -> Result<(), IngestError> {
    let VerifiedBatchPublication {
        state_before,
        chain_epoch_before,
        projection_head,
        through_height,
        through_hash,
    } = *publication;
    let (projection_height, projection_hash) = projection_head.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "transaction-history projection head disappeared before publication".to_owned(),
        )
    })?;
    let _write_guard = derive_projection_write_guard();
    let chain_epoch_after = context.chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical epoch disappeared during transaction-history verification".to_owned(),
        )
    })?;
    if chain_epoch_after.id != chain_epoch_before.id {
        return Err(IngestError::DeriveDispatch(
            "canonical epoch changed during transaction-history verification".to_owned(),
        ));
    }
    let state_after = context
        .derive_store
        .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?;
    if state_after != state_before {
        return Err(IngestError::DeriveDispatch(
            "transaction-history projection state changed during verification".to_owned(),
        ));
    }
    if transaction_history_projection_head(&context.derive_store)? != projection_head {
        return Err(IngestError::DeriveDispatch(
            "transaction-history projection head changed during verification".to_owned(),
        ));
    }
    let revision = state_after
        .map_or(Some(1), |state| state.revision.checked_add(1))
        .ok_or_else(|| {
            IngestError::DeriveDispatch(
                "transaction-history projection revision overflowed".to_owned(),
            )
        })?;
    context.derive_store.put_consumer_projection_state(
        TRANSACTION_HISTORY_CONSUMER_NAME,
        ConsumerProjectionState {
            projection_epoch_id: chain_epoch_after.id,
            projection_tip_height: projection_height,
            projection_tip_hash: projection_hash,
            revision,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: VERIFICATION_START_HEIGHT,
                complete_through_height: through_height,
                complete_through_hash: through_hash,
            }),
        },
    )?;
    Ok(())
}

fn transaction_history_projection_head(
    derive_store: &DeriveStore,
) -> Result<Option<(BlockHeight, BlockHash)>, IngestError> {
    let Some(height) =
        derive_store.last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
    else {
        return Ok(None);
    };
    let payload = derive_store
        .get_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &BlockSummaryConsumer::key_for_height(height),
        )?
        .ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "block-summary projection head {} is unavailable",
                height.value()
            ))
        })?;
    let record = decode_stored_record(&payload)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let summary = record.summary.ok_or_else(|| {
        IngestError::DeriveDispatch("block-summary projection head has no summary".to_owned())
    })?;
    if summary.block_height != height.value() {
        return Err(IngestError::DeriveDispatch(
            "block-summary projection head height does not match its key".to_owned(),
        ));
    }
    let hash = decode_rpc_block_hash_hex(&summary.block_hash)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(Some((height, hash)))
}

fn next_verification_height(
    state: Option<ConsumerProjectionState>,
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = state.and_then(|state| state.coverage) else {
        return Ok(VERIFICATION_START_HEIGHT);
    };
    if coverage.complete_from_height != VERIFICATION_START_HEIGHT {
        return Err(IngestError::DeriveDispatch(format!(
            "transaction-history coverage starts at {}, expected {}",
            coverage.complete_from_height.value(),
            VERIFICATION_START_HEIGHT.value()
        )));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::DeriveDispatch("transaction-history coverage height overflowed".to_owned())
    })
}

fn verify_context_rows(
    derive_store: &DeriveStore,
    contexts: &[zinder_derive::BlockCommitContext],
) -> Result<usize, IngestError> {
    let mut transaction_count = 0_usize;
    for block in contexts {
        let expected = block
            .transactions
            .iter()
            .map(|transaction| TransactionHistoryConsumer::project_entry(block, transaction))
            .collect::<Vec<_>>();
        let actual = TransactionHistoryConsumer::entries_at_height(derive_store, block.height)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
        if actual != expected {
            return Err(IngestError::DeriveDispatch(format!(
                "transaction-history rows at height {} do not match canonical facts",
                block.height.value()
            )));
        }
        transaction_count = transaction_count.saturating_add(expected.len());
    }
    Ok(transaction_count)
}

async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(delay) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use prost::Message as _;
    use zinder_core::{
        BlockHash, LockTime, PrivacyShape, TransactionComponentCounts, TransactionFactsArtifact,
        TransactionId, TransactionLocation, TransactionPublicFacts, TransactionVersion,
    };
    use zinder_derive::{
        BlockCommitContext, BlockCommitPayload, DeriveStoreOptions,
        TRANSACTION_HISTORY_COLUMN_FAMILY, TransparentSpendFacts,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    fn transaction(height: BlockHeight, block_hash: BlockHash) -> TransactionFactsArtifact {
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        TransactionFactsArtifact::new(
            TransactionLocation::new(transaction_id, height, block_hash, 0),
            TransactionPublicFacts {
                transaction_id,
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::V5,
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: 128,
                counts: TransactionComponentCounts::EMPTY,
                privacy_shape: PrivacyShape::TransparentOnly,
                is_coinbase: true,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                unsupported_sections: Vec::new(),
            },
        )
    }

    fn block(height: u32) -> BlockCommitContext {
        let height = BlockHeight::new(height);
        let block_hash = BlockHash::from_bytes([0x22; 32]);
        BlockCommitContext::new(
            BlockCommitPayload {
                height,
                block_hash,
                previous_block_hash: BlockHash::from_bytes([0x21; 32]),
                block_time_unix_seconds: 1_700_000_000,
                block_size_bytes: 256,
                transactions: vec![transaction(height, block_hash)],
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        )
    }

    fn derive_store() -> TestResult<(tempfile::TempDir, DeriveStore)> {
        let directory = tempfile::tempdir()?;
        let store = DeriveStore::open(
            directory.path(),
            DeriveStoreOptions {
                sync_writes: false,
                consumers: DeriveStore::bundled_consumers(),
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        Ok((directory, store))
    }

    #[test]
    fn verifier_accepts_rows_written_by_the_projection_owner() -> TestResult {
        let (_directory, store) = derive_store()?;
        let block = block(1);
        let entry = TransactionHistoryConsumer::project_entry(&block, &block.transactions[0]);
        store.put_consumer(
            TRANSACTION_HISTORY_COLUMN_FAMILY,
            &TransactionHistoryConsumer::key_for_row(block.height, 0),
            &entry.encode_to_vec(),
        )?;

        assert_eq!(verify_context_rows(&store, &[block])?, 1);
        Ok(())
    }

    #[test]
    fn verifier_rejects_a_missing_canonical_row() -> TestResult {
        let (_directory, store) = derive_store()?;
        let result = verify_context_rows(&store, &[block(1)]);

        assert!(
            matches!(result, Err(IngestError::DeriveDispatch(reason)) if reason.contains("do not match canonical facts"))
        );
        Ok(())
    }

    #[test]
    fn verification_resumes_after_the_verified_height() -> TestResult {
        let state = ConsumerProjectionState {
            projection_epoch_id: zinder_core::ChainEpochId::new(7),
            projection_tip_height: BlockHeight::new(20),
            projection_tip_hash: BlockHash::from_bytes([0x33; 32]),
            revision: 2,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(10),
                complete_through_hash: BlockHash::from_bytes([0x44; 32]),
            }),
        };

        assert_eq!(next_verification_height(Some(state))?, BlockHeight::new(11));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_interrupts_verifier_wait() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(sleep_or_cancel(Duration::from_mins(1), &cancel).await);
    }
}
