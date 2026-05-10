//! Per-crate acceptance binary that aggregates `tests/integration/` and
//! `tests/live/` modules under the workspace test-tier convention.

mod common;
mod integration;
mod live;
