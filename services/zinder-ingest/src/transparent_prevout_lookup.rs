//! Bounded transparent-prevout reads used by ingest commit-time paths.
//!
//! Bulk catchup should be paced by the number of transparent prevouts that
//! must be read from the canonical store, not only by block count. This module
//! keeps that lookup shape and its metrics in one place so derive-context
//! hydration and transparent-address spend indexing stay aligned.

use std::{collections::HashMap, time::Instant};

use zinder_core::{TransparentOutPoint, TransparentPrevoutArtifact};
use zinder_store::{ChainEpochReader, StoreError};

use crate::chain_ingest::outcome_status;

const TRANSPARENT_PREVOUT_STORE_LOOKUP_CHUNK_SIZE: usize = 4_096;

#[derive(Clone, Copy)]
pub(crate) enum TransparentPrevoutLookupMode {
    ReaderEpoch,
    WriterCommit,
}

#[derive(Clone, Copy)]
pub(crate) enum TransparentPrevoutLookupStage {
    DeriveContext,
    SpendAddressIndex,
}

impl TransparentPrevoutLookupStage {
    const fn metric_label(self) -> &'static str {
        match self {
            Self::DeriveContext => "derive_context",
            Self::SpendAddressIndex => "spend_address_index",
        }
    }
}

#[derive(Clone, Copy)]
struct PrevoutStoreLookupProgress {
    requested_outpoints: usize,
    chunk_count: usize,
    completed_outpoints: usize,
    completed_chunks: usize,
    is_active: bool,
}

pub(crate) fn read_chunked_transparent_prevouts_by_outpoints(
    reader: &ChainEpochReader<'_>,
    mode: TransparentPrevoutLookupMode,
    stage: TransparentPrevoutLookupStage,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>, StoreError> {
    if outpoints.is_empty() {
        record_prevout_store_lookup_progress(
            stage,
            PrevoutStoreLookupProgress {
                requested_outpoints: 0,
                chunk_count: 0,
                completed_outpoints: 0,
                completed_chunks: 0,
                is_active: false,
            },
        );
        return Ok(HashMap::new());
    }

    let chunk_count = outpoints
        .len()
        .div_ceil(TRANSPARENT_PREVOUT_STORE_LOOKUP_CHUNK_SIZE);
    record_prevout_store_lookup_progress(
        stage,
        PrevoutStoreLookupProgress {
            requested_outpoints: outpoints.len(),
            chunk_count,
            completed_outpoints: 0,
            completed_chunks: 0,
            is_active: true,
        },
    );
    let mut completed_outpoints = 0usize;
    let mut completed_chunks = 0usize;
    let mut resolved = HashMap::with_capacity(outpoints.len());

    for chunk in outpoints.chunks(TRANSPARENT_PREVOUT_STORE_LOOKUP_CHUNK_SIZE) {
        let chunk_started_at = Instant::now();
        let chunk_outcome = match mode {
            TransparentPrevoutLookupMode::ReaderEpoch => {
                reader.transparent_prevouts_by_outpoints(chunk)
            }
            TransparentPrevoutLookupMode::WriterCommit => {
                reader.transparent_prevouts_by_outpoints_for_writer_commit(chunk)
            }
        };
        record_prevout_store_lookup_chunk(stage, chunk_started_at, chunk.len(), &chunk_outcome);
        let prevouts_by_outpoint = chunk_outcome?;
        resolved.extend(prevouts_by_outpoint);

        completed_outpoints = completed_outpoints.saturating_add(chunk.len());
        completed_chunks = completed_chunks.saturating_add(1);
        record_prevout_store_lookup_progress(
            stage,
            PrevoutStoreLookupProgress {
                requested_outpoints: outpoints.len(),
                chunk_count,
                completed_outpoints,
                completed_chunks,
                is_active: true,
            },
        );
    }

    record_prevout_store_lookup_progress(
        stage,
        PrevoutStoreLookupProgress {
            requested_outpoints: outpoints.len(),
            chunk_count,
            completed_outpoints,
            completed_chunks,
            is_active: false,
        },
    );
    Ok(resolved)
}

fn record_prevout_store_lookup_chunk<T>(
    stage: TransparentPrevoutLookupStage,
    started_at: Instant,
    chunk_outpoint_count: usize,
    outcome: &Result<T, StoreError>,
) {
    metrics::histogram!(
        "zinder_ingest_prevout_store_lookup_chunk_duration_seconds",
        "stage" => stage.metric_label(),
        "status" => outcome_status(outcome),
        "error_class" => store_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::histogram!(
        "zinder_ingest_prevout_store_lookup_chunk_outpoint_count",
        "stage" => stage.metric_label(),
        "status" => outcome_status(outcome)
    )
    .record(usize_to_u32_saturating(chunk_outpoint_count));
    metrics::counter!(
        "zinder_ingest_prevout_store_lookup_chunks_total",
        "stage" => stage.metric_label(),
        "status" => outcome_status(outcome),
        "error_class" => store_error_class(outcome.as_ref().err())
    )
    .increment(1);
}

fn record_prevout_store_lookup_progress(
    stage: TransparentPrevoutLookupStage,
    progress: PrevoutStoreLookupProgress,
) {
    metrics::gauge!(
        "zinder_ingest_prevout_store_lookup_active",
        "stage" => stage.metric_label()
    )
    .set(if progress.is_active { 1.0 } else { 0.0 });
    metrics::gauge!(
        "zinder_ingest_prevout_store_lookup_requested_outpoints",
        "stage" => stage.metric_label()
    )
    .set(f64::from(usize_to_u32_saturating(
        progress.requested_outpoints,
    )));
    metrics::gauge!(
        "zinder_ingest_prevout_store_lookup_completed_outpoints",
        "stage" => stage.metric_label()
    )
    .set(f64::from(usize_to_u32_saturating(
        progress.completed_outpoints,
    )));
    metrics::gauge!(
        "zinder_ingest_prevout_store_lookup_chunks",
        "stage" => stage.metric_label()
    )
    .set(f64::from(usize_to_u32_saturating(progress.chunk_count)));
    metrics::gauge!(
        "zinder_ingest_prevout_store_lookup_completed_chunks",
        "stage" => stage.metric_label()
    )
    .set(f64::from(usize_to_u32_saturating(
        progress.completed_chunks,
    )));
}

fn store_error_class(error: Option<&StoreError>) -> &'static str {
    match error {
        None => "none",
        Some(StoreError::StorageUnavailable { .. }) => "storage_unavailable",
        Some(StoreError::PrimaryAlreadyOpen { .. }) => "primary_already_open",
        Some(StoreError::SecondaryCatchupFailed { .. }) => "secondary_catchup_failed",
        Some(StoreError::ArtifactMissing { .. }) => "artifact_missing",
        Some(StoreError::ArtifactCorrupt { .. }) => "artifact_corrupt",
        Some(StoreError::Unsupported { .. }) => "unsupported",
        Some(_) => "store",
    }
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}
