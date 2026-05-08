//! Per-crate acceptance binary that aggregates `tests/integration/` modules
//! per the workspace test-tier convention ([ADR-0006]).
//!
//! [ADR-0006]: ../../docs/adrs/0006-test-tiers-and-live-config.md

mod integration;
