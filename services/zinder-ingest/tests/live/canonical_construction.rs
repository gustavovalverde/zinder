#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::Instant,
};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion, Network,
    NetworkUpgradeActivations,
    wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name},
};
use zinder_ingest::{CanonicalConstructionConfig, load_fresh_canonical_block_replay};
use zinder_source::NodeSource;
use zinder_store::{
    CanonicalBlockReplayLoadEvidence, CanonicalStoreBuildPlan, CanonicalStoreError,
    CanonicalStoreWorkload, RocksDbCanonicalBuilder, RocksDbCanonicalStore, RocksDbIoMode,
    RocksDbResourceBudget,
};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};

use crate::common::{fetch_live_network_upgrade_activations, zebra_source_for_live_env};

const RETAINED_BLOCK_COUNT: u32 = 1_000;
const MIB: u64 = 1_024 * 1_024;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn canonical_replay_loads_1000_blocks_from_fixed_checkpoint() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashTestnet, Network::ZcashMainnet])? else {
        return Ok(());
    };
    let source = zebra_source_for_live_env(&env)?;
    let fixed_tip = source.tip_id().await?;
    let checkpoint_height = fixed_tip
        .height
        .value()
        .checked_sub(RETAINED_BLOCK_COUNT)
        .map(BlockHeight::new)
        .ok_or_else(|| {
            eyre!(
                "canonical replay tracer needs a tip at or above {RETAINED_BLOCK_COUNT}; got {}",
                fixed_tip.height.value()
            )
        })?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    let checkpoint_id = BlockId::new(checkpoint.height, checkpoint.hash);
    let build_plan =
        CanonicalStoreBuildPlan::checkpointed(env.network(), checkpoint_id, fixed_tip)?;
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    let temporary = tempdir()?;
    let store_path = temporary.path().join("canonical");
    let resource_budget = RocksDbResourceBudget::canonical_writer_defaults();
    let config = live_construction_config(&env, activations)?;

    let construction_started_at = Instant::now();
    let builder = RocksDbCanonicalBuilder::create_fresh(
        &store_path,
        CanonicalStoreWorkload::Wallet,
        build_plan,
        resource_budget,
    )?;
    let outcome = load_fresh_canonical_block_replay(builder, &source, config).await?;
    let elapsed = construction_started_at.elapsed();
    assert_and_record_live_evidence(
        build_plan,
        outcome.evidence,
        elapsed,
        outcome.builder.io_mode(),
    );

    drop(outcome.builder);
    let error = RocksDbCanonicalStore::open_ready(
        &store_path,
        env.network(),
        CanonicalStoreWorkload::Wallet,
        resource_budget,
    )
    .err()
    .ok_or_else(|| eyre!("replay-only live construction must remain BUILDING"))?;
    assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));
    assert!(!temporary.path().join("derive").exists());
    Ok(())
}

fn live_construction_config(
    env: &LiveTestEnv,
    activations: Arc<NetworkUpgradeActivations>,
) -> Result<CanonicalConstructionConfig> {
    Ok(CanonicalConstructionConfig {
        request_timeout: env.target.request_timeout,
        max_response_bytes: env.target.max_response_bytes,
        source_segment_target_response_bytes: NonZeroU64::new(
            env.target.max_response_bytes.get().min(12 * MIB),
        )
        .ok_or_else(|| eyre!("invalid source response target"))?,
        source_segment_max_blocks: NonZeroU32::new(8)
            .ok_or_else(|| eyre!("invalid source segment bound"))?,
        source_fetch_max_in_flight_requests: NonZeroU32::new(8)
            .ok_or_else(|| eyre!("invalid source request bound"))?,
        source_fetch_max_in_flight_bytes: NonZeroU64::new(
            env.target.max_response_bytes.get().max(64 * MIB),
        )
        .ok_or_else(|| eyre!("invalid source-fetch byte watermark"))?,
        block_prepare_concurrency: NonZeroU32::new(8)
            .ok_or_else(|| eyre!("invalid prepare concurrency"))?,
        block_prepare_memory_watermark_bytes: NonZeroU64::new(128 * MIB)
            .ok_or_else(|| eyre!("invalid prepare byte watermark"))?,
        network_upgrade_activations: activations,
    })
}

fn assert_and_record_live_evidence(
    build_plan: CanonicalStoreBuildPlan,
    evidence: CanonicalBlockReplayLoadEvidence,
    elapsed: std::time::Duration,
    io_mode: RocksDbIoMode,
) {
    let elapsed_millis = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let blocks_per_second = evidence
        .block_count
        .saturating_mul(1_000)
        .saturating_div(elapsed_millis);
    let logical_mib_per_second = evidence
        .logical_replay_bytes
        .saturating_mul(1_000)
        .saturating_div(elapsed_millis)
        .saturating_div(MIB);
    let fixed_tip = build_plan.build_tip();
    let checkpoint = build_plan.history_predecessor();

    assert_eq!(
        evidence.first_height,
        build_plan.history_bounds().first_available_height()
    );
    assert_eq!(
        evidence.first_parent_hash,
        build_plan.history_predecessor().hash
    );
    assert_eq!(evidence.block_count, u64::from(RETAINED_BLOCK_COUNT));
    assert_eq!(evidence.tip_height, fixed_tip.height);
    assert_eq!(evidence.tip_hash, fixed_tip.hash);
    assert_eq!(
        evidence.replay_format_version,
        CanonicalBlockReplayFormatVersion::V1
    );
    assert_eq!(
        evidence.block_digest_version,
        CanonicalBlockFactsDigestVersion::V1
    );
    assert_eq!(
        evidence.sequence_digest_version,
        CanonicalBlockFactsSequenceDigestVersion::V1
    );
    assert!(evidence.logical_replay_bytes > 0);
    assert!(evidence.sst_file_bytes > 0);
    assert!(evidence.sst_file_count > 0);

    #[allow(
        clippy::print_stderr,
        reason = "calibration test reports range identity and measurements for operator review"
    )]
    {
        eprintln!(
            "canonical_replay_live_evidence network={} checkpoint_height={} checkpoint_hash={} \
             tip_height={} tip_hash={} block_count={} logical_replay_bytes={} sst_file_bytes={} \
             sst_file_count={} sequence_digest={} elapsed_milliseconds={} blocks_per_second={} \
             logical_mib_per_second={} io_mode={io_mode:?}",
            encode_zinder_native_chain_name(build_plan.network()),
            checkpoint.height.value(),
            encode_rpc_block_hash_hex(checkpoint.hash),
            fixed_tip.height.value(),
            encode_rpc_block_hash_hex(fixed_tip.hash),
            evidence.block_count,
            evidence.logical_replay_bytes,
            evidence.sst_file_bytes,
            evidence.sst_file_count,
            hex::encode(evidence.sequence_digest.as_bytes()),
            elapsed_millis,
            blocks_per_second,
            logical_mib_per_second,
        );
    }
}
