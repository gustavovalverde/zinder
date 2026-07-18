//! `RocksDB` flush cadence for bulk-catchup commits.
//!
//! Flushing after every committed epoch would serialize the write-ahead
//! log on the hot path; `BulkCatchupFlushState` tracks epochs committed
//! since the last flush so this only flushes on the configured interval,
//! and is a no-op when nothing has been committed since then.

use std::time::Instant;

use zinder_store::PrimaryChainStore;

use super::{
    BULK_STAGE_CANONICAL_FLUSH, BulkCatchupFlushState, record_bulk_pipeline_stage_duration,
};
use crate::chain_ingest::IngestError;

pub(crate) async fn flush_pending_bulk_catchup_writes(
    store: &PrimaryChainStore,
    flush_state: &mut BulkCatchupFlushState,
) -> Result<(), IngestError> {
    if !flush_state.has_pending_epochs() {
        return Ok(());
    }
    flush_primary_chain_store(store).await?;
    flush_state.mark_flushed();
    Ok(())
}

/// Wraps the synchronous `PrimaryChainStore::flush` in a `spawn_blocking`
/// so a multi-second `RocksDB` flush during `BulkCatchup` does not stall
/// the Tokio worker the bulk catchup loop runs on.
async fn flush_primary_chain_store(store: &PrimaryChainStore) -> Result<(), IngestError> {
    let flush_started_at = Instant::now();
    let store = store.clone();
    let flush_outcome = tokio::task::spawn_blocking(move || store.flush())
        .await
        .map_err(|join_error| IngestError::BlockingTaskFailed {
            reason: join_error.to_string(),
        })?
        .map_err(IngestError::from);
    record_bulk_pipeline_stage_duration(
        BULK_STAGE_CANONICAL_FLUSH,
        flush_started_at,
        flush_outcome.as_ref().err(),
    );
    flush_outcome
}
