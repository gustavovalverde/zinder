//! Live federation tests for [`ExplorerQuery::FeeSummary`].
//!
//! The handler reads typed block-summary facts from the materialized-view store and
//! aggregates per-transaction ZIP-317 conventional fee floors via
//! `zinder_core::TransactionComponentCounts::zip317_conventional_fee_zat`.
//! The test exercises the full pipeline against a real upstream node:
//! bulk catch up a window, ask the explorer for a fee summary over the
//! window, and assert the freshness envelope plus the structural
//! invariants the wire shape promises (`block_count`,
//! `transaction_count`, min/max bounds, total ≥ count × floor).

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use eyre::{Result, eyre};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tonic::Request;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, Network};
use zinder_explorer::{
    ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings, MaterializedViewStore,
    MaterializedViewStoreOptions,
};
use zinder_ingest::{
    MaterializedViewReplayConfig, MaterializedViewReplayPolicy,
    catch_up_materialized_view_store_to_canonical,
    open_primary_materialized_view_store_for_canonical,
};
use zinder_proto::capabilities::EXPLORER_FEE_SUMMARY_V1;
use zinder_proto::v1::explorer::{
    FeeSummaryRequest, FeeSummaryResponse,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_query::WalletQuery;
use zinder_store::PrimaryChainStore;
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{WalletQueryServerOptions, bulk_catchup_store, serve_wallet_query_grpc};

/// ZIP-317 conventional-fee minimum: `MARGINAL_FEE × GRACE_ACTIONS`.
/// Every non-coinbase transaction's `zip317_conventional_fee_zat` is at
/// least this floor.
const MIN_ZIP317_FLOOR_ZAT: u64 = 5_000 * 2;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fee_summary_aggregates_zip317_floors_across_window() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = FeeSummaryFixture::open(&env).await?;
    let tip = fixture.sample_block_height.value();
    let start = tip.saturating_sub(9);
    let response = fixture.fee_summary(start, tip).await?;
    assert_fee_summary_shape(&response, tip - start + 1)?;

    tracing::info!(
        target: "zinder::live",
        event = "fee_summary_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        block_count = response.block_count,
        transaction_count = response.transaction_count,
        total_zat = response.total_zip317_conventional_fee_zat,
        "explorer fee summary validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fee_summary_rejects_inverted_and_oversized_ranges() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = FeeSummaryFixture::open(&env).await?;
    let tip = fixture.sample_block_height.value();
    let inverted = fixture.fee_summary(tip, tip.saturating_sub(1)).await;
    assert!(
        matches!(inverted, Err(status) if status.code() == tonic::Code::InvalidArgument),
        "inverted range must return InvalidArgument",
    );
    let oversized = fixture.fee_summary(0, 1024).await;
    assert!(
        matches!(oversized, Err(status) if status.code() == tonic::Code::InvalidArgument),
        "range > 256 blocks must return InvalidArgument",
    );
    fixture.shutdown().await;
    Ok(())
}

struct FeeSummaryFixture {
    network: Network,
    sample_block_height: BlockHeight,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
}

impl FeeSummaryFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, tip_height) = bulk_catchup_store(env).await?;
        let storage_path = store_tempdir.path().join("zinder-store");
        catch_up_materialized_views(&store, &storage_path).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
        let (wallet_grpc_addr, wallet_server_handle) = serve_wallet_query_grpc(
            wallet_query,
            WalletQueryServerOptions {
                network: Some(network),
                ingest_control_endpoint: None,
            },
        )
        .await?;
        let wallet_endpoint = format!("http://{wallet_grpc_addr}");
        let materialized_view_store = MaterializedViewStore::open_secondary(
            MaterializedViewStore::path_for_canonical(&storage_path),
            store_tempdir
                .path()
                .join("zinder-materialized-views-secondary-explorer"),
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: MaterializedViewStore::bundled_consumers(),
                rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        materialized_view_store.try_catch_up()?;

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_materialized_view_store(materialized_view_store)
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            sample_block_height: tip_height,
            explorer_adapter,
            wallet_server_handle,
            _store_tempdir: store_tempdir,
        })
    }

    async fn fee_summary(
        &self,
        start_height: u32,
        end_height: u32,
    ) -> std::result::Result<FeeSummaryResponse, tonic::Status> {
        let response = ExplorerQueryService::fee_summary(
            &self.explorer_adapter,
            Request::new(FeeSummaryRequest {
                start_height,
                end_height,
            }),
        )
        .await?;
        Ok(response.into_inner())
    }

    async fn shutdown(&mut self) {
        self.wallet_server_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
    }
}

fn assert_fee_summary_shape(response: &FeeSummaryResponse, requested_blocks: u32) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("FeeSummary response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_FEE_SUMMARY_V1);
    assert!(
        freshness
            .chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
            .is_some(),
        "fee summary freshness must carry a chain epoch",
    );
    assert!(
        response.block_count <= requested_blocks,
        "block_count {} cannot exceed requested span {requested_blocks}",
        response.block_count,
    );
    if response.transaction_count == 0 {
        assert_eq!(response.total_zip317_conventional_fee_zat, 0);
        assert_eq!(response.min_zip317_conventional_fee_zat, 0);
        assert_eq!(response.max_zip317_conventional_fee_zat, 0);
        return Ok(());
    }
    assert!(
        response.min_zip317_conventional_fee_zat >= MIN_ZIP317_FLOOR_ZAT,
        "ZIP-317 fee floor is {MIN_ZIP317_FLOOR_ZAT} zat; min was {}",
        response.min_zip317_conventional_fee_zat,
    );
    assert!(
        response.max_zip317_conventional_fee_zat >= response.min_zip317_conventional_fee_zat,
        "max {} must be >= min {}",
        response.max_zip317_conventional_fee_zat,
        response.min_zip317_conventional_fee_zat,
    );
    let count_u64 = u64::from(response.transaction_count);
    assert!(
        response.total_zip317_conventional_fee_zat
            >= count_u64.saturating_mul(response.min_zip317_conventional_fee_zat),
        "total {} must be at least count × min ({} × {})",
        response.total_zip317_conventional_fee_zat,
        count_u64,
        response.min_zip317_conventional_fee_zat,
    );
    assert!(
        response.total_zip317_conventional_fee_zat
            <= count_u64.saturating_mul(response.max_zip317_conventional_fee_zat),
        "total {} must be at most count × max ({} × {})",
        response.total_zip317_conventional_fee_zat,
        count_u64,
        response.max_zip317_conventional_fee_zat,
    );
    Ok(())
}

async fn catch_up_materialized_views(store: &PrimaryChainStore, storage_path: &Path) -> Result<()> {
    let materialized_view_primary = open_primary_materialized_view_store_for_canonical(
        storage_path,
        zinder_store::RocksDbResourceBudget::for_local_tests(),
    )?;
    catch_up_materialized_view_store_to_canonical(
        store,
        &materialized_view_primary,
        materialized_view_replay_config()?,
    )
    .await?;
    drop(materialized_view_primary);
    Ok(())
}

/// Builds the one-shot materialized-view replay configuration used to populate the
/// materialized-view primary before the explorer attaches its secondary reader.
fn materialized_view_replay_config() -> Result<MaterializedViewReplayConfig> {
    Ok(MaterializedViewReplayConfig {
        replay_batch_blocks: NonZeroU32::new(500)
            .ok_or_else(|| eyre!("invalid materialized-view replay batch"))?,
        min_replay_batch_blocks: NonZeroU32::new(10)
            .ok_or_else(|| eyre!("invalid minimum materialized-view replay batch"))?,
        replay_policy: MaterializedViewReplayPolicy::Continuous,
        memory_budget_bytes: None,
        memory_degrade_ratio: 0.85,
        memory_pause_ratio: 0.95,
        memory_resume_ratio: 0.75,
        startup_handoff_lag_blocks: 1_000,
    })
}
