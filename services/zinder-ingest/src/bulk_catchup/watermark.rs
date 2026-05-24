use std::{
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use parking_lot::Mutex;

use crate::chain_ingest::{IngestError, ingest_error_class};

#[derive(Clone, Debug)]
pub(crate) struct ByteWatermark {
    stage: &'static str,
    limit_bytes: NonZeroU64,
    inner: Arc<Mutex<ByteWatermarkState>>,
}

#[derive(Debug, Default)]
struct ByteWatermarkState {
    reserved_bytes: u64,
    reservations: u32,
}

#[derive(Debug)]
pub(crate) struct ByteReservation {
    watermark: ByteWatermark,
    bytes: u64,
    is_released: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkSnapshot {
    pub(crate) reserved_bytes: u64,
    pub(crate) reservations: u32,
    pub(crate) limit_bytes: u64,
}

impl ByteWatermark {
    pub(crate) fn new(stage: &'static str, limit_bytes: NonZeroU64) -> Self {
        Self {
            stage,
            limit_bytes,
            inner: Arc::new(Mutex::new(ByteWatermarkState::default())),
        }
    }

    pub(crate) fn try_reserve(&self, bytes: u64) -> Option<ByteReservation> {
        let bytes = bytes.max(1);
        let mut state = self.inner.lock();
        let next_reserved_bytes = state.reserved_bytes.checked_add(bytes)?;
        let first_reservation = state.reservations == 0;
        if next_reserved_bytes > self.limit_bytes.get() && !first_reservation {
            metrics::counter!(
                "zinder_ingest_bulk_pipeline_watermark_blocked_total",
                "stage" => self.stage
            )
            .increment(1);
            return None;
        }

        state.reserved_bytes = next_reserved_bytes;
        state.reservations = state.reservations.saturating_add(1);
        let snapshot = self.snapshot_from_state(&state);
        drop(state);
        self.record(snapshot);
        Some(ByteReservation {
            watermark: self.clone(),
            bytes,
            is_released: AtomicBool::new(false),
        })
    }

    pub(crate) fn snapshot(&self) -> WatermarkSnapshot {
        let state = self.inner.lock();
        self.snapshot_from_state(&state)
    }

    fn resize(&self, current_bytes: u64, next_bytes: u64) {
        let mut state = self.inner.lock();
        state.reserved_bytes = state
            .reserved_bytes
            .saturating_sub(current_bytes)
            .saturating_add(next_bytes.max(1));
        let snapshot = self.snapshot_from_state(&state);
        drop(state);
        self.record(snapshot);
    }

    fn release(&self, bytes: u64) {
        let mut state = self.inner.lock();
        state.reserved_bytes = state.reserved_bytes.saturating_sub(bytes);
        state.reservations = state.reservations.saturating_sub(1);
        let snapshot = self.snapshot_from_state(&state);
        drop(state);
        self.record(snapshot);
    }

    fn snapshot_from_state(&self, state: &ByteWatermarkState) -> WatermarkSnapshot {
        WatermarkSnapshot {
            reserved_bytes: state.reserved_bytes,
            reservations: state.reservations,
            limit_bytes: self.limit_bytes.get(),
        }
    }

    fn record(&self, snapshot: WatermarkSnapshot) {
        metrics::gauge!(
            "zinder_ingest_bulk_pipeline_queue_bytes",
            "stage" => self.stage
        )
        .set(u64_to_f64(snapshot.reserved_bytes));
        metrics::gauge!(
            "zinder_ingest_bulk_pipeline_active",
            "stage" => self.stage
        )
        .set(f64::from(snapshot.reservations));
    }
}

impl ByteReservation {
    pub(crate) fn resize(&mut self, next_bytes: u64) {
        if self.is_released.load(Ordering::Acquire) {
            return;
        }
        let next_bytes = next_bytes.max(1);
        self.watermark.resize(self.bytes, next_bytes);
        self.bytes = next_bytes;
    }

    pub(crate) fn release(&self) {
        if self.is_released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.watermark.release(self.bytes);
    }
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) fn record_stage_duration(
    stage: &'static str,
    started_at: Instant,
    stage_error: Option<&IngestError>,
) {
    let status = if stage_error.is_some() { "error" } else { "ok" };
    metrics::histogram!(
        "zinder_ingest_bulk_pipeline_stage_duration_seconds",
        "stage" => stage,
        "status" => status,
        "error_class" => ingest_error_class(stage_error)
    )
    .record(started_at.elapsed());
}

pub(crate) fn record_queue_depth(stage: &'static str, depth: usize) {
    metrics::gauge!(
        "zinder_ingest_bulk_pipeline_queue_depth",
        "stage" => stage
    )
    .set(f64::from(usize_to_u32_saturating(depth)));
}

pub(crate) fn record_reorder_buffer(stage: &'static str, blocks: usize, bytes: u64) {
    metrics::gauge!(
        "zinder_ingest_bulk_pipeline_reorder_buffer_blocks",
        "stage" => stage
    )
    .set(f64::from(usize_to_u32_saturating(blocks)));
    metrics::gauge!(
        "zinder_ingest_bulk_pipeline_reorder_buffer_bytes",
        "stage" => stage
    )
    .set(u64_to_f64(bytes));
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; byte counts are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn reservation_releases_on_drop() -> Result<(), Box<dyn Error>> {
        let watermark = ByteWatermark::new(
            "test_stage",
            NonZeroU64::new(100).ok_or("invalid watermark")?,
        );
        {
            let _reservation = watermark.try_reserve(40).ok_or("reservation should fit")?;
            assert_eq!(watermark.snapshot().reserved_bytes, 40);
        }
        assert_eq!(watermark.snapshot().reserved_bytes, 0);
        Ok(())
    }

    #[test]
    fn reservation_refuses_over_limit_after_first_reservation() -> Result<(), Box<dyn Error>> {
        let watermark = ByteWatermark::new(
            "test_stage",
            NonZeroU64::new(100).ok_or("invalid watermark")?,
        );
        let _first = watermark
            .try_reserve(80)
            .ok_or("first reservation should fit")?;

        assert!(watermark.try_reserve(21).is_none());
        assert_eq!(watermark.snapshot().reserved_bytes, 80);
        Ok(())
    }

    #[test]
    fn first_reservation_can_exceed_limit() -> Result<(), Box<dyn Error>> {
        let watermark = ByteWatermark::new(
            "test_stage",
            NonZeroU64::new(100).ok_or("invalid watermark")?,
        );
        let _reservation = watermark
            .try_reserve(120)
            .ok_or("first reservation should be allowed")?;

        assert_eq!(watermark.snapshot().reserved_bytes, 120);
        Ok(())
    }

    #[test]
    fn reservation_resize_updates_snapshot() -> Result<(), Box<dyn Error>> {
        let watermark = ByteWatermark::new(
            "test_stage",
            NonZeroU64::new(100).ok_or("invalid watermark")?,
        );
        let mut reservation = watermark.try_reserve(30).ok_or("reservation should fit")?;

        reservation.resize(70);

        assert_eq!(watermark.snapshot().reserved_bytes, 70);
        Ok(())
    }
}
