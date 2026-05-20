//! Shared helpers for `zinder-explorer`'s integration and live tests.
//!
//! Subset of `services/zinder-ingest/tests/common/mod.rs` that the
//! `services/zinder-explorer` test crate needs to backfill against a live
//! upstream node and probe the federated balance read path. Duplicated
//! deliberately so the live gating contract stays colocated with the
//! consumer; a third consumer is the prompt for consolidating into
//! `zinder-testkit::live`.

#![allow(
    dead_code,
    reason = "Each test file consumes only a subset of the common helpers."
)]

use std::{num::NonZeroU32, path::Path};

use eyre::Result;
use zinder_core::BlockHeight;
use zinder_ingest::{BackfillConfig, NodeSourceKind};
use zinder_source::{NodeSource, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_testkit::live::LiveTestEnv;

/// Builds a [`BackfillConfig`] from a resolved live-test env plus per-test
/// runtime knobs.
#[allow(
    clippy::too_many_arguments,
    reason = "Test helper mirrors the resolved BackfillConfig field set."
)]
pub(crate) fn live_backfill_config(
    env: &LiveTestEnv,
    storage_path: &Path,
    from_height: BlockHeight,
    to_height: BlockHeight,
    commit_batch_blocks: NonZeroU32,
    allow_near_tip_finalize: bool,
) -> BackfillConfig {
    const FETCH_CONCURRENCY: NonZeroU32 = NonZeroU32::MIN.saturating_add(7);
    BackfillConfig {
        node: env.target.clone(),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: storage_path.to_owned(),
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        from_height,
        to_height,
        commit_batch_blocks,
        fetch_concurrency: FETCH_CONCURRENCY,
        flush_every_n_epochs: NonZeroU32::MIN.saturating_add(4),
        upstream_tip_hint: None,
        allow_near_tip_finalize,
        checkpoint: None,
    }
}

/// Builds a [`ZebraJsonRpcSource`] from a resolved backfill config.
pub(crate) fn zebra_source_from_backfill(
    backfill_config: &BackfillConfig,
) -> Result<ZebraJsonRpcSource> {
    match backfill_config.node_source {
        NodeSourceKind::ZebraJsonRpc => Ok(ZebraJsonRpcSource::with_options(
            backfill_config.node.network,
            &backfill_config.node.json_rpc_addr,
            backfill_config.node.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: backfill_config.node.request_timeout,
                max_response_bytes: backfill_config.node.max_response_bytes,
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
