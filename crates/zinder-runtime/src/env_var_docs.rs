//! Operator-facing environment-variable contract for Zinder service binaries.
//!
//! The `ZINDER_*` env-var schema is part of Zinder's public interface (see
//! [Public interfaces §Environment variable mapping]). This module is the
//! single source of truth for the operator-facing surface: the entries in
//! [`ENVIRONMENT_VARIABLES`] feed both the rendered Markdown table embedded
//! in `docs/architecture/public-interfaces.md` and the CI drift test that
//! catches doc/source skew.
//!
//! [`render_environment_variable_table`] returns the Markdown the doc block
//! must contain. Tests assert exact equality; the doc-mirror integration
//! test prints the expected rendering on failure so the operator can
//! copy-paste the canonical output back into the doc.
//!
//! Adding a new env-var to a service config is a two-line change here plus
//! the corresponding field on the relevant config struct. The mirror test
//! makes drift in either direction a build failure.
//!
//! [Public interfaces §Environment variable mapping]:
//!     ../../docs/architecture/public-interfaces.md#environment-variable-mapping

use std::fmt::Write as _;

/// One operator-facing environment variable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EnvVarDoc {
    /// Variable name in the canonical `ZINDER_<SECTION>__<FIELD>` form.
    pub name: &'static str,
    /// The TOML config field the variable resolves to. Mirrors the inverse
    /// mapping from the env-var name back to its config struct's field path.
    pub toml_path: &'static str,
    /// Which Zinder binaries consume this variable. Used for the "Used by"
    /// column so operators can scan the table for one runtime at a time.
    pub used_by: &'static [&'static str],
    /// Whether the field is required, optional, or required-when-conditioned
    /// (string descriptors keep the column compact).
    pub requirement: Requirement,
    /// Whether the value is sensitive (redacted in `--print-config`, structured
    /// logs, and `Debug` impls).
    pub sensitive: bool,
    /// One-line operator-facing description.
    pub description: &'static str,
}

/// Whether an env var is required, optional, or conditional.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Requirement {
    /// Binary fails to start without this variable (or its config-file
    /// equivalent).
    Required,
    /// Variable is optional; a default applies when unset.
    Optional,
    /// Required only when the named precondition is met (e.g. another env
    /// var has a specific value).
    ConditionalOn(&'static str),
}

impl Requirement {
    fn label(self) -> String {
        match self {
            Self::Required => "Required".to_owned(),
            Self::Optional => "Optional".to_owned(),
            Self::ConditionalOn(condition) => format!("When `{condition}`"),
        }
    }
}

/// Operator-facing `ZINDER_*` environment variables advertised by Zinder
/// service binaries.
///
/// Mirrored into `docs/architecture/public-interfaces.md` between the
/// `<!-- env-var-table:public-interfaces:start -->` and corresponding `:end`
/// markers; the doc-mirror test in
/// `crates/zinder-runtime/tests/integration/env_var_docs.rs` fails when the
/// rendered table and the doc block diverge.
///
/// `ZINDER_TEST_*` variables are intentionally absent: they are stripped at
/// the runtime config loader so production binaries never observe them.
/// Live-test environment knobs are documented next to the affected tests in
/// `CLAUDE.md` and `docs/runbooks/testing.md`.
pub const ENVIRONMENT_VARIABLES: &[EnvVarDoc] = &[
    EnvVarDoc {
        name: "ZINDER_NETWORK__NAME",
        toml_path: "network.name",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Network identifier: `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`. \
                      Note: live-test gating reads the bare `ZINDER_NETWORK` env var directly \
                      and never reaches the config loader, so test runbooks still quote that form.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__JSON_RPC_ADDR",
        toml_path: "node.json_rpc_addr",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Upstream Zebra JSON-RPC URL the service connects to. Optional for \
                      `zinder-explorer`: without it the upstream-observation probe stays \
                      off and `ExplorerFreshness.upstream` is always unset.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__INDEXER_GRPC_ADDR",
        toml_path: "node.indexer_grpc_addr",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Optional Zebra indexer gRPC endpoint enabling the streaming mempool source \
                      and chain-tip wakeups. Falls back to JSON-RPC polling when unset.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__METHOD",
        toml_path: "node.auth.method",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Upstream-node auth shape: `basic`, `cookie`, or unset for no auth.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__USERNAME",
        toml_path: "node.auth.username",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=basic"),
        sensitive: false,
        description: "Basic-auth username. Paired with `ZINDER_NODE__AUTH__PASSWORD`.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__PASSWORD",
        toml_path: "node.auth.password",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=basic"),
        sensitive: true,
        description: "Basic-auth password. Redacted in `--print-config` and structured logs.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__PATH",
        toml_path: "node.auth.path",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=cookie"),
        sensitive: false,
        description: "Path to a cookie file. Mutually exclusive with `ZINDER_NODE__AUTH__COOKIE`.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__COOKIE",
        toml_path: "node.auth.cookie",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=cookie"),
        sensitive: true,
        description: "Inline cookie credentials (`username:password`). Mutually exclusive with \
                      `ZINDER_NODE__AUTH__PATH`. Accepted for PaaS environments without \
                      persistent disks.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__REQUEST_TIMEOUT_SECS",
        toml_path: "node.request_timeout_secs",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Upstream-node JSON-RPC request timeout in seconds. Defaults to 30.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__MAX_RESPONSE_BYTES",
        toml_path: "node.max_response_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum JSON-RPC response body size (bytes) accepted from the node.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__BROADCAST_TIMEOUT_SECS",
        toml_path: "node.broadcast_timeout_secs",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Per-call timeout (seconds) applied only to `sendrawtransaction`. When \
                      unset, the global `request_timeout_secs` applies instead. Recommended: 7.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__HEALTH__ADDR",
        toml_path: "node.health.addr",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "URL of the upstream's HTTP `/ready` endpoint. When set, the writer polls \
                      it as the primary upstream-sync signal; when unset, the writer falls back \
                      to `getblockchaininfo.verificationprogress`/`estimatedheight`. See \
                      [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md).",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__HEALTH__POLL_INTERVAL_MS",
        toml_path: "node.health.poll_interval_ms",
        used_by: &["zinder-ingest", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Cadence of the upstream-health probe in milliseconds. Defaults to 30000. \
                      Must be greater than zero. `zinder-explorer` reuses the same cadence for \
                      its upstream-observation probe (the one that populates \
                      `ExplorerFreshness.upstream`).",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__HEALTH__VERIFICATION_PROGRESS_FLOOR",
        toml_path: "node.health.verification_progress_floor",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Lower bound on `getblockchaininfo.verificationprogress` below which the \
                      fallback path reports `upstream_not_ready`. Defaults to 0.999. Must be in \
                      `(0.0, 1.0)`.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__HEALTH__ESTIMATED_GAP_FLOOR_BLOCKS",
        toml_path: "node.health.estimated_gap_floor_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Block gap between `estimatedheight` and the local tip above which the \
                      fallback path reports `upstream_not_ready`. Defaults to 10.",
    },
    EnvVarDoc {
        name: "ZINDER_OPS__LISTEN_ADDR",
        toml_path: "ops.listen_addr",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Listen address for the operational HTTP endpoint \
                      (`/healthz`, `/readyz`, `/metrics`). Defaults to a per-service \
                      loopback address (`127.0.0.1:9105` ingest, `9106` query, `9107` \
                      compat, `9069` explorer). Set to an empty string to disable the \
                      endpoint entirely.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST_CONTROL__LISTEN_ADDR",
        toml_path: "ingest_control.listen_addr",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Listen address of the private IngestControl gRPC endpoint. Localhost-only \
                      by default; cross-host deployments must add bearer-token auth per ADR-0006. \
                      Set to an empty string to disable the endpoint for diagnostic one-shot runs \
                      (such as `--target-height` pre-seed).",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST_CONTROL__ADDR",
        toml_path: "ingest_control.addr",
        used_by: &["zinder-query", "zinder-compat-lightwalletd"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "URL of the colocated IngestControl writer (`http://host:port`). Readers \
                      use it for tip-change subscriptions, mempool reads, and writer-status \
                      lookups. Defaults to `http://127.0.0.1:9100`.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST_CONTROL__BEARER_TOKEN_PATH",
        toml_path: "ingest_control.bearer_token_path",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::ConditionalOn("ingest enforces auth"),
        sensitive: false,
        description: "Path to the shared-secret bearer token the IngestControl endpoint enforces \
                      on every request (ADR-0006). The writer reads it to verify; the readers \
                      read the same file to present. File-only by policy; inline secrets are \
                      rejected at config load.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__PATH",
        toml_path: "storage.path",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Canonical RocksDB store path. Writers open it as primary; readers open it \
                      as a secondary.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__SECONDARY_PATH",
        toml_path: "storage.secondary_path",
        used_by: &[
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Process-unique RocksDB secondary metadata directory. Never share this path \
                      across reader processes.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__INITIAL_CATCHUP_TIMEOUT_MS",
        toml_path: "storage.initial_catchup_timeout_ms",
        used_by: &[
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum startup RocksDB secondary catchup duration before a reader starts \
                      with the opened secondary and lets /readyz report replica lag. Defaults to \
                      30000.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__BLOCK_CACHE_BYTES",
        toml_path: "storage.canonical.rocksdb.block_cache_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store RocksDB block cache budget in bytes. Defaults to 536870912 \
                      for writers and 134217728 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WAL_BYTES",
        toml_path: "storage.canonical.rocksdb.max_wal_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store RocksDB live WAL ceiling in bytes. Defaults to 268435456 \
                      for writers and 33554432 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_OPEN_FILES",
        toml_path: "storage.canonical.rocksdb.max_open_files",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store RocksDB open SST file cap. Defaults to 512 for writers and \
                      128 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__WRITE_BUFFER_BYTES",
        toml_path: "storage.canonical.rocksdb.write_buffer_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store per-column-family RocksDB write buffer size. Defaults to \
                      16777216 for writers and 8388608 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WRITE_BUFFER_COUNT",
        toml_path: "storage.canonical.rocksdb.max_write_buffer_count",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store per-column-family mutable plus immutable RocksDB write \
                      buffer count. Defaults to 2.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__CANONICAL__ROCKSDB__MEMTABLE_BUDGET_BYTES",
        toml_path: "storage.canonical.rocksdb.memtable_budget_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-explorer",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical-store total RocksDB memtable budget across column families. \
                      Defaults to 268435456 for writers and 16777216 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__BLOCK_CACHE_BYTES",
        toml_path: "storage.derive.rocksdb.block_cache_bytes",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store RocksDB block cache budget in bytes. Defaults to 134217728 for \
                      writers and 67108864 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WAL_BYTES",
        toml_path: "storage.derive.rocksdb.max_wal_bytes",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store RocksDB live WAL ceiling in bytes. Defaults to 67108864 for \
                      writers and 16777216 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_OPEN_FILES",
        toml_path: "storage.derive.rocksdb.max_open_files",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store RocksDB open SST file cap. Defaults to 256 for writers and 64 \
                      for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__WRITE_BUFFER_BYTES",
        toml_path: "storage.derive.rocksdb.write_buffer_bytes",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store per-column-family RocksDB write buffer size. Defaults to \
                      8388608 for writers and 4194304 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WRITE_BUFFER_COUNT",
        toml_path: "storage.derive.rocksdb.max_write_buffer_count",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store per-column-family mutable plus immutable RocksDB write buffer \
                      count. Defaults to 2.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__DERIVE__ROCKSDB__MEMTABLE_BUDGET_BYTES",
        toml_path: "storage.derive.rocksdb.memtable_budget_bytes",
        used_by: &["zinder-ingest", "zinder-query", "zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive-store total RocksDB memtable budget across column families. Defaults \
                      to 67108864 for writers and 16777216 for readers.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__SOURCE",
        toml_path: "ingest.source",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Source-adapter selector. Lives on `[ingest]` (not `[node]`) because the \
                      choice is a writer-private implementation decision: `[node]` describes the \
                      upstream node itself, `[ingest].source` describes which adapter ingest \
                      uses to talk to it. See [ADR-0016](../adrs/0016-source-streaming-pipeline.md).",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__RAW_BLOB_POLICY",
        toml_path: "storage.raw_blob_policy",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Raw-byte blob write policy: `none`, `transactions`, or `all`. Defaults to \
                      `none` so fact-first indexing does not write raw block or transaction blobs \
                      unless a deployment explicitly needs raw export.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__REORG_WINDOW_BLOCKS",
        toml_path: "ingest.reorg_window_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Chain-truth invariant: how deep the live reorg window extends. Bounds \
                      finalization, classifier default, and replacement traversal. Must be \
                      greater than zero. Defaults to 100.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__PHASES__CATCHUP_THRESHOLD_BLOCKS",
        toml_path: "ingest.phases.catchup_threshold_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Gap (in blocks) at which the unified loop transitions between \
                      `BulkCatchup` and `TipFollow`. Defaults to `ingest.reorg_window_blocks`. \
                      See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md).",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_BLOCKS",
        toml_path: "ingest.bulk_catchup.canonical_batch_max_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Block count per bulk-catchup commit batch. Defaults to 1000.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_ARTIFACT_BYTES",
        toml_path: "ingest.bulk_catchup.canonical_batch_max_artifact_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Canonical artifact bytes accumulated before closing a bulk-catchup batch. \
                      Defaults to 536870912.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES",
        toml_path: "ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Estimated canonical write bytes accumulated before closing a \
                      bulk-catchup batch. Defaults to 536870912.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE",
        toml_path: "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Minimum blocks accumulated before estimated write bytes can close a \
                      bulk-catchup batch. Single oversized blocks can still close immediately. \
                      Defaults to 100.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__SOURCE_SEGMENT_MAX_BLOCKS",
        toml_path: "ingest.bulk_catchup.source_segment_max_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum connected blocks requested from the source in one bulk-catchup \
                      segment. Defaults to 16.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__SOURCE_SEGMENT_TARGET_RESPONSE_BYTES",
        toml_path: "ingest.bulk_catchup.source_segment_target_response_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Target source response bytes for adaptive segment sizing. Defaults to \
                      33554432.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS",
        toml_path: "ingest.bulk_catchup.source_fetch_max_in_flight_requests",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum concurrent source segment requests. Defaults to 12.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__SOURCE_FETCH_MAX_IN_FLIGHT_BYTES",
        toml_path: "ingest.bulk_catchup.source_fetch_max_in_flight_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum reserved source response bytes across active fetches and completed \
                      source reassembly. Must be greater than or equal to \
                      node.max_response_bytes. Defaults to 402653184.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__BLOCK_PREPARE_CONCURRENCY",
        toml_path: "ingest.bulk_catchup.block_prepare_concurrency",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Parallel canonical block-prepare slots. Defaults to \
                      `min(available_parallelism(), 16)`.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES",
        toml_path: "ingest.bulk_catchup.block_prepare_max_in_flight_artifact_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum reserved derived artifact bytes across active and completed \
                      block-prepare work. Defaults to 536870912.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES",
        toml_path: "ingest.bulk_catchup.commit_reassembly_max_queued_artifact_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum safe-tip artifact bytes that can accumulate while the previous \
                      bulk-catchup batch is attaching metadata, committing, or flushing. \
                      Defaults to 536870912.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__REPLAY_BATCH_BLOCKS",
        toml_path: "ingest.derive.replay_batch_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum block contexts hydrated and dispatched in one derive replay write. \
                      Must be greater than zero. Defaults to 100.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__REPLAY_POLICY",
        toml_path: "ingest.derive.replay_policy",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Derive replay pressure policy. `canonical-first` pauses rebuildable derive \
                      replay under memory pressure so canonical ingest keeps the process budget. \
                      `continuous` replays retained chain events whenever they are available. \
                      Defaults to `canonical-first`.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__MEMORY_BUDGET_BYTES",
        toml_path: "ingest.derive.memory_budget_bytes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Explicit derive replay memory budget in bytes. When unset, derive replay \
                      uses the runtime cgroup `memory.high` or `memory.max` value when present.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__MEMORY_DEGRADE_RATIO",
        toml_path: "ingest.derive.memory_degrade_ratio",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Memory pressure ratio at which derive replay shrinks the effective replay \
                      batch size. Defaults to 0.90.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__MEMORY_PAUSE_RATIO",
        toml_path: "ingest.derive.memory_pause_ratio",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Memory pressure ratio at which canonical-first derive replay pauses. \
                      Defaults to 0.99.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__MEMORY_RESUME_RATIO",
        toml_path: "ingest.derive.memory_resume_ratio",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Memory pressure ratio below which degraded derive replay returns to the \
                      normal replay batch size. Paused replay resumes as degraded work once \
                      pressure falls below memory_pause_ratio. Defaults to 0.80.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__DERIVE__MIN_REPLAY_BATCH_BLOCKS",
        toml_path: "ingest.derive.min_replay_batch_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Smallest effective derive replay batch size under memory degradation. \
                      Must be greater than zero and no larger than replay_batch_blocks. Defaults \
                      to 10.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__BULK_CATCHUP__FLUSH_INTERVAL_EPOCHS",
        toml_path: "ingest.bulk_catchup.flush_interval_epochs",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Bulk-catchup RocksDB flush cadence in committed epochs. Must be greater \
                      than zero. Defaults to 5.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__TIP_FOLLOW__POLL_INTERVAL_MS",
        toml_path: "ingest.tip_follow.poll_interval_ms",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Tip-follow poll cadence in milliseconds. Must be greater than zero. \
                      Defaults to 1000.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__TIP_FOLLOW__LAG_THRESHOLD_BLOCKS",
        toml_path: "ingest.tip_follow.lag_threshold_blocks",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Block lag at which tip-follow reports `cause=syncing`. Defaults to 1.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__MODIFIERS__TARGET_HEIGHT",
        toml_path: "ingest.modifiers.target_height",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "One-shot stop-at modifier; the loop exits 0 after committing this \
                      height. Renamed from `to_height`.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__MODIFIERS__CHECKPOINT_HEIGHT",
        toml_path: "ingest.modifiers.checkpoint_height",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Pre-seed an empty store from an upstream-supplied checkpoint at this \
                      height.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__MODIFIERS__ALLOW_NEAR_TIP_FINALIZE",
        toml_path: "ingest.modifiers.allow_near_tip_finalize",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Disposable-store override: lets bulk-catchup finalize inside the reorg \
                      window. Invalid combined with `coverage = \"wallet-serving\"`.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__MODIFIERS__COVERAGE",
        toml_path: "ingest.modifiers.coverage",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Ingest coverage mode: `\"explicit\"` or `\"wallet-serving\"`. Defaults to \
                      `\"explicit\"`.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__CHAIN_EVENT_RETENTION_HOURS",
        toml_path: "retention.chain_event_retention_hours",
        used_by: &["zinder-ingest", "zinder-query"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Chain-event retention window in hours, enforced by `zinder-ingest` and \
                      advertised by `zinder-query` through `ServerInfo`. Defaults to 168 (7 days). \
                      `0` disables eviction.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS",
        toml_path: "retention.chain_event_retention_check_interval_ms",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Chain-event retention sweep cadence in milliseconds. Must be greater than \
                      zero. Defaults to 60000 (one minute).",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__CURSOR_AT_RISK_WARNING_HOURS",
        toml_path: "retention.cursor_at_risk_warning_hours",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Cursor-at-risk warning lead time in hours. Must be \u{2264} \
                      `retention.chain_event_retention_hours`. Defaults to 24.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__MEMPOOL_MINED_RETENTION_MINUTES",
        toml_path: "retention.mempool_mined_retention_minutes",
        used_by: &["zinder-ingest", "zinder-query"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Mined-mempool retention window in minutes, enforced by `zinder-ingest` and \
                      advertised by `zinder-query`. Defaults to 60. `0` disables retention.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__MEMPOOL_INVALIDATED_RETENTION_HOURS",
        toml_path: "retention.mempool_invalidated_retention_hours",
        used_by: &["zinder-ingest", "zinder-query"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Invalidated-mempool retention window in hours, enforced by `zinder-ingest` \
                      and advertised by `zinder-query`. Defaults to 24. `0` disables retention.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS",
        toml_path: "retention.mempool_event_retention_check_interval_ms",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Mempool-event retention sweep cadence in milliseconds. Must be greater \
                      than zero. Defaults to 30000.",
    },
    EnvVarDoc {
        name: "ZINDER_RETENTION__MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES",
        toml_path: "retention.mempool_cursor_at_risk_warning_minutes",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Mempool cursor-at-risk warning lead time in minutes. Must be \u{2264} the \
                      shortest configured mempool retention window. Defaults to 12.",
    },
    EnvVarDoc {
        name: "ZINDER_EXPLORER__BEARER_TOKEN_PATH",
        toml_path: "explorer.bearer_token_path",
        used_by: &["zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Path to the shared-secret bearer token the ExplorerQuery endpoint enforces \
                      on cross-service explorer-plane reads (ADR-0006).",
    },
    EnvVarDoc {
        name: "ZINDER_EXPLORER__LISTEN_ADDR",
        toml_path: "explorer.listen_addr",
        used_by: &["zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Listen address for the ExplorerQuery gRPC endpoint. Defaults to 127.0.0.1:9068.",
    },
    EnvVarDoc {
        name: "ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT",
        toml_path: "explorer.wallet_query_endpoint",
        used_by: &["zinder-explorer"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "WalletQuery gRPC endpoint backing the federated `TransparentAddressBalance` compute path. \
                      Empty/unset disables the `explorer.transparent_address.balance_v1` capability.",
    },
];

/// Renders [`ENVIRONMENT_VARIABLES`] as a GitHub-flavored Markdown table.
///
/// The rendered block is the canonical content of the
/// `env-var-table:public-interfaces` doc block; the doc-mirror integration
/// test fails when the doc and renderer diverge so adding a field to one
/// without the other is caught at build time.
#[must_use]
pub fn render_environment_variable_table() -> String {
    let mut output = String::new();
    output.push_str("| Variable | Used by | Requirement | TOML field | Description |\n");
    output.push_str("| -------- | ------- | ----------- | ---------- | ----------- |\n");
    for env_var in ENVIRONMENT_VARIABLES {
        let used_by = env_var.used_by.join(", ");
        let toml_field = format!("`{}`", env_var.toml_path);
        let sensitivity_marker = if env_var.sensitive {
            " (sensitive; redacted)"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "| `{}` | {} | {} | {} | {}{} |",
            env_var.name,
            used_by,
            env_var.requirement.label(),
            toml_field,
            env_var.description,
            sensitivity_marker,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_table_includes_required_network_row() {
        let rendered = render_environment_variable_table();
        assert!(rendered.contains("| `ZINDER_NETWORK__NAME` |"));
        assert!(rendered.contains("Required"));
        assert!(rendered.contains("zcash-mainnet"));
    }

    #[test]
    fn sensitive_variables_carry_redaction_marker_in_table() {
        let rendered = render_environment_variable_table();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("| `ZINDER_NODE__AUTH__PASSWORD`")
                    && line.contains("(sensitive; redacted)")),
            "sensitive rows must surface the redaction marker so operators do not \
             expect to see plaintext in --print-config:\n{rendered}",
        );
    }

    #[test]
    fn conditional_requirement_renders_when_clause() {
        let rendered = render_environment_variable_table();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("| `ZINDER_NODE__AUTH__USERNAME`")
                    && line.contains("When `ZINDER_NODE__AUTH__METHOD=basic`")),
            "conditional rows must render their precondition:\n{rendered}",
        );
    }

    #[test]
    fn every_variable_starts_with_zinder_prefix() {
        for env_var in ENVIRONMENT_VARIABLES {
            assert!(
                env_var.name.starts_with("ZINDER_"),
                "documented variables must follow the ZINDER_* convention; got {}",
                env_var.name,
            );
        }
    }
}
