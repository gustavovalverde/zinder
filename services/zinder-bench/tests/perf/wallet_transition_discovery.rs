#![allow(
    missing_docs,
    reason = "manual benchmark helpers are described by the report contract"
)]

use std::{
    env, fs,
    num::{NonZeroU32, NonZeroU64},
    path::Path,
    time::Instant,
};

use eyre::{Result, WrapErr, ensure, eyre};
use rust_rocksdb::{PerfContext, PerfMetric, PerfStatsLevel, perf::set_perf_stats};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId, CanonicalBlockFacts,
    CanonicalBlockFactsDigestVersion, CanonicalBlockReplayFormatVersion, CanonicalTransactionFacts,
    ChainTipMetadata, CommitmentTreeCheckpoint, CommitmentTreeFrontiers, CompactChainMetadata,
    ConsensusBranchId, LockTime, Network, NetworkUpgradeActivation, NetworkUpgradeActivations,
    PrivacyShape, SerializedBytesDigest, TransactionBlobArtifact, TransactionComponentCounts,
    TransactionId, TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
    TransactionVersion, TransparentAddressScriptHash, TransparentInputFact, TransparentOutPoint,
    TransparentOutputFact, UnixTimestampMillis, ValidatedCanonicalBlockReplay,
    encode_canonical_block_replay,
};
use zinder_store::{
    CanonicalBaselinePublication, CanonicalBuildBlock, CanonicalEventFence,
    CanonicalEventHistoryRequest, CanonicalLiveAppend, CanonicalReorgPolicy,
    CanonicalRetainedEvent, CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload,
    RawBlobRetention, RocksDbCanonicalBuilder, RocksDbCanonicalSecondary, RocksDbResourceBudget,
};
use zinder_wallet_projection::{
    WalletCanonicalSourceIdentity, WalletProjectionFamilyRowCounts, WalletProjectionReadyEvidence,
};
use zinder_wallet_rocksdb::{
    MAX_WALLET_PROJECTION_TRANSITION_LOGICAL_BYTES, RocksDbWalletBuildOptions,
    RocksDbWalletFollowingStore, RocksDbWalletStore, build_wallet_from_canonical,
};

const CASE_BLOCK_COUNTS: [u32; 3] = [256, 512, 1_024];
const MAX_BLOCK_COUNT: u32 = 1_024;
const SPENDS_PER_BLOCK: u32 = 16;
const PREDECESSOR_OUTPUT_COUNT: u32 = MAX_BLOCK_COUNT * SPENDS_PER_BLOCK;
const SUPPORTED_REORG_DEPTH: u32 = 100;
const OUTPUT_VALUE_ZAT: u64 = 10;
const FIXTURE_FORMAT_VERSION: u16 = 1;
const REPORT_FORMAT_VERSION: u16 = 1;
const REPORT_PATH_ENV: &str = "ZINDER_WALLET_TRANSITION_REPORT";
const SOFTWARE_REVISION_ENV: &str = "ZINDER_WALLET_TRANSITION_SOFTWARE_REVISION";

const CANONICAL_WRITER_BUDGET: RocksDbResourceBudget =
    RocksDbResourceBudget::canonical_writer_defaults();
const CANONICAL_READER_BUDGET: RocksDbResourceBudget =
    RocksDbResourceBudget::canonical_reader_defaults();
const WALLET_WRITER_BUDGET: RocksDbResourceBudget =
    RocksDbResourceBudget::materialized_view_writer_defaults();

#[derive(Serialize)]
struct WalletTransitionReport {
    report_format_version: u16,
    software_revision: String,
    fixture_identity_sha256: String,
    fixture: FixtureSummary,
    resource_envelope: ResourceEnvelope,
    cases: Vec<WalletTransitionCase>,
}

#[derive(Serialize)]
struct FixtureSummary {
    fixture_format_version: u16,
    network: &'static str,
    baseline_height: u32,
    maximum_replay_block_count: u32,
    spends_per_block: u32,
    predecessor_output_count: u32,
    output_value_zat: u64,
    supported_reorg_depth: u32,
    replay_materialized_before_timing: bool,
    timed_public_method: &'static str,
}

#[derive(Serialize)]
struct ResourceEnvelope {
    build_mode: &'static str,
    rocksdb_perf_level: &'static str,
    canonical_writer: ResourceBudgetSummary,
    canonical_reader: ResourceBudgetSummary,
    wallet_writer: ResourceBudgetSummary,
}

#[derive(Serialize)]
struct ResourceBudgetSummary {
    block_cache_bytes: u64,
    max_wal_bytes: u64,
    max_open_files: i32,
    write_buffer_bytes: u64,
    max_write_buffer_count: i32,
    max_background_jobs: i32,
    memtable_budget_bytes: u64,
    statistics_level: &'static str,
}

#[derive(Serialize)]
struct WalletTransitionCase {
    replay_block_count: u32,
    transparent_spend_count: u32,
    public_transition_call_count: u32,
    elapsed_seconds: f64,
    blocks_per_second: f64,
    spends_per_second: f64,
    rocksdb_perf_context: RocksDbPerfContextSummary,
    target_fence: FenceSummary,
    ready_evidence: ReadyEvidenceSummary,
}

#[derive(Serialize)]
struct RocksDbPerfContextSummary {
    point_read_count: Option<u64>,
    get_read_bytes: u64,
    multiget_read_bytes: u64,
    block_read_count: u64,
    block_cache_hit_count: u64,
    get_from_memtable_count: u64,
    user_key_comparison_count: u64,
}

#[derive(Serialize)]
struct FenceSummary {
    chain_epoch_id: u64,
    chain_event_sequence: u64,
    visible_tip_height: u32,
    visible_tip_hash_hex: String,
    sequence_block_count: u64,
    sequence_digest_sha256: String,
    settled_tip_height: u32,
    settled_tip_hash_hex: String,
}

#[derive(Serialize)]
struct ReadyEvidenceSummary {
    wallet_projection_digest_hex: String,
    row_counts: RowCountSummary,
    utxo_count: u64,
    utxo_total_value_zat: u64,
    utxo_commitment_display_digest_hex: String,
}

#[derive(Serialize)]
#[allow(
    clippy::struct_field_names,
    reason = "report fields intentionally preserve the production READY row-count vocabulary"
)]
struct RowCountSummary {
    transparent_unspent_output_count: u64,
    transparent_unspent_output_by_address_count: u64,
    transparent_spent_output_count: u64,
    transparent_address_transaction_count: u64,
    transparent_address_balance_count: u64,
    reorg_undo_count: u64,
}

struct PreparedFixture {
    _temporary: TempDir,
    initial_source: WalletCanonicalSourceIdentity,
    retained_events: Vec<CanonicalRetainedEvent>,
    replay_rows: Vec<ValidatedCanonicalBlockReplay>,
    block_ids: Vec<BlockId>,
    wallet_checkpoints: Vec<std::path::PathBuf>,
}

#[test]
#[ignore = "manual matched release benchmark on an isolated host"]
#[allow(
    clippy::too_many_lines,
    reason = "the benchmark keeps fixture preparation, the timed public call, and its evidence checks in one visible contract"
)]
fn spend_dense_reconciliation_reports_matched_public_boundary_evidence() -> Result<()> {
    let fixture = prepare_fixture()?;
    let mut cases = Vec::with_capacity(CASE_BLOCK_COUNTS.len());

    for (case_index, replay_block_count) in CASE_BLOCK_COUNTS.into_iter().enumerate() {
        let replay_count = usize::try_from(replay_block_count)?;
        let target_event = *fixture
            .retained_events
            .get(
                replay_count
                    .checked_sub(1)
                    .ok_or_else(|| eyre!("case must be non-empty"))?,
            )
            .ok_or_else(|| eyre!("target retained event is absent"))?;
        let target_fence = target_event.resulting_fence();
        let settled_height = settled_height_for_target(target_fence.visible_tip().height);
        let target_settled_tip = *fixture
            .block_ids
            .get(usize::try_from(settled_height.value().saturating_sub(1))?)
            .ok_or_else(|| eyre!("target settled block is absent"))?;
        let replay_range =
            BlockHeightRange::inclusive(BlockHeight::new(2), target_fence.visible_tip().height);
        let retained_events = fixture.retained_events[..replay_count].to_vec();
        let replay_rows = fixture.replay_rows[..replay_count]
            .iter()
            .cloned()
            .map(Ok::<_, CanonicalStoreError>)
            .collect::<Vec<_>>();
        let wallet_path = fixture
            .wallet_checkpoints
            .get(case_index)
            .ok_or_else(|| eyre!("wallet checkpoint is absent"))?;
        let mut wallet = RocksDbWalletStore::open_ready_for_following(
            wallet_path,
            Network::ZcashRegtest,
            WALLET_WRITER_BUDGET,
        )?;

        set_perf_stats(PerfStatsLevel::EnableCount);
        let mut perf_context = PerfContext::default();
        perf_context.reset();
        let started = Instant::now();
        let transition = wallet.reconcile_canonical_event_sequence(
            fixture.initial_source,
            &retained_events,
            target_fence,
            target_settled_tip,
            None,
            replay_range,
            transition_logical_byte_limit()?,
            replay_rows,
        );
        let elapsed = started.elapsed();
        let perf_summary = perf_context_summary(&perf_context);
        set_perf_stats(PerfStatsLevel::Disable);
        transition?;

        let ready_evidence = wallet.ready_evidence().clone();
        assert_ready_evidence(
            &ready_evidence,
            target_event,
            target_fence,
            target_settled_tip,
            replay_block_count,
        )?;
        assert_sample_rows(wallet, target_fence, target_settled_tip, replay_block_count)?;

        let elapsed_seconds = elapsed.as_secs_f64();
        ensure!(
            elapsed_seconds > 0.0,
            "timed transition duration must be positive"
        );
        let spend_count = replay_block_count
            .checked_mul(SPENDS_PER_BLOCK)
            .ok_or_else(|| eyre!("spend count overflow"))?;
        cases.push(WalletTransitionCase {
            replay_block_count,
            transparent_spend_count: spend_count,
            public_transition_call_count: 1,
            elapsed_seconds,
            blocks_per_second: f64::from(replay_block_count) / elapsed_seconds,
            spends_per_second: f64::from(spend_count) / elapsed_seconds,
            rocksdb_perf_context: perf_summary,
            target_fence: fence_summary(target_fence, target_settled_tip),
            ready_evidence: ready_evidence_summary(&ready_evidence),
        });
    }

    let report = WalletTransitionReport {
        report_format_version: REPORT_FORMAT_VERSION,
        software_revision: env::var(SOFTWARE_REVISION_ENV)
            .unwrap_or_else(|_| "unreported".to_owned()),
        fixture_identity_sha256: fixture_identity_sha256(),
        fixture: FixtureSummary {
            fixture_format_version: FIXTURE_FORMAT_VERSION,
            network: "zcash-regtest",
            baseline_height: 1,
            maximum_replay_block_count: MAX_BLOCK_COUNT,
            spends_per_block: SPENDS_PER_BLOCK,
            predecessor_output_count: PREDECESSOR_OUTPUT_COUNT,
            output_value_zat: OUTPUT_VALUE_ZAT,
            supported_reorg_depth: SUPPORTED_REORG_DEPTH,
            replay_materialized_before_timing: true,
            timed_public_method: "RocksDbWalletFollowingStore::reconcile_canonical_event_sequence",
        },
        resource_envelope: ResourceEnvelope {
            build_mode: "release",
            rocksdb_perf_level: "enable-count",
            canonical_writer: resource_budget_summary(CANONICAL_WRITER_BUDGET),
            canonical_reader: resource_budget_summary(CANONICAL_READER_BUDGET),
            wallet_writer: resource_budget_summary(WALLET_WRITER_BUDGET),
        },
        cases,
    };
    let encoded = serde_json::to_vec_pretty(&report)?;
    if let Some(report_path) = env::var_os(REPORT_PATH_ENV) {
        fs::write(&report_path, &encoded)
            .wrap_err_with(|| format!("failed to write {}", Path::new(&report_path).display()))?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "fixture construction keeps the exact canonical and wallet fence preparation explicit"
)]
fn prepare_fixture() -> Result<PreparedFixture> {
    let temporary = TempDir::new()?;
    let activations = inactive_upgrade_activations()?;
    let canonical_path = temporary.path().join("canonical");
    let wallet_path = temporary.path().join("wallet-baseline");
    let baseline_facts = baseline_block_facts();
    let baseline_tip = block_id(&baseline_facts);
    let build_plan = CanonicalStoreBuildPlan::complete(
        &activations,
        0,
        baseline_tip,
        RawBlobRetention::Transactions,
        CanonicalReorgPolicy::new(SUPPORTED_REORG_DEPTH)?,
    )?;
    let mut builder = RocksDbCanonicalBuilder::create_fresh(
        &canonical_path,
        CanonicalStoreWorkload::Wallet,
        build_plan,
        CANONICAL_WRITER_BUDGET,
    )?;
    builder.bulk_load_blocks([Ok::<_, std::io::Error>(canonical_build_block(
        baseline_facts,
    ))])?;
    builder.load_subtree_roots(std::iter::empty())?;
    builder.confirm_source_tip_checkpoint(&CommitmentTreeCheckpoint::new(
        baseline_tip,
        1,
        CommitmentTreeFrontiers::default(),
    ))?;
    let validated = builder.prepare_cold_certified_publication()?;
    let publication = validated.prepare_baseline(CanonicalBaselinePublication::new(
        baseline_tip,
        UnixTimestampMillis::new(1_800_000_000_000),
    ))?;
    let mut canonical_store = validated.publish_baseline(publication)?;
    let initial_fence = canonical_store.event_fence();

    let wallet_outcome = build_wallet_from_canonical(
        &canonical_store,
        &wallet_path,
        RocksDbWalletBuildOptions {
            resource_budget: WALLET_WRITER_BUDGET,
            supported_reorg_depth: SUPPORTED_REORG_DEPTH,
            ..RocksDbWalletBuildOptions::for_local_tests()
        },
    )?;
    let initial_source = wallet_outcome.report.canonical_source_identity();
    ensure!(
        initial_source.source_position().tip == initial_fence.visible_tip(),
        "starting wallet does not exactly match the baseline canonical fence"
    );
    drop(wallet_outcome.store);

    let mut block_ids = Vec::with_capacity(usize::try_from(MAX_BLOCK_COUNT)? + 1);
    block_ids.push(baseline_tip);
    for replay_offset in 0..MAX_BLOCK_COUNT {
        let height = replay_offset
            .checked_add(2)
            .ok_or_else(|| eyre!("fixture height overflow"))?;
        let parent = *block_ids
            .last()
            .ok_or_else(|| eyre!("fixture parent block is absent"))?;
        let facts = spend_block_facts(height, parent.hash, replay_offset);
        let tip = block_id(&facts);
        let settled_height = settled_height_for_target(tip.height);
        let settled_tip = *block_ids
            .get(usize::try_from(settled_height.value().saturating_sub(1))?)
            .ok_or_else(|| eyre!("fixture settled block is absent"))?;
        let expected_fence = canonical_store.event_fence();
        let (next_store, _) = canonical_store.commit_live_append(
            CanonicalLiveAppend::new(
                expected_fence,
                canonical_build_block(facts),
                Vec::new(),
                settled_tip,
                UnixTimestampMillis::new(1_800_000_000_000 + u64::from(height)),
            ),
            &activations,
        )?;
        canonical_store = next_store;
        block_ids.push(tip);
    }

    let mut baseline_wallet = RocksDbWalletStore::open_ready_for_following(
        &wallet_path,
        Network::ZcashRegtest,
        WALLET_WRITER_BUDGET,
    )?;
    let mut wallet_checkpoints = Vec::with_capacity(CASE_BLOCK_COUNTS.len());
    for block_count in CASE_BLOCK_COUNTS {
        let checkpoint_path = temporary.path().join(format!("wallet-case-{block_count}"));
        let checkpoint =
            baseline_wallet.create_owner_checkpoint(&checkpoint_path, WALLET_WRITER_BUDGET)?;
        ensure!(
            checkpoint.ready_evidence == *baseline_wallet.ready_evidence(),
            "wallet checkpoint READY evidence changed"
        );
        wallet_checkpoints.push(checkpoint_path);
    }
    drop(baseline_wallet);
    drop(canonical_store);

    let secondary = RocksDbCanonicalSecondary::open_ready(
        &canonical_path,
        temporary.path().join("canonical-secondary"),
        &activations,
        CanonicalStoreWorkload::Wallet,
        RawBlobRetention::Transactions,
        CanonicalReorgPolicy::new(SUPPORTED_REORG_DEPTH)?,
        CANONICAL_READER_BUDGET,
    )?;
    let cursor = initial_source.source_position().event_cursor.as_bytes();
    let retained_events = secondary.canonical_event_history(CanonicalEventHistoryRequest::new(
        Some(&cursor),
        NonZeroU32::new(MAX_BLOCK_COUNT)
            .ok_or_else(|| eyre!("maximum block count must be non-zero"))?,
    ))?;
    ensure!(
        retained_events.len() == usize::try_from(MAX_BLOCK_COUNT)?,
        "fixture retained-event count differs from the replay block count"
    );
    let replay_range =
        BlockHeightRange::inclusive(BlockHeight::new(2), BlockHeight::new(MAX_BLOCK_COUNT + 1));
    let replay_rows = secondary
        .scan_canonical_replay_range(replay_range)?
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        replay_rows.len() == usize::try_from(MAX_BLOCK_COUNT)?,
        "fixture replay-row count differs from the replay block count"
    );

    Ok(PreparedFixture {
        _temporary: temporary,
        initial_source,
        retained_events,
        replay_rows,
        block_ids,
        wallet_checkpoints,
    })
}

fn assert_ready_evidence(
    evidence: &WalletProjectionReadyEvidence,
    target_event: CanonicalRetainedEvent,
    target_fence: CanonicalEventFence,
    target_settled_tip: BlockId,
    replay_block_count: u32,
) -> Result<()> {
    ensure!(
        evidence.source_position.chain_epoch_id == target_fence.chain_epoch_id(),
        "wallet epoch differs from the target canonical fence"
    );
    ensure!(
        evidence.source_position.tip == target_fence.visible_tip(),
        "wallet tip differs from the target canonical fence"
    );
    ensure!(
        evidence.source_position.event_sequence == target_fence.chain_event_sequence(),
        "wallet event sequence differs from the target canonical fence"
    );
    ensure!(
        evidence.source_position.event_cursor.as_bytes() == target_event.cursor().as_bytes(),
        "wallet event cursor differs from the target retained event"
    );
    ensure!(
        evidence.source_sequence_digest == target_fence.sequence_digest(),
        "wallet source digest differs from the target canonical fence"
    );
    ensure!(
        evidence.settled_tip == target_settled_tip,
        "wallet settled tip differs from the target canonical fence"
    );

    let spend_count = u64::from(replay_block_count) * u64::from(SPENDS_PER_BLOCK);
    let expected_rows = WalletProjectionFamilyRowCounts {
        transparent_unspent_output_count: u64::from(PREDECESSOR_OUTPUT_COUNT),
        transparent_unspent_output_by_address_count: u64::from(PREDECESSOR_OUTPUT_COUNT),
        transparent_spent_output_count: spend_count,
        transparent_address_transaction_count: u64::from(SPENDS_PER_BLOCK) + spend_count,
        transparent_address_balance_count: u64::from(SPENDS_PER_BLOCK),
        reorg_undo_count: u64::from(SUPPORTED_REORG_DEPTH),
    };
    ensure!(
        evidence.row_counts == expected_rows,
        "wallet row counts differ from the spend-dense fixture contract"
    );
    ensure!(
        evidence.utxo_summary.utxo_count == u64::from(PREDECESSOR_OUTPUT_COUNT),
        "wallet UTXO count changed after one-input/one-output spends"
    );
    ensure!(
        evidence.utxo_summary.total_value_zat
            == u64::from(PREDECESSOR_OUTPUT_COUNT) * OUTPUT_VALUE_ZAT,
        "wallet UTXO value changed after equal-value spends"
    );
    Ok(())
}

fn assert_sample_rows(
    wallet: RocksDbWalletFollowingStore,
    target_fence: CanonicalEventFence,
    target_settled_tip: BlockId,
    replay_block_count: u32,
) -> Result<()> {
    ensure!(
        wallet
            .find_reorg_undo(target_fence.visible_tip().height)?
            .is_some(),
        "wallet tip undo row is absent"
    );
    let expected_source =
        WalletCanonicalSourceIdentity::from_ready_evidence(wallet.ready_evidence());
    let wallet = wallet.into_ready_store(expected_source)?;
    let final_predecessor_index = replay_block_count
        .checked_mul(SPENDS_PER_BLOCK)
        .and_then(|count| count.checked_sub(1))
        .ok_or_else(|| eyre!("final predecessor index overflow"))?;
    for predecessor_index in [0, final_predecessor_index] {
        let predecessor = predecessor_outpoint(predecessor_index);
        ensure!(
            wallet.find_unspent_output(predecessor)?.is_none(),
            "spent predecessor remains in the unspent family"
        );
        ensure!(
            wallet.find_spent_output(predecessor)?.is_some(),
            "spent predecessor is absent from the spent family"
        );
    }
    for (height, lane) in [(2, 0), (replay_block_count + 1, SPENDS_PER_BLOCK - 1)] {
        let successor = TransparentOutPoint::new(transaction_id(height, lane), 0);
        ensure!(
            wallet.find_unspent_output(successor)?.is_some(),
            "successor output is absent from the unspent family"
        );
    }
    ensure!(
        wallet.ready_evidence().settled_tip == target_settled_tip,
        "serving admission changed the settled tip"
    );
    Ok(())
}

fn baseline_block_facts() -> CanonicalBlockFacts {
    let transaction_id = setup_transaction_id();
    let outputs = (0..PREDECESSOR_OUTPUT_COUNT)
        .map(|output_index| {
            let lane = output_index % SPENDS_PER_BLOCK;
            TransparentOutputFact::new(
                output_index,
                OUTPUT_VALUE_ZAT,
                script_pub_key(lane),
                address(lane),
            )
        })
        .collect();
    block_facts(
        1,
        Network::ZcashRegtest.genesis_hash(),
        deterministic_block_hash(1),
        vec![transaction_facts(
            transaction_id,
            true,
            vec![TransparentInputFact::new(
                0,
                TransparentOutPoint::COINBASE_SENTINEL,
            )],
            outputs,
        )],
    )
}

fn spend_block_facts(
    height: u32,
    parent_hash: BlockHash,
    replay_offset: u32,
) -> CanonicalBlockFacts {
    let transactions = (0..SPENDS_PER_BLOCK)
        .map(|lane| {
            let predecessor_index = replay_offset * SPENDS_PER_BLOCK + lane;
            transaction_facts(
                transaction_id(height, lane),
                false,
                vec![TransparentInputFact::new(
                    0,
                    predecessor_outpoint(predecessor_index),
                )],
                vec![TransparentOutputFact::new(
                    0,
                    OUTPUT_VALUE_ZAT,
                    script_pub_key(lane),
                    address(lane),
                )],
            )
        })
        .collect();
    block_facts(
        height,
        parent_hash,
        deterministic_block_hash(height),
        transactions,
    )
}

fn block_facts(
    height: u32,
    parent_hash: BlockHash,
    block_hash: BlockHash,
    transactions: Vec<CanonicalTransactionFacts>,
) -> CanonicalBlockFacts {
    CanonicalBlockFacts {
        block_header: BlockHeaderArtifact::new(
            BlockHeight::new(height),
            block_hash,
            parent_hash,
            [0; 32],
            [0; 32],
            i64::from(height),
            0,
            [0; 32],
            0,
            0,
        ),
        serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
            &block_hash.as_bytes(),
        ),
        transactions,
    }
}

fn transaction_facts(
    transaction_id: TransactionId,
    is_coinbase: bool,
    transparent_inputs: Vec<TransparentInputFact>,
    transparent_outputs: Vec<TransparentOutputFact>,
) -> CanonicalTransactionFacts {
    CanonicalTransactionFacts {
        public_facts: TransactionPublicFacts {
            transaction_id,
            auth_digest: None,
            wtxid: None,
            version: TransactionVersion::V4,
            consensus_branch_id: None,
            lock_time: LockTime::Unlocked,
            expiry_height: None,
            size_bytes: 32,
            counts: TransactionComponentCounts {
                transparent_input_count: u32::try_from(transparent_inputs.len())
                    .unwrap_or(u32::MAX),
                transparent_output_count: u32::try_from(transparent_outputs.len())
                    .unwrap_or(u32::MAX),
                ..TransactionComponentCounts::EMPTY
            },
            orchard_value_balance_zat: None,
            orchard_anchor: None,
            ironwood_value_balance_zat: None,
            privacy_shape: PrivacyShape::Unclassified,
            is_coinbase,
            unsupported_sections: Vec::new(),
        },
        serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
            &transaction_id.as_bytes(),
        ),
        intrinsic_value_balances: TransactionIntrinsicValueBalances::default(),
        transparent_inputs,
        transparent_outputs,
    }
}

fn canonical_build_block(facts: CanonicalBlockFacts) -> CanonicalBuildBlock {
    let height = facts.block_header.height;
    let block_hash = facts.block_header.block_hash;
    let parent_hash = facts.block_header.parent_hash;
    let compact_block = zinder_core::CompactBlockArtifact::empty(
        BlockId::new(height, block_hash),
        parent_hash,
        height.value(),
        CompactChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    );
    let transaction_blobs = facts
        .transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| {
            TransactionBlobArtifact::new(
                TransactionLocation::new(
                    transaction.public_facts.transaction_id,
                    height,
                    block_hash,
                    u32::try_from(index).unwrap_or(u32::MAX),
                ),
                transaction.public_facts.transaction_id.as_bytes(),
            )
        })
        .collect();
    let replay_envelope = encode_canonical_block_replay(
        &facts,
        CanonicalBlockReplayFormatVersion::V1,
        CanonicalBlockFactsDigestVersion::V1,
    );
    CanonicalBuildBlock {
        facts,
        replay_envelope,
        compact_block,
        tip_metadata: ChainTipMetadata::new(0, 0, 0),
        tree_state_checkpoint: Some(CommitmentTreeCheckpoint::new(
            BlockId::new(height, block_hash),
            height.value(),
            CommitmentTreeFrontiers::default(),
        )),
        block_final_note_commitment_roots: None,
        transaction_blobs,
        block_blob: None,
    }
}

fn inactive_upgrade_activations() -> Result<NetworkUpgradeActivations> {
    let activations = [
        "Overwinter",
        "Sapling",
        "Blossom",
        "Heartwood",
        "Canopy",
        "NU5",
        "NU6",
        "NU6.1",
        "NU6.2",
        "NU6.3",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| NetworkUpgradeActivation {
        branch_id: ConsensusBranchId::new(u32::try_from(index).unwrap_or(u32::MAX) + 1),
        activation_height: BlockHeight::new(10_000 + u32::try_from(index).unwrap_or(u32::MAX)),
        name: name.to_owned(),
    })
    .collect();
    Ok(NetworkUpgradeActivations::new(
        Network::ZcashRegtest,
        activations,
    )?)
}

fn predecessor_outpoint(output_index: u32) -> TransparentOutPoint {
    TransparentOutPoint::new(setup_transaction_id(), output_index)
}

fn setup_transaction_id() -> TransactionId {
    TransactionId::from_bytes(deterministic_bytes(b"setup-transaction", 0, 0))
}

fn transaction_id(height: u32, lane: u32) -> TransactionId {
    TransactionId::from_bytes(deterministic_bytes(b"spend-transaction", height, lane))
}

fn deterministic_block_hash(height: u32) -> BlockHash {
    BlockHash::from_bytes(deterministic_bytes(b"block", height, 0))
}

fn deterministic_bytes(domain: &[u8], first: u32, second: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"zinder:wallet-transition-discovery:v1\0");
    hasher.update(domain);
    hasher.update(first.to_be_bytes());
    hasher.update(second.to_be_bytes());
    hasher.finalize().into()
}

fn address(lane: u32) -> TransparentAddressScriptHash {
    let mut bytes = [0_u8; 32];
    bytes[0] = 0xa1;
    bytes[1] = u8::try_from(lane).unwrap_or(u8::MAX);
    TransparentAddressScriptHash::from_bytes(bytes)
}

fn script_pub_key(lane: u32) -> Vec<u8> {
    vec![0x51, u8::try_from(lane).unwrap_or(u8::MAX)]
}

fn block_id(facts: &CanonicalBlockFacts) -> BlockId {
    BlockId::new(facts.block_header.height, facts.block_header.block_hash)
}

fn settled_height_for_target(target_height: BlockHeight) -> BlockHeight {
    BlockHeight::new(
        target_height
            .value()
            .saturating_sub(SUPPORTED_REORG_DEPTH)
            .max(1),
    )
}

fn transition_logical_byte_limit() -> Result<NonZeroU64> {
    NonZeroU64::new(MAX_WALLET_PROJECTION_TRANSITION_LOGICAL_BYTES)
        .ok_or_else(|| eyre!("wallet transition logical-byte limit must be non-zero"))
}

fn perf_context_summary(context: &PerfContext) -> RocksDbPerfContextSummary {
    RocksDbPerfContextSummary {
        point_read_count: None,
        get_read_bytes: context.metric(PerfMetric::GetReadBytes),
        multiget_read_bytes: context.metric(PerfMetric::MultigetReadBytes),
        block_read_count: context.metric(PerfMetric::BlockReadCount),
        block_cache_hit_count: context.metric(PerfMetric::BlockCacheHitCount),
        get_from_memtable_count: context.metric(PerfMetric::GetFromMemtableCount),
        user_key_comparison_count: context.metric(PerfMetric::UserKeyComparisonCount),
    }
}

fn fixture_identity_sha256() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"zinder:wallet-transition-discovery:fixture:v1\0");
    hasher.update(FIXTURE_FORMAT_VERSION.to_be_bytes());
    hasher.update(MAX_BLOCK_COUNT.to_be_bytes());
    hasher.update(SPENDS_PER_BLOCK.to_be_bytes());
    hasher.update(PREDECESSOR_OUTPUT_COUNT.to_be_bytes());
    hasher.update(OUTPUT_VALUE_ZAT.to_be_bytes());
    hasher.update(SUPPORTED_REORG_DEPTH.to_be_bytes());
    hex::encode(hasher.finalize())
}

fn fence_summary(fence: CanonicalEventFence, settled_tip: BlockId) -> FenceSummary {
    FenceSummary {
        chain_epoch_id: fence.chain_epoch_id().value(),
        chain_event_sequence: fence.chain_event_sequence(),
        visible_tip_height: fence.visible_tip().height.value(),
        visible_tip_hash_hex: hex::encode(fence.visible_tip().hash.as_bytes()),
        sequence_block_count: fence.sequence_digest().block_count(),
        sequence_digest_sha256: hex::encode(fence.sequence_digest().as_bytes()),
        settled_tip_height: settled_tip.height.value(),
        settled_tip_hash_hex: hex::encode(settled_tip.hash.as_bytes()),
    }
}

fn ready_evidence_summary(evidence: &WalletProjectionReadyEvidence) -> ReadyEvidenceSummary {
    ReadyEvidenceSummary {
        wallet_projection_digest_hex: hex::encode(evidence.projection_digest.as_bytes()),
        row_counts: row_count_summary(evidence.row_counts),
        utxo_count: evidence.utxo_summary.utxo_count,
        utxo_total_value_zat: evidence.utxo_summary.total_value_zat,
        utxo_commitment_display_digest_hex: hex::encode(
            evidence.utxo_summary.commitment.display_digest(),
        ),
    }
}

const fn row_count_summary(counts: WalletProjectionFamilyRowCounts) -> RowCountSummary {
    RowCountSummary {
        transparent_unspent_output_count: counts.transparent_unspent_output_count,
        transparent_unspent_output_by_address_count: counts
            .transparent_unspent_output_by_address_count,
        transparent_spent_output_count: counts.transparent_spent_output_count,
        transparent_address_transaction_count: counts.transparent_address_transaction_count,
        transparent_address_balance_count: counts.transparent_address_balance_count,
        reorg_undo_count: counts.reorg_undo_count,
    }
}

const fn resource_budget_summary(budget: RocksDbResourceBudget) -> ResourceBudgetSummary {
    ResourceBudgetSummary {
        block_cache_bytes: budget.block_cache_bytes,
        max_wal_bytes: budget.max_wal_bytes,
        max_open_files: budget.max_open_files,
        write_buffer_bytes: budget.write_buffer_bytes,
        max_write_buffer_count: budget.max_write_buffer_count,
        max_background_jobs: budget.max_background_jobs,
        memtable_budget_bytes: budget.memtable_budget_bytes,
        statistics_level: budget.statistics_level.as_str(),
    }
}
