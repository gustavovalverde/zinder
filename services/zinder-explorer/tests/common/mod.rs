//! Shared helpers for `zinder-explorer`'s integration and live tests.
//!
//! Subset of `services/zinder-ingest/tests/common/mod.rs` that the
//! `services/zinder-explorer` test crate needs to bulk catch up against a live
//! upstream node and probe the federated balance read path. Duplicated
//! deliberately so the live gating contract stays colocated with the
//! consumer; a third consumer is the prompt for consolidating into
//! `zinder-testkit::live`.

#![allow(
    dead_code,
    reason = "Each test file consumes only a subset of the common helpers."
)]

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::Path,
    sync::Arc,
};

use eyre::Result;
use zinder_core::{BlockHeight, NetworkUpgradeActivations};
use zinder_ingest::{BulkCatchupRunConfig, NodeSourceKind};
use zinder_source::{NodeSource, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_testkit::live::LiveTestEnv;

/// Builds a [`BulkCatchupRunConfig`] from a resolved live-test env plus per-test
/// runtime knobs.
#[allow(
    clippy::too_many_arguments,
    reason = "Test helper mirrors the resolved BulkCatchupRunConfig field set."
)]
pub(crate) fn live_bulk_catchup_run_config(
    env: &LiveTestEnv,
    storage_path: &Path,
    from_height: BlockHeight,
    to_height: BlockHeight,
    canonical_batch_max_blocks: NonZeroU32,
    allow_near_tip_finalize: bool,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
) -> BulkCatchupRunConfig {
    const SOURCE_SEGMENT_MAX_BLOCKS: NonZeroU32 = NonZeroU32::MIN.saturating_add(7);
    BulkCatchupRunConfig {
        node: env.target.clone(),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: storage_path.to_owned(),
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations,
        from_height,
        to_height,
        canonical_batch_max_blocks,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .unwrap_or(NonZeroU64::MIN),
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .unwrap_or(NonZeroU32::MIN),
        source_segment_max_blocks: SOURCE_SEGMENT_MAX_BLOCKS,
        source_segment_target_response_bytes: NonZeroU64::new(12 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        source_fetch_max_in_flight_requests: NonZeroU32::new(8).unwrap_or(NonZeroU32::MIN),
        source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        block_prepare_concurrency: SOURCE_SEGMENT_MAX_BLOCKS,
        block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        flush_interval_epochs: NonZeroU32::MIN.saturating_add(4),
        upstream_tip_hint: None,
        allow_near_tip_finalize,
        checkpoint: None,
    }
}

/// Builds a [`ZebraJsonRpcSource`] from a resolved bulk-catchup config.
pub(crate) fn zebra_source_from_bulk_catchup(
    bulk_catchup_config: &BulkCatchupRunConfig,
) -> Result<ZebraJsonRpcSource> {
    match bulk_catchup_config.node_source {
        NodeSourceKind::ZebraJsonRpc => Ok(ZebraJsonRpcSource::with_options(
            bulk_catchup_config.node.network,
            &bulk_catchup_config.node.json_rpc_addr,
            bulk_catchup_config.node.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: bulk_catchup_config.node.request_timeout,
                max_response_bytes: bulk_catchup_config.node.max_response_bytes,
            },
        )?),
    }
}

/// Probes the upstream node tip via a fresh source.
pub(crate) async fn fetch_live_tip_height(env: &LiveTestEnv) -> Result<BlockHeight> {
    let probe_source = ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?;
    Ok(NodeSource::tip_id(&probe_source).await?.height)
}

/// Fetches the node-advertised upgrade table for live bulk-catchup derivation.
pub(crate) async fn fetch_live_network_upgrade_activations(
    env: &LiveTestEnv,
) -> Result<Arc<NetworkUpgradeActivations>> {
    let probe_source = ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?;
    Ok(Arc::new(
        probe_source.fetch_network_upgrade_activations().await?,
    ))
}

/// Calls Zebra's regtest-only `generate` JSON-RPC to mine `block_count` empty
/// blocks. Returns the list of newly mined block hashes.
///
/// Mirrors the helper in `services/zinder-ingest/tests/common/mod.rs` so the
/// derive-plane mempool overlay live test can drive deterministic chain-tip
/// changes without depending on a wallet-side broadcast cycle. Errors on
/// non-regtest networks because Zebra rejects `generate` outside regtest.
pub(crate) async fn regtest_generate_blocks(
    env: &LiveTestEnv,
    block_count: u32,
) -> Result<Vec<String>> {
    let body =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"generate","params":[{block_count}]}}"#);
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre::eyre!(
            "regtest generate({block_count}) curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
        eyre::eyre!("regtest generate response is not JSON: {error}; body={body}")
    })?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre::eyre!(
            "regtest generate({block_count}) RPC returned error: {error_field}"
        ));
    }
    let result_field = parsed.get("result").ok_or_else(|| {
        eyre::eyre!("regtest generate response missing result field; body={body}")
    })?;
    let block_hashes: Vec<String> =
        serde_json::from_value(result_field.clone()).map_err(|error| {
            eyre::eyre!(
                "regtest generate result is not a list of block hashes: {error}; body={body}"
            )
        })?;
    Ok(block_hashes)
}
