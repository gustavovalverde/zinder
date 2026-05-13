//! Deployment-tier smoke tests.
//!
//! Each test in this module builds the `deploy/single-container` Docker
//! image and asserts that the integrated stack (`zinder-ingest` + `zinder-query`,
//! supervised by `s6-overlay`) serves the public surface end-to-end against
//! the operator's regtest Zebra sidecar. See [Service operations §Validation
//! Tiers](../../../../docs/architecture/service-operations.md#validation-tiers)
//! and ADR-0007 for the topology this exercises.
//!
//! Tests are gated by [`DEPLOY_TEST_IGNORE_REASON`] plus a runtime Docker
//! probe so machines without Docker silently skip; the `ci-deploy` nextest
//! profile is the runner for the matrix that has Docker available.

mod single_container_smoke;
