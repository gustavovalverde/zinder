//! Fixed-range capture and replay benchmark harness for Zinder.
//!
//! This crate is the validation vehicle for held ingest optimizations
//! (windowed prevout resolver, cache sizing, background-job counts, allocator
//! experiments). It captures the raw source payloads for one dense block range
//! into an immutable fixture, then replays the real bulk-catchup pipeline over
//! that fixture and a cloned canonical store so every run sees identical source
//! bytes and identical starting state. The only time-dependent measurement is
//! wall-clock duration.
//!
//! The harness links `zinder-ingest` and drives its real pipeline entry points;
//! it is a standalone binary that never ships inside a production image.

pub mod capture;
pub mod error;
pub mod fixture;
pub mod metrics_scrape;
pub mod recorder;
pub mod replay;
pub mod report;
pub mod rss;

pub use error::BenchError;
