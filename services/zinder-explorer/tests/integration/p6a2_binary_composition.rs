//! P6a.2 process-composition contract coverage.

#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    env, fs,
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use eyre::{Result, WrapErr as _, eyre};
use parking_lot::RwLock;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    task::JoinHandle,
};
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{
    Code, Request, Response, Status,
    transport::{Channel, Endpoint, Server},
};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, Network, NetworkUpgradeActivations, PrivacyShape,
    TransactionComponentCounts, TransactionFactsArtifact, TransactionId, TransactionVersion,
    TransparentAddressScriptHash, TransparentInputFact, TransparentOutPoint, TransparentOutputFact,
    TransparentSpendFact, UnixTimestampMillis,
    wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name},
};
use zinder_ingest::{MaterializedViewReplayConfig, MaterializedViewTailer};
use zinder_materialized_views::{
    MaterializedViewPreset, MaterializedViewStore, MaterializedViewStoreOptions,
};
use zinder_proto::capabilities::{
    self, CapabilitySurface, EXPLORER_BLOCK_SUMMARY_V2, EXPLORER_CHAIN_REORG_HISTORY_V1,
    EXPLORER_FEE_SUMMARY_V1, EXPLORER_MEMPOOL_EVENT_COUNTS_V1, EXPLORER_MIGRATION_COHORTS_V1,
    EXPLORER_MIGRATION_DENOMINATIONS_V1, EXPLORER_MIGRATION_OVERVIEW_V1,
    EXPLORER_NETWORK_UPGRADE_STATUS_V1, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_FEES_V1,
    EXPLORER_TRANSACTION_RECENT_V1, capabilities_for_surface,
};
use zinder_proto::v1::{
    explorer::{self, explorer_query_client::ExplorerQueryClient},
    ingest::{
        AcquireCanonicalProjectionBuildLeaseRequest, CanonicalEventPageRequest,
        CanonicalEventPageResponse, CanonicalProjectionBuildLeaseResponse, CanonicalWriterFence,
        CanonicalWriterStatusRequest, CanonicalWriterStatusResponse,
        CreateCanonicalOwnerCheckpointRequest, CreateCanonicalOwnerCheckpointResponse,
        MempoolTransactionRequest, ReadmitCanonicalOwnerCheckpointRequest,
        ReleaseCanonicalProjectionBuildLeaseRequest, ReleaseCanonicalProjectionBuildLeaseResponse,
        RenewCanonicalProjectionBuildLeaseRequest, ServerInfoRequest, ServerInfoResponse,
        WriterPhase, WriterStatusRequest, WriterStatusResponse,
        canonical_control_server::{CanonicalControl, CanonicalControlServer},
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    ops,
    wallet::{
        self,
        wallet_query_server::{WalletQuery as WalletQueryService, WalletQueryServer},
    },
};
use zinder_proto::wire::{
    CanonicalConstructionManifestBindingFields, encode_canonical_construction_manifest_binding,
};
use zinder_proto::{
    CONTRACT_REVISION,
    capabilities::{
        INGEST_CONTROL_MEMPOOL_EVENTS_V2, INGEST_CONTROL_MEMPOOL_SNAPSHOT_V3,
        INGEST_CONTROL_MEMPOOL_TRANSACTION_V2, INGEST_CONTROL_SERVER_INFO_V1,
        INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1,
        INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPENDS_BY_OUTPOINT_V1, INGEST_CONTROL_WRITER_STATUS_V1,
    },
};
use zinder_store::{
    CanonicalEventFence, CanonicalEventHistoryRequest, CanonicalLiveAppend,
    CanonicalLiveReplacement, CanonicalReorgPolicy, CanonicalReplacementBlock,
    CanonicalStoreWorkload, ChainStoreOptions, RawBlobRetention, RocksDbCanonicalSecondary,
    RocksDbCanonicalStore, RocksDbResourceBudget,
};
use zinder_testkit::{
    ChainFixture, FixtureBlock, FixtureTransactionRows, WalletServingStoreFixture,
    canonical_build_block_for_wallet_serving_fixture, sample_regtest_upgrade_activations,
    synthetic_transaction_public_facts,
};
use zinder_wallet_projection::WalletCanonicalSourceIdentity;
use zinder_wallet_rocksdb::{MAX_WALLET_PROJECTION_TRANSITION_LOGICAL_BYTES, RocksDbWalletStore};

const P6A2_EXPLORER_CAPABILITIES: [&str; 11] = [
    EXPLORER_SERVER_INFO_V1,
    EXPLORER_BLOCK_SUMMARY_V2,
    EXPLORER_CHAIN_REORG_HISTORY_V1,
    EXPLORER_TRANSACTION_RECENT_V1,
    EXPLORER_TRANSACTION_FEES_V1,
    EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
    EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_NETWORK_UPGRADE_STATUS_V1,
    EXPLORER_MIGRATION_OVERVIEW_V1,
    EXPLORER_MIGRATION_COHORTS_V1,
    EXPLORER_MIGRATION_DENOMINATIONS_V1,
];

const P6A2_RPC_CAPABILITIES: [&str; 10] = [
    EXPLORER_SERVER_INFO_V1,
    EXPLORER_BLOCK_SUMMARY_V2,
    EXPLORER_CHAIN_REORG_HISTORY_V1,
    EXPLORER_TRANSACTION_RECENT_V1,
    EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
    EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_NETWORK_UPGRADE_STATUS_V1,
    EXPLORER_MIGRATION_OVERVIEW_V1,
    EXPLORER_MIGRATION_COHORTS_V1,
    EXPLORER_MIGRATION_DENOMINATIONS_V1,
];

const P6A2_FIELD_CAPABILITIES: [&str; 1] = [EXPLORER_TRANSACTION_FEES_V1];

const P6A2_OMITTED_CAPABILITY_ORDER: [&str; 33] = [
    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
    capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    capabilities::EXPLORER_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
    capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
    capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
    capabilities::EXPLORER_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
    capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
    capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
    capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
    capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
    capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
    capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
    capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
    capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
    capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
];

const P6A2_OMITTED_RPC_CAPABILITIES: [&str; 29] = [
    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
    capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    capabilities::EXPLORER_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
    capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
    capabilities::EXPLORER_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
    capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
    capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
    capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
    capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
    capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
    capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
    capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
];

const P6A2_OMITTED_FIELD_CAPABILITIES: [&str; 4] = [
    capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
    capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1,
    capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
];

const P6A2_OMITTED_FIELD_CARRIERS: [(&str, &[&str]); 4] = [
    (
        capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
        &[capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2],
    ),
    (
        capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
        &[capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1],
    ),
    (
        capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1,
        &[capabilities::EXPLORER_UTXO_SET_SUMMARY_V1],
    ),
    (
        capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
        &[
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
        ],
    ),
];

#[test]
fn p6a2_allocation_remains_exactly_eleven_of_forty_four() {
    assert_eq!(P6A2_EXPLORER_CAPABILITIES.len(), 11);
    assert_eq!(
        P6A2_RPC_CAPABILITIES.len() + P6A2_FIELD_CAPABILITIES.len(),
        11
    );
    assert_eq!(
        P6A2_OMITTED_RPC_CAPABILITIES.len() + P6A2_OMITTED_FIELD_CAPABILITIES.len(),
        33
    );
    let explorer_capabilities = capabilities_for_surface(CapabilitySurface::Explorer)
        .map(|capability| capability.string)
        .collect::<Vec<_>>();
    let omitted_capabilities = explorer_capabilities
        .iter()
        .copied()
        .filter(|capability| !P6A2_EXPLORER_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    assert_eq!(explorer_capabilities.len(), 44);
    assert_eq!(omitted_capabilities, P6A2_OMITTED_CAPABILITY_ORDER);
    assert_eq!(
        P6A2_OMITTED_CAPABILITY_ORDER
            .iter()
            .copied()
            .filter(|capability| P6A2_OMITTED_RPC_CAPABILITIES.contains(capability))
            .collect::<Vec<_>>(),
        P6A2_OMITTED_RPC_CAPABILITIES
    );
    assert_eq!(
        P6A2_OMITTED_CAPABILITY_ORDER
            .iter()
            .copied()
            .filter(|capability| P6A2_OMITTED_FIELD_CAPABILITIES.contains(capability))
            .collect::<Vec<_>>(),
        P6A2_OMITTED_FIELD_CAPABILITIES
    );
}

type ChainEventStream =
    Pin<Box<dyn Stream<Item = Result<wallet::ChainEventEnvelope, Status>> + Send>>;
type MempoolEventStream =
    Pin<Box<dyn Stream<Item = Result<wallet::MempoolEventEnvelope, Status>> + Send>>;

/// Test-local control plane that reports the exact admitted primary identity.
#[derive(Clone)]
struct ExactChainIngestControl {
    state: Arc<RwLock<ExactChainIngestControlState>>,
}

#[derive(Clone)]
struct ExactChainIngestControlState {
    chain_view: wallet::ChainView,
    canonical_writer_status: CanonicalWriterStatusResponse,
}

impl ExactChainIngestControl {
    fn new(
        chain_view: wallet::ChainView,
        canonical_writer_status: CanonicalWriterStatusResponse,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(ExactChainIngestControlState {
                chain_view,
                canonical_writer_status,
            })),
        }
    }

    fn state(&self) -> ExactChainIngestControlState {
        self.state.read().clone()
    }
}

#[tonic::async_trait]
impl IngestControl for ExactChainIngestControl {
    type VisibleChainEventsStream = ChainEventStream;
    type MempoolEventsStream = MempoolEventStream;

    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        Ok(Response::new(ServerInfoResponse {
            server_info: Some(ops::ServerInfo {
                network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
                service_name: "zinder-ingest".to_owned(),
                service_version: "p6a2-test".to_owned(),
                build_git_commit: "p6a2-test".to_owned(),
                capabilities: [
                    INGEST_CONTROL_SERVER_INFO_V1,
                    INGEST_CONTROL_WRITER_STATUS_V1,
                    INGEST_CONTROL_MEMPOOL_SNAPSHOT_V3,
                    INGEST_CONTROL_MEMPOOL_TRANSACTION_V2,
                    INGEST_CONTROL_MEMPOOL_EVENTS_V2,
                    INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1,
                    INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPENDS_BY_OUTPOINT_V1,
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                contract_revision: CONTRACT_REVISION,
                materialized_view_preset: String::new(),
                materialized_view_identities: Vec::new(),
            }),
        }))
    }

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        let chain_view = self.state().chain_view;
        Ok(Response::new(WriterStatusResponse {
            chain_view: Some(chain_view),
            network_name: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
            phase: WriterPhase::FollowingTip.into(),
            gap_blocks: Some(0),
            upstream_not_ready: None,
        }))
    }

    async fn visible_chain_events(
        &self,
        _request: Request<wallet::EventStreamStart>,
    ) -> Result<Response<Self::VisibleChainEventsStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::empty())))
    }

    async fn mempool_snapshot(
        &self,
        _request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        let chain_view = self.state().chain_view;
        let source_tip = chain_view
            .chain_epoch
            .as_ref()
            .and_then(|epoch| epoch.visible_tip.clone());
        Ok(Response::new(wallet::MempoolSnapshotResponse {
            chain_view: Some(chain_view),
            events_resume_cursor: Vec::new(),
            snapshot_age_millis: 0,
            entries: Vec::new(),
            next_cursor: Vec::new(),
            source_tip,
        }))
    }

    async fn mempool_transaction(
        &self,
        _request: Request<MempoolTransactionRequest>,
    ) -> Result<Response<wallet::TransactionStatusResponse>, Status> {
        Err(Status::not_found("P6a.2 fixture mempool is empty"))
    }

    async fn mempool_events(
        &self,
        _request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::pending())))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        let chain_view = self.state().chain_view;
        Ok(Response::new(
            wallet::TransparentMempoolOutputsByAddressResponse {
                chain_view: Some(chain_view),
                outputs: Vec::new(),
            },
        ))
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        let chain_view = self.state().chain_view;
        Ok(Response::new(
            wallet::TransparentMempoolSpendsByOutpointResponse {
                chain_view: Some(chain_view),
                spends: Vec::new(),
            },
        ))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture has no chained mempool outputs",
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture does not expose source value pools",
        ))
    }
}

#[tonic::async_trait]
impl CanonicalControl for ExactChainIngestControl {
    async fn writer_status(
        &self,
        _request: Request<CanonicalWriterStatusRequest>,
    ) -> Result<Response<CanonicalWriterStatusResponse>, Status> {
        Ok(Response::new(self.state().canonical_writer_status))
    }

    async fn event_page(
        &self,
        _request: Request<CanonicalEventPageRequest>,
    ) -> Result<Response<CanonicalEventPageResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }

    async fn create_owner_checkpoint(
        &self,
        _request: Request<CreateCanonicalOwnerCheckpointRequest>,
    ) -> Result<Response<CreateCanonicalOwnerCheckpointResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }

    async fn readmit_owner_checkpoint(
        &self,
        _request: Request<ReadmitCanonicalOwnerCheckpointRequest>,
    ) -> Result<Response<CreateCanonicalOwnerCheckpointResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }

    async fn acquire_projection_build_lease(
        &self,
        _request: Request<AcquireCanonicalProjectionBuildLeaseRequest>,
    ) -> Result<Response<CanonicalProjectionBuildLeaseResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }

    async fn renew_projection_build_lease(
        &self,
        _request: Request<RenewCanonicalProjectionBuildLeaseRequest>,
    ) -> Result<Response<CanonicalProjectionBuildLeaseResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }

    async fn release_projection_build_lease(
        &self,
        _request: Request<ReleaseCanonicalProjectionBuildLeaseRequest>,
    ) -> Result<Response<ReleaseCanonicalProjectionBuildLeaseResponse>, Status> {
        Err(Status::unimplemented(
            "P6a.2 fixture only serves WriterStatus",
        ))
    }
}

struct ExactChainIngestControlServer {
    endpoint: String,
    state: Arc<RwLock<ExactChainIngestControlState>>,
    cancel: CancellationToken,
    handle: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl ExactChainIngestControlServer {
    async fn spawn(service: ExactChainIngestControl) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::clone(&service.state);
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(IngestControlServer::new(service.clone()))
                .add_service(CanonicalControlServer::new(service))
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    server_cancel.cancelled_owned(),
                )
                .await
        });
        Ok(Self {
            endpoint: format!("http://{address}"),
            state,
            cancel,
            handle,
        })
    }

    fn update(
        &self,
        chain_view: wallet::ChainView,
        canonical_writer_status: CanonicalWriterStatusResponse,
    ) {
        *self.state.write() = ExactChainIngestControlState {
            chain_view,
            canonical_writer_status,
        };
    }

    async fn stop(self) -> Result<()> {
        self.cancel.cancel();
        self.handle.await??;
        Ok(())
    }
}

type P6a2WalletQueryStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;

macro_rules! p6a2_wallet_query_unimplemented {
    ($method:ident, $request:ty, $response:ty) => {
        fn $method<'life0, 'async_trait>(
            &'life0 self,
            _request: Request<$request>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Response<$response>, Status>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: Sync + 'async_trait,
        {
            Box::pin(async move {
                Err(Status::unimplemented(concat!(
                    "P6a.2 startup-admission WalletQuery fixture does not expose ",
                    stringify!($method),
                )))
            })
        }
    };
}

/// Test-local Wallet boundary limited to Explorer's startup admission calls.
#[derive(Clone)]
struct StartupAdmissionWalletQuery {
    server_info: wallet::ServerInfoResponse,
    network_upgrade_activations: wallet::NetworkUpgradeActivationsResponse,
}

#[tonic::async_trait]
impl WalletQueryService for StartupAdmissionWalletQuery {
    type CompactBlocksInRangeStream = P6a2WalletQueryStream<wallet::CompactBlocksInRangeChunk>;
    type FullBlocksInRangeStream = P6a2WalletQueryStream<wallet::FullBlocksInRangeChunk>;
    type ChainEventsStream = P6a2WalletQueryStream<wallet::ChainEventEnvelope>;
    type MempoolEventsStream = P6a2WalletQueryStream<wallet::MempoolEventEnvelope>;
    type TransparentAddressUnspentOutputsStream =
        P6a2WalletQueryStream<wallet::TransparentUnspentOutputsChunk>;
    type TransparentAddressTxIdsInRangeStream =
        P6a2WalletQueryStream<wallet::TransparentAddressTxIdsChunk>;

    p6a2_wallet_query_unimplemented!(
        visible_tip_block,
        wallet::VisibleTipBlockRequest,
        wallet::VisibleTipBlockResponse
    );
    p6a2_wallet_query_unimplemented!(
        settled_tip_block,
        wallet::SettledTipBlockRequest,
        wallet::SettledTipBlockResponse
    );
    p6a2_wallet_query_unimplemented!(
        block_id_by_selector,
        wallet::BlockSelectorRequest,
        wallet::BlockIdResponse
    );
    p6a2_wallet_query_unimplemented!(
        block_header_by_selector,
        wallet::BlockSelectorRequest,
        wallet::BlockHeaderResponse
    );
    p6a2_wallet_query_unimplemented!(
        compact_block,
        wallet::CompactBlockRequest,
        wallet::CompactBlockResponse
    );
    p6a2_wallet_query_unimplemented!(
        compact_blocks_in_range,
        wallet::CompactBlocksInRangeRequest,
        Self::CompactBlocksInRangeStream
    );
    p6a2_wallet_query_unimplemented!(
        full_block,
        wallet::FullBlockRequest,
        wallet::FullBlockResponse
    );
    p6a2_wallet_query_unimplemented!(
        full_blocks_in_range,
        wallet::FullBlocksInRangeRequest,
        Self::FullBlocksInRangeStream
    );
    p6a2_wallet_query_unimplemented!(
        transaction,
        wallet::TransactionRequest,
        wallet::TransactionStatusResponse
    );
    p6a2_wallet_query_unimplemented!(
        tree_state_at_height,
        wallet::TreeStateAtHeightRequest,
        wallet::TreeStateResponse
    );
    p6a2_wallet_query_unimplemented!(
        latest_tree_state_checkpoint,
        wallet::LatestTreeStateCheckpointRequest,
        wallet::TreeStateResponse
    );
    p6a2_wallet_query_unimplemented!(
        subtree_roots,
        wallet::SubtreeRootsRequest,
        wallet::SubtreeRootsResponse
    );
    p6a2_wallet_query_unimplemented!(
        broadcast_transaction,
        wallet::BroadcastTransactionRequest,
        wallet::BroadcastTransactionResponse
    );
    p6a2_wallet_query_unimplemented!(
        chain_events,
        wallet::ChainEventsRequest,
        Self::ChainEventsStream
    );
    p6a2_wallet_query_unimplemented!(
        mempool_snapshot,
        wallet::MempoolSnapshotRequest,
        wallet::MempoolSnapshotResponse
    );
    p6a2_wallet_query_unimplemented!(
        mempool_events,
        wallet::MempoolEventsRequest,
        Self::MempoolEventsStream
    );
    p6a2_wallet_query_unimplemented!(
        transparent_mempool_outputs_by_address,
        wallet::TransparentMempoolOutputsByAddressRequest,
        wallet::TransparentMempoolOutputsByAddressResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_mempool_spends_by_outpoint,
        wallet::TransparentMempoolSpendsByOutpointRequest,
        wallet::TransparentMempoolSpendsByOutpointResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_outputs_by_outpoint,
        wallet::TransparentOutputsByOutpointRequest,
        wallet::TransparentOutputsByOutpointResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_spends_by_outpoint,
        wallet::TransparentSpendsByOutpointRequest,
        wallet::TransparentSpendsByOutpointResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_unspent_outputs_by_outpoint,
        wallet::TransparentUnspentOutputsByOutpointRequest,
        wallet::TransparentUnspentOutputsByOutpointResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_mempool_outputs_by_outpoint,
        wallet::TransparentMempoolOutputsByOutpointRequest,
        wallet::TransparentOutputsByOutpointResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_address_unspent_outputs,
        wallet::TransparentAddressUnspentOutputsRequest,
        Self::TransparentAddressUnspentOutputsStream
    );
    p6a2_wallet_query_unimplemented!(
        transparent_address_tx_ids_in_range,
        wallet::TransparentAddressTxIdsInRangeRequest,
        Self::TransparentAddressTxIdsInRangeStream
    );
    p6a2_wallet_query_unimplemented!(
        transparent_address_balance,
        wallet::TransparentAddressBalanceRequest,
        wallet::TransparentAddressBalanceResponse
    );
    p6a2_wallet_query_unimplemented!(
        chain_value_pools_at_tip,
        wallet::ChainValuePoolsAtTipRequest,
        wallet::ChainValuePoolsAtTipResponse
    );
    p6a2_wallet_query_unimplemented!(
        transparent_utxo_set_summary,
        wallet::TransparentUtxoSetSummaryRequest,
        wallet::TransparentUtxoSetSummaryResponse
    );

    async fn server_info(
        &self,
        _request: Request<wallet::ServerInfoRequest>,
    ) -> Result<Response<wallet::ServerInfoResponse>, Status> {
        Ok(Response::new(self.server_info.clone()))
    }

    async fn network_upgrade_activations(
        &self,
        _request: Request<wallet::NetworkUpgradeActivationsRequest>,
    ) -> Result<Response<wallet::NetworkUpgradeActivationsResponse>, Status> {
        Ok(Response::new(self.network_upgrade_activations.clone()))
    }
}

struct StartupAdmissionWalletServer {
    endpoint: String,
    cancel: CancellationToken,
    handle: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl StartupAdmissionWalletServer {
    async fn spawn(service: StartupAdmissionWalletQuery) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(WalletQueryServer::new(service))
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    server_cancel.cancelled_owned(),
                )
                .await
        });
        let server = Self {
            endpoint: format!("http://{address}"),
            cancel,
            handle,
        };
        if let Err(error) = await_grpc_endpoint(address, "startup-admission WalletQuery").await {
            server.stop().await?;
            return Err(error);
        }
        Ok(server)
    }

    async fn stop(self) -> Result<()> {
        self.cancel.cancel();
        self.handle.await??;
        Ok(())
    }
}

struct QueryBinary {
    canonical_path: PathBuf,
    byte_len: u64,
}

fn required_query_binary() -> Result<QueryBinary> {
    let configured = env::var_os("ZINDER_TEST_QUERY_BINARY").ok_or_else(|| {
        eyre!(
            "P6a.2 real-query proof requires ZINDER_TEST_QUERY_BINARY to name an explicitly built zinder-query binary"
        )
    })?;
    let canonical_path = fs::canonicalize(&configured).wrap_err_with(|| {
        format!(
            "canonicalize explicit ZINDER_TEST_QUERY_BINARY path {}",
            PathBuf::from(&configured).display()
        )
    })?;
    let metadata = fs::metadata(&canonical_path)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(eyre!(
            "ZINDER_TEST_QUERY_BINARY must resolve to a nonempty file, got {}",
            canonical_path.display()
        ));
    }
    Ok(QueryBinary {
        canonical_path,
        byte_len: metadata.len(),
    })
}

struct ChildProcess {
    role: &'static str,
    child: Option<Child>,
}

impl ChildProcess {
    fn spawn(role: &'static str, command: &mut Command) -> Result<Self> {
        Ok(Self {
            role,
            child: Some(command.spawn().wrap_err_with(|| format!("spawn {role}"))?),
        })
    }

    async fn stop(&mut self) -> Result<String> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| eyre!("{} process was already collected", self.role))?;
        if child.try_wait()?.is_none() {
            child
                .start_kill()
                .wrap_err_with(|| format!("terminate {} process", self.role))?;
        }
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .map_err(|_| eyre!("{} process did not stop within five seconds", self.role))??;
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

struct P6a2BinaryRuntime {
    canonical_store: WalletServingStoreFixture,
    materialized_view_tailer: MaterializedViewTailer,
    wallet_follow_canonical: RocksDbCanonicalSecondary,
    construction_identity: zinder_store::CanonicalStoreConstructionIdentity,
    node: Option<OrderedNodeServer>,
    query_binary: QueryBinary,
    query: Option<ChildProcess>,
    explorer: ChildProcess,
    ingest_control: Option<ExactChainIngestControlServer>,
    query_addr: SocketAddr,
    explorer_addr: SocketAddr,
    ops_addr: SocketAddr,
}

impl P6a2BinaryRuntime {
    #[allow(
        clippy::too_many_lines,
        reason = "The real-process fixture keeps exact source identity, materialized replay, and child admission together."
    )]
    async fn spawn(chain: &ChainFixture) -> Result<Self> {
        let activations = p6a2_activations();
        let mut canonical_store =
            WalletServingStoreFixture::from_chain_after_live_append(chain, activations.as_ref())?;
        let canonical_primary_path = canonical_store.canonical_primary_path();
        let wallet_primary_path = canonical_store.wallet_primary_path();
        let fixture_root = canonical_primary_path
            .parent()
            .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?;
        let construction_identity = canonical_store.canonical_construction_identity()?;
        let (canonical_secondary, wallet_secondary) = canonical_store.take_readers()?;
        drop(wallet_secondary);

        let canonical_fence = canonical_secondary.event_fence();
        let chain_view = zinder_store::chain_view_message(canonical_secondary.chain_epoch()?);
        let materialized_view_primary_path =
            MaterializedViewStore::path_for_canonical(&canonical_primary_path);
        let materialized_view_primary = MaterializedViewStore::open_with_materialized_view_preset(
            &materialized_view_primary_path,
            construction_identity,
            MaterializedViewPreset::Explorer,
            MaterializedViewStoreOptions {
                sync_writes: false,
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..MaterializedViewStoreOptions::default()
            },
        )?;
        let materialized_view_tailer = MaterializedViewTailer {
            canonical: Arc::new(RwLock::new(canonical_secondary)),
            materialized_view_store: materialized_view_primary,
            config: MaterializedViewReplayConfig::DEFAULT,
            activations: Arc::clone(&activations),
            reorg_window_blocks: ChainStoreOptions::for_local_tests().reorg_window_blocks,
            chain_event_retention_window: Some(Duration::from_hours(168)),
            cursor_at_risk_warning: Duration::from_hours(24),
        };
        materialized_view_tailer.catch_up()?;

        let wallet_follow_canonical = RocksDbCanonicalSecondary::open_ready(
            &canonical_primary_path,
            fixture_root.join("p6a2-wallet-follow-canonical-secondary"),
            activations.as_ref(),
            CanonicalStoreWorkload::Wallet,
            RawBlobRetention::Transactions,
            p6a2_reorg_policy()?,
            RocksDbResourceBudget::for_local_tests(),
        )?;

        let ingest_control = ExactChainIngestControlServer::spawn(ExactChainIngestControl::new(
            chain_view,
            canonical_writer_status(canonical_fence, construction_identity),
        ))
        .await?;
        let node = p6a2_node_server().await?;
        let query_binary = required_query_binary()?;
        let query_addr = unused_loopback_addr()?;
        let query_ops_addr = unused_loopback_addr()?;
        let explorer_addr = unused_loopback_addr()?;
        let ops_addr = unused_loopback_addr()?;
        let query_canonical_secondary_root = fixture_root.join("p6a2-query-canonical-secondary");
        let query_wallet_secondary_root = wallet_primary_path
            .parent()
            .ok_or_else(|| eyre!("wallet fixture primary has no parent directory"))?
            .join("p6a2-query-wallet-secondary");
        let explorer_secondary_root = canonical_primary_path
            .parent()
            .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?
            .join("p6a2-explorer-secondary");
        fs::create_dir_all(&explorer_secondary_root).wrap_err_with(|| {
            format!(
                "create Explorer-owned materialized-view secondary root {}",
                explorer_secondary_root.display()
            )
        })?;

        let mut query_command = Command::new(&query_binary.canonical_path);
        query_command
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("--network")
            .arg("zcash-regtest")
            .arg("--canonical-primary-path")
            .arg(&canonical_primary_path)
            .arg("--canonical-secondary-root")
            .arg(&query_canonical_secondary_root)
            .arg("--raw-blob-policy")
            .arg("transactions")
            .arg("--wallet-primary-path")
            .arg(&wallet_primary_path)
            .arg("--wallet-secondary-root")
            .arg(&query_wallet_secondary_root)
            .arg("--ingest-control-addr")
            .arg(&ingest_control.endpoint)
            .arg("--listen-addr")
            .arg(query_addr.to_string())
            .arg("--ops-listen-addr")
            .arg(query_ops_addr.to_string())
            .arg("--reorg-window-blocks")
            .arg("100")
            .arg("--node-json-rpc-addr")
            .arg(node.url());
        let mut query = ChildProcess::spawn("zinder-query", &mut query_command)?;
        if let Err(error) = await_grpc_endpoint(query_addr, "zinder-query").await {
            let stderr = query
                .stop()
                .await
                .unwrap_or_else(|stop_error| stop_error.to_string());
            return Err(error).wrap_err(format!("zinder-query stderr:\n{stderr}"));
        }

        let mut explorer_command = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
        explorer_command
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("--network")
            .arg("zcash-regtest")
            .arg("--storage-path")
            .arg(&canonical_primary_path)
            .arg("--secondary-path")
            .arg(&explorer_secondary_root)
            .arg("--listen-addr")
            .arg(explorer_addr.to_string())
            .arg("--ops-listen-addr")
            .arg(ops_addr.to_string())
            .arg("--wallet-query-endpoint")
            .arg(format!("http://{query_addr}"));
        let mut explorer = ChildProcess::spawn("zinder-explorer", &mut explorer_command)?;
        if let Err(error) = await_grpc_endpoint(explorer_addr, "zinder-explorer").await {
            let explorer_stderr = explorer
                .stop()
                .await
                .unwrap_or_else(|stop_error| stop_error.to_string());
            let query_stderr = query
                .stop()
                .await
                .unwrap_or_else(|stop_error| stop_error.to_string());
            return Err(error).wrap_err(format!(
                "zinder-explorer stderr:\n{explorer_stderr}\nzinder-query stderr:\n{query_stderr}"
            ));
        }

        Ok(Self {
            canonical_store,
            materialized_view_tailer,
            wallet_follow_canonical,
            construction_identity,
            node: Some(node),
            query_binary,
            query: Some(query),
            explorer,
            ingest_control: Some(ingest_control),
            query_addr,
            explorer_addr,
            ops_addr,
        })
    }

    async fn explorer_client(&self) -> Result<ExplorerQueryClient<Channel>> {
        Ok(ExplorerQueryClient::new(
            await_grpc_endpoint(self.explorer_addr, "zinder-explorer").await?,
        ))
    }

    fn append_live_block(&mut self, chain: &ChainFixture) -> Result<CanonicalEventFence> {
        let tip = chain
            .blocks()
            .last()
            .ok_or_else(|| eyre!("P6a.2 append fixture omitted its live block"))?;
        let activations = p6a2_activations();
        let canonical_primary = self.open_canonical_primary()?;
        let expected_fence = canonical_primary.event_fence();
        let live_block = canonical_build_block_for_wallet_serving_fixture(
            chain,
            tip.height,
            activations.as_ref(),
        )?;
        let (canonical_primary, fence) = canonical_primary.commit_live_append(
            CanonicalLiveAppend::new(
                expected_fence,
                live_block,
                Vec::new(),
                expected_fence.visible_tip(),
                UnixTimestampMillis::new(u64::from(tip.block_time_seconds).saturating_mul(1_000)),
            ),
            activations.as_ref(),
        )?;
        drop(canonical_primary);

        self.apply_wallet_append(fence)?;
        self.materialized_view_tailer.catch_up()?;
        self.update_ingest_control()?;
        Ok(fence)
    }

    fn replace_live_suffix(&mut self, chain: &ChainFixture) -> Result<CanonicalEventFence> {
        let tip = chain
            .blocks()
            .last()
            .ok_or_else(|| eyre!("P6a.2 replacement fixture omitted its replacement block"))?;
        let activations = p6a2_activations();
        let canonical_primary = self.open_canonical_primary()?;
        let expected_fence = canonical_primary.event_fence();
        if tip.hash == expected_fence.visible_tip().hash {
            return Err(eyre!(
                "P6a.2 replacement fixture must use a distinct visible-tip hash"
            ));
        }
        let replacement_block = canonical_build_block_for_wallet_serving_fixture(
            chain,
            tip.height,
            activations.as_ref(),
        )?;
        let (canonical_primary, fence) = canonical_primary.commit_live_replacement(
            CanonicalLiveReplacement::new(
                expected_fence,
                vec![CanonicalReplacementBlock::new(
                    replacement_block,
                    Vec::new(),
                )],
                UnixTimestampMillis::new(u64::from(tip.block_time_seconds).saturating_mul(1_000)),
            ),
            activations.as_ref(),
        )?;
        drop(canonical_primary);

        self.reconcile_wallet_replacement(fence)?;
        self.materialized_view_tailer.catch_up()?;
        self.update_ingest_control()?;
        Ok(fence)
    }

    fn open_canonical_primary(&self) -> Result<RocksDbCanonicalStore> {
        let activations = p6a2_activations();
        Ok(RocksDbCanonicalStore::open_ready(
            self.canonical_store.canonical_primary_path(),
            activations.as_ref(),
            CanonicalStoreWorkload::Wallet,
            RawBlobRetention::Transactions,
            p6a2_reorg_policy()?,
            RocksDbResourceBudget::for_local_tests(),
        )?)
    }

    fn apply_wallet_append(&mut self, target_fence: CanonicalEventFence) -> Result<()> {
        self.wallet_follow_canonical.try_catch_up()?;
        let wallet_primary_path = self.canonical_store.wallet_primary_path();
        let mut wallet = RocksDbWalletStore::open_ready_for_following(
            wallet_primary_path,
            Network::ZcashRegtest,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let source = WalletCanonicalSourceIdentity::from_ready_evidence(wallet.ready_evidence());
        let source_cursor = source.source_position().event_cursor.as_bytes();
        let retained_events = self.wallet_follow_canonical.canonical_event_history(
            CanonicalEventHistoryRequest::new(Some(&source_cursor), NonZeroU32::MIN),
        )?;
        let event = exactly_one_retained_event(&retained_events, "append")?;
        if event.resulting_fence() != target_fence {
            return Err(eyre!(
                "P6a.2 append retained event does not end at the committed canonical fence"
            ));
        }
        let resulting_epoch = self
            .wallet_follow_canonical
            .chain_epoch_at(event.resulting_epoch_id())?;
        let resulting_settled_tip = BlockId::new(
            resulting_epoch.settled_tip_height,
            resulting_epoch.settled_tip_hash,
        );
        let replay_range = event.committed_range();
        let replay_rows = self
            .wallet_follow_canonical
            .scan_canonical_replay_range(replay_range)?;
        wallet.apply_canonical_event_range(
            source,
            event,
            target_fence,
            resulting_settled_tip,
            p6a2_wallet_transition_logical_bytes()?,
            replay_rows,
        )?;
        Ok(())
    }

    fn reconcile_wallet_replacement(&mut self, target_fence: CanonicalEventFence) -> Result<()> {
        self.wallet_follow_canonical.try_catch_up()?;
        let wallet_primary_path = self.canonical_store.wallet_primary_path();
        let mut wallet = RocksDbWalletStore::open_ready_for_following(
            wallet_primary_path,
            Network::ZcashRegtest,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let source = WalletCanonicalSourceIdentity::from_ready_evidence(wallet.ready_evidence());
        let source_cursor = source.source_position().event_cursor.as_bytes();
        let retained_events = self.wallet_follow_canonical.canonical_event_history(
            CanonicalEventHistoryRequest::new(Some(&source_cursor), NonZeroU32::MIN),
        )?;
        let reorg = exactly_one_retained_event(&retained_events, "replacement")?;
        if reorg.resulting_fence() != target_fence {
            return Err(eyre!(
                "P6a.2 replacement retained event does not end at the committed canonical fence"
            ));
        }
        let rollback_range = reorg
            .reverted_range()
            .ok_or_else(|| eyre!("P6a.2 replacement retained event omitted its reverted suffix"))?;
        let resulting_epoch = self
            .wallet_follow_canonical
            .chain_epoch_at(reorg.resulting_epoch_id())?;
        let resulting_settled_tip = BlockId::new(
            resulting_epoch.settled_tip_height,
            resulting_epoch.settled_tip_hash,
        );
        let replay_range = reorg.committed_range();
        let replay_rows = self
            .wallet_follow_canonical
            .scan_canonical_replay_range(replay_range)?;
        wallet.reconcile_canonical_event_sequence(
            source,
            &retained_events,
            target_fence,
            resulting_settled_tip,
            Some(rollback_range),
            replay_range,
            p6a2_wallet_transition_logical_bytes()?,
            replay_rows,
        )?;
        Ok(())
    }

    fn update_ingest_control(&self) -> Result<()> {
        let chain_view =
            zinder_store::chain_view_message(self.wallet_follow_canonical.chain_epoch()?);
        let ingest_control = self
            .ingest_control
            .as_ref()
            .ok_or_else(|| eyre!("P6a.2 IngestControl process is not running"))?;
        ingest_control.update(
            chain_view,
            canonical_writer_status(
                self.wallet_follow_canonical.event_fence(),
                self.construction_identity,
            ),
        );
        Ok(())
    }

    async fn stop_query(&mut self) -> Result<()> {
        let mut query = self
            .query
            .take()
            .ok_or_else(|| eyre!("P6a.2 Query process is not running"))?;
        let stderr = query.stop().await?;
        if !stderr.is_empty() {
            tracing::debug!(
                query_binary = %self.query_binary.canonical_path.display(),
                query_addr = %self.query_addr,
                "P6a.2 Query process stopped for lifecycle coverage"
            );
        }
        Ok(())
    }

    async fn restart_compatible_query(&mut self) -> Result<()> {
        if self.query.is_some() {
            return Err(eyre!("P6a.2 Query process is already running"));
        }
        let canonical_primary_path = self.canonical_store.canonical_primary_path();
        let wallet_primary_path = self.canonical_store.wallet_primary_path();
        let fixture_root = canonical_primary_path
            .parent()
            .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?;
        let query_canonical_secondary_root =
            fixture_root.join("p6a2-query-compatible-recovery-canonical-secondary");
        let query_wallet_secondary_root = canonical_primary_path
            .parent()
            .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?
            .join("p6a2-query-compatible-recovery-wallet-secondary");
        let ingest_control_endpoint = self
            .ingest_control
            .as_ref()
            .ok_or_else(|| eyre!("P6a.2 IngestControl process is not running"))?
            .endpoint
            .clone();
        let node_url = self
            .node
            .as_ref()
            .ok_or_else(|| eyre!("P6a.2 node fixture is not running"))?
            .url()
            .to_owned();

        let mut command = Command::new(&self.query_binary.canonical_path);
        command
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("--network")
            .arg("zcash-regtest")
            .arg("--canonical-primary-path")
            .arg(&canonical_primary_path)
            .arg("--canonical-secondary-root")
            .arg(&query_canonical_secondary_root)
            .arg("--raw-blob-policy")
            .arg("transactions")
            .arg("--wallet-primary-path")
            .arg(&wallet_primary_path)
            .arg("--wallet-secondary-root")
            .arg(&query_wallet_secondary_root)
            .arg("--ingest-control-addr")
            .arg(ingest_control_endpoint)
            .arg("--listen-addr")
            .arg(self.query_addr.to_string())
            .arg("--reorg-window-blocks")
            .arg("100")
            .arg("--node-json-rpc-addr")
            .arg(node_url);
        let mut query = ChildProcess::spawn("zinder-query compatible recovery", &mut command)?;
        if let Err(error) =
            await_grpc_endpoint(self.query_addr, "zinder-query compatible recovery").await
        {
            let stderr = query
                .stop()
                .await
                .unwrap_or_else(|stop_error| stop_error.to_string());
            return Err(error).wrap_err(format!(
                "zinder-query compatible recovery stderr:\n{stderr}"
            ));
        }
        self.query = Some(query);
        Ok(())
    }

    async fn stop(mut self) -> Result<()> {
        let explorer_stderr = self.explorer.stop().await?;
        let query_stderr = match self.query.as_mut() {
            Some(query) => query.stop().await?,
            None => String::new(),
        };
        if let Some(ingest_control) = self.ingest_control.take() {
            ingest_control.stop().await?;
        }
        if let Some(node) = self.node.take() {
            node.stop().await?;
        }
        if !explorer_stderr.is_empty() || !query_stderr.is_empty() {
            tracing::debug!(
                query_binary = %self.query_binary.canonical_path.display(),
                query_binary_bytes = self.query_binary.byte_len,
                query_addr = %self.query_addr,
                explorer_addr = %self.explorer_addr,
                ops_addr = %self.ops_addr,
                "P6a.2 child-process fixture stopped"
            );
        }
        Ok(())
    }
}

/// A real `WalletQuery` process with valid but different construction evidence.
struct IncompatibleQueryReplacement {
    _canonical_store: WalletServingStoreFixture,
    construction_identity: zinder_store::CanonicalStoreConstructionIdentity,
    ingest_control: Option<ExactChainIngestControlServer>,
    query: ChildProcess,
}

impl IncompatibleQueryReplacement {
    async fn spawn(
        query_binary: &QueryBinary,
        query_addr: SocketAddr,
        node_url: String,
    ) -> Result<Self> {
        let chain = p6a2_chain_fixture()?.with_raw_blob_retention(RawBlobRetention::All);
        let activations = p6a2_activations();
        let mut canonical_store =
            WalletServingStoreFixture::from_chain_after_live_append(&chain, activations.as_ref())?;
        let canonical_primary_path = canonical_store.canonical_primary_path();
        let wallet_primary_path = canonical_store.wallet_primary_path();
        let construction_identity = canonical_store.canonical_construction_identity()?;
        let (canonical_secondary, wallet_secondary) = canonical_store.take_readers()?;
        let canonical_fence = canonical_secondary.event_fence();
        let chain_view = zinder_store::chain_view_message(canonical_secondary.chain_epoch()?);
        drop(canonical_secondary);
        drop(wallet_secondary);
        let ingest_control = ExactChainIngestControlServer::spawn(ExactChainIngestControl::new(
            chain_view,
            canonical_writer_status(canonical_fence, construction_identity),
        ))
        .await?;
        let fixture_root = canonical_primary_path.parent().ok_or_else(|| {
            eyre!("incompatible canonical fixture primary has no parent directory")
        })?;
        let query_canonical_secondary_root =
            fixture_root.join("p6a2-incompatible-query-canonical-secondary");
        let query_wallet_secondary_root =
            fixture_root.join("p6a2-incompatible-query-wallet-secondary");
        let query_ops_addr = unused_loopback_addr()?;

        let mut command = Command::new(&query_binary.canonical_path);
        command
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("--network")
            .arg("zcash-regtest")
            .arg("--canonical-primary-path")
            .arg(&canonical_primary_path)
            .arg("--canonical-secondary-root")
            .arg(&query_canonical_secondary_root)
            .arg("--raw-blob-policy")
            .arg("all")
            .arg("--wallet-primary-path")
            .arg(&wallet_primary_path)
            .arg("--wallet-secondary-root")
            .arg(&query_wallet_secondary_root)
            .arg("--ingest-control-addr")
            .arg(&ingest_control.endpoint)
            .arg("--listen-addr")
            .arg(query_addr.to_string())
            .arg("--ops-listen-addr")
            .arg(query_ops_addr.to_string())
            .arg("--reorg-window-blocks")
            .arg("100")
            .arg("--node-json-rpc-addr")
            .arg(node_url);
        let mut query = ChildProcess::spawn("zinder-query incompatible replacement", &mut command)?;
        if let Err(error) =
            await_grpc_endpoint(query_addr, "zinder-query incompatible replacement").await
        {
            let stderr = query
                .stop()
                .await
                .unwrap_or_else(|stop_error| stop_error.to_string());
            let _ = ingest_control.stop().await;
            return Err(error).wrap_err(format!(
                "zinder-query incompatible replacement stderr:\n{stderr}"
            ));
        }

        Ok(Self {
            _canonical_store: canonical_store,
            construction_identity,
            ingest_control: Some(ingest_control),
            query,
        })
    }

    async fn stop(mut self) -> Result<()> {
        let query_stderr = self.query.stop().await?;
        if let Some(ingest_control) = self.ingest_control.take() {
            ingest_control.stop().await?;
        }
        if !query_stderr.is_empty() {
            tracing::debug!("P6a.2 incompatible Query replacement stopped");
        }
        Ok(())
    }
}

fn canonical_writer_status(
    fence: CanonicalEventFence,
    construction_identity: zinder_store::CanonicalStoreConstructionIdentity,
) -> CanonicalWriterStatusResponse {
    let binding = construction_identity.construction_manifest_binding();
    CanonicalWriterStatusResponse {
        network_name: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        fence: Some(CanonicalWriterFence {
            chain_epoch_id: fence.chain_epoch_id().value(),
            event_sequence: fence.chain_event_sequence(),
            visible_tip_height: fence.visible_tip().height.value(),
            visible_tip_hash: fence.visible_tip().hash.as_bytes().to_vec(),
            canonical_sequence_digest: fence.sequence_digest().as_bytes().to_vec(),
            visible_block_count: fence.sequence_digest().block_count(),
        }),
        oldest_retained_event_sequence: 1,
        canonical_construction_manifest_binding: Some(
            encode_canonical_construction_manifest_binding(
                CanonicalConstructionManifestBindingFields::new(binding.version, binding.sha256),
            ),
        ),
    }
}

fn p6a2_reorg_policy() -> Result<CanonicalReorgPolicy> {
    Ok(CanonicalReorgPolicy::new(
        ChainStoreOptions::for_local_tests().reorg_window_blocks,
    )?)
}

fn p6a2_wallet_transition_logical_bytes() -> Result<NonZeroU64> {
    NonZeroU64::new(MAX_WALLET_PROJECTION_TRANSITION_LOGICAL_BYTES)
        .ok_or_else(|| eyre!("P6a.2 wallet transition logical-byte limit must remain nonzero"))
}

fn exactly_one_retained_event(
    retained_events: &[zinder_store::CanonicalRetainedEvent],
    transition: &'static str,
) -> Result<zinder_store::CanonicalRetainedEvent> {
    let [event] = retained_events else {
        return Err(eyre!(
            "P6a.2 {transition} expected exactly one retained canonical event, got {}",
            retained_events.len()
        ));
    };
    Ok(*event)
}

fn p6a2_activations() -> Arc<NetworkUpgradeActivations> {
    Arc::new(sample_regtest_upgrade_activations())
}

struct OrderedNodeServer {
    endpoint: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl OrderedNodeServer {
    async fn spawn(activations: Arc<NetworkUpgradeActivations>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let upgrades = ordered_upgrade_result(&activations)?;
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = server_cancel.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let upgrades = upgrades.clone();
                            tokio::spawn(async move {
                                let _ = serve_ordered_node_connection(stream, &upgrades).await;
                            });
                        }
                        Err(_) => return,
                    },
                }
            }
        });
        Ok(Self {
            endpoint: format!("http://{address}"),
            cancel,
            handle,
        })
    }

    fn url(&self) -> &str {
        &self.endpoint
    }

    async fn stop(mut self) -> Result<()> {
        self.cancel.cancel();
        (&mut self.handle).await?;
        Ok(())
    }
}

impl Drop for OrderedNodeServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn p6a2_node_server() -> Result<OrderedNodeServer> {
    OrderedNodeServer::spawn(p6a2_activations()).await
}

fn ordered_upgrade_result(activations: &NetworkUpgradeActivations) -> Result<String> {
    let entries = activations
        .activations()
        .iter()
        .map(|activation| {
            Ok(format!(
                "\"{:08x}\":{{\"name\":{},\"activationheight\":{}}}",
                activation.branch_id.value(),
                serde_json::to_string(&activation.name)?,
                activation.activation_height.value(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("{{\"upgrades\":{{{}}}}}", entries.join(",")))
}

async fn serve_ordered_node_connection(
    mut stream: tokio::net::TcpStream,
    upgrades: &str,
) -> Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let headers_end = loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err(eyre!("ordered node connection ended before HTTP headers"));
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..headers_end])?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, header_value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| header_value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| eyre!("ordered node request omitted content length"))?;
    while request.len() < headers_end.saturating_add(content_length) {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err(eyre!("ordered node connection ended before JSON-RPC body"));
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let body = &request[headers_end..headers_end + content_length];
    let request_json: serde_json::Value = serde_json::from_slice(body)?;
    let id = request_json
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let id_json = serde_json::to_string(&id)?;
    let method = request_json
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| eyre!("ordered node JSON-RPC request omitted method"))?;
    let rpc_result = match method {
        "rpc.discover" => "{\"openrpc\":\"1.3.2\",\"methods\":[{\"name\":\"rpc.discover\"}]}",
        "getblockchaininfo" => upgrades,
        _ => "null",
    };
    let response = if rpc_result == "null" {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id_json},\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}"
        )
    } else {
        format!("{{\"jsonrpc\":\"2.0\",\"id\":{id_json},\"result\":{rpc_result}}}")
    };
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .as_bytes(),
        )
        .await?;
    Ok(())
}

async fn await_grpc_endpoint(address: SocketAddr, service: &'static str) -> Result<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(channel) = endpoint.connect().await {
                return channel;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| eyre!("{service} at {address} did not accept connections within ten seconds"))
}

fn unused_loopback_addr() -> Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

fn startup_admission_server_info(
    construction_identity: zinder_store::CanonicalStoreConstructionIdentity,
) -> wallet::ServerInfoResponse {
    let binding = construction_identity.construction_manifest_binding();
    wallet::ServerInfoResponse {
        info: Some(wallet::WalletServerInfo {
            common: Some(ops::ServerInfo {
                network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
                contract_revision: CONTRACT_REVISION,
                capabilities: vec![
                    capabilities::WALLET_READ_SERVER_INFO_V2.to_owned(),
                    capabilities::WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1.to_owned(),
                ],
                ..ops::ServerInfo::default()
            }),
            canonical_construction_manifest_binding: Some(
                encode_canonical_construction_manifest_binding(
                    CanonicalConstructionManifestBindingFields::new(
                        binding.version,
                        binding.sha256,
                    ),
                ),
            ),
            ..wallet::WalletServerInfo::default()
        }),
    }
}

fn wallet_activation_response(
    activations: &NetworkUpgradeActivations,
) -> wallet::NetworkUpgradeActivationsResponse {
    wallet::NetworkUpgradeActivationsResponse {
        activations: activations
            .activations()
            .iter()
            .map(|activation| wallet::NetworkUpgradeActivation {
                consensus_branch_id: activation.branch_id.value(),
                name: activation.name.clone(),
                activation_height: activation.activation_height.value(),
            })
            .collect(),
    }
}

fn p6a2_same_network_incompatible_activations() -> Result<NetworkUpgradeActivations> {
    let mut activations = p6a2_activations().activations().to_vec();
    let last_activation = activations
        .last_mut()
        .ok_or_else(|| eyre!("P6a.2 activation fixture unexpectedly has no entries"))?;
    let shifted_height = last_activation
        .activation_height
        .value()
        .checked_add(1)
        .ok_or_else(|| eyre!("P6a.2 activation fixture height cannot be shifted"))?;
    last_activation.activation_height = BlockHeight::new(shifted_height);
    Ok(NetworkUpgradeActivations::new(
        Network::ZcashRegtest,
        activations,
    )?)
}

struct ExplorerStartupCandidate {
    grpc_addr: SocketAddr,
    ops_addr: SocketAddr,
    succeeded: bool,
    stderr: String,
}

async fn run_explorer_startup_candidate(
    runtime: &P6a2BinaryRuntime,
    wallet_query_endpoint: &str,
    secondary_directory_name: &'static str,
) -> Result<ExplorerStartupCandidate> {
    let grpc_addr = unused_loopback_addr()?;
    let ops_addr = unused_loopback_addr()?;
    let canonical_primary_path = runtime.canonical_store.canonical_primary_path();
    let candidate_secondary_root = canonical_primary_path
        .parent()
        .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?
        .join(secondary_directory_name);
    fs::create_dir_all(&candidate_secondary_root).wrap_err_with(|| {
        format!(
            "create Explorer startup-candidate secondary root {}",
            candidate_secondary_root.display()
        )
    })?;

    let mut candidate = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
    candidate
        .env_clear()
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("--network")
        .arg("zcash-regtest")
        .arg("--storage-path")
        .arg(&canonical_primary_path)
        .arg("--secondary-path")
        .arg(candidate_secondary_root)
        .arg("--listen-addr")
        .arg(grpc_addr.to_string())
        .arg("--ops-listen-addr")
        .arg(ops_addr.to_string())
        .arg("--wallet-query-endpoint")
        .arg(wallet_query_endpoint);
    let output = tokio::time::timeout(Duration::from_secs(10), candidate.output())
        .await
        .map_err(|_| eyre!("Explorer startup candidate did not exit within ten seconds"))??;
    Ok(ExplorerStartupCandidate {
        grpc_addr,
        ops_addr,
        succeeded: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn assert_listener_never_published(
    address: SocketAddr,
    endpoint_name: &'static str,
) -> Result<()> {
    match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(address)).await {
        Ok(Ok(_)) => Err(eyre!(
            "Explorer startup rejection unexpectedly published {endpoint_name} at {address}"
        )),
        Ok(Err(_)) | Err(_) => Ok(()),
    }
}

async fn assert_explorer_startup_rejected_before_publication(
    candidate: ExplorerStartupCandidate,
    expected_classification: &'static str,
) -> Result<()> {
    assert!(
        !candidate.succeeded,
        "Explorer unexpectedly admitted an invalid Wallet endpoint"
    );
    assert!(
        candidate.stderr.contains("explorer_run_failed"),
        "Explorer startup failure did not emit explorer_run_failed:\n{}",
        candidate.stderr
    );
    assert!(
        candidate.stderr.contains(expected_classification),
        "Explorer startup failure did not report {expected_classification}:\n{}",
        candidate.stderr
    );
    assert_listener_never_published(candidate.grpc_addr, "ExplorerQuery gRPC").await?;
    assert_listener_never_published(candidate.ops_addr, "Explorer ops capability/readiness").await
}

async fn get_explorer_ops_json(
    address: SocketAddr,
    path: &'static str,
) -> Result<(u16, serde_json::Value)> {
    let mut stream = TcpStream::connect(address).await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let headers_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| eyre!("Explorer ops response omitted HTTP headers"))?;
    let headers = std::str::from_utf8(&response[..headers_end])?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| eyre!("Explorer ops response omitted an HTTP status"))?
        .parse::<u16>()?;
    Ok((
        status,
        serde_json::from_slice(&response[headers_end + 4..])?,
    ))
}

async fn get_explorer_healthz(address: SocketAddr) -> Result<serde_json::Value> {
    let (status, body) = get_explorer_ops_json(address, "/healthz").await?;
    assert_eq!(status, 200, "Explorer /healthz returned {status}");
    Ok(body)
}

async fn await_explorer_readyz(
    address: SocketAddr,
    expected_status: u16,
    expected_cause: &'static str,
) -> Result<serde_json::Value> {
    for _ in 0..150 {
        if let Ok((status, body)) = get_explorer_ops_json(address, "/readyz").await
            && status == expected_status
            && body["cause"] == expected_cause
        {
            return Ok(body);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(eyre!(
        "Explorer /readyz did not reach HTTP {expected_status} with cause {expected_cause} within fifteen seconds"
    ))
}

async fn await_wallet_query_visible_tip(
    address: SocketAddr,
    expected_tip: &FixtureBlock,
) -> Result<()> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2));
    let expected_hash = encode_rpc_block_hash_hex(expected_tip.hash);
    for _ in 0..150 {
        if let Ok(channel) = endpoint.connect().await {
            let mut client = wallet::wallet_query_client::WalletQueryClient::new(channel);
            if let Ok(response) = client
                .visible_tip_block(wallet::VisibleTipBlockRequest { at_epoch_id: None })
                .await
            {
                let visible_tip = response.into_inner().visible_tip_block;
                if visible_tip.is_some_and(|tip| {
                    tip.height == expected_tip.height.value() && tip.block_hash == expected_hash
                }) {
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(eyre!(
        "WalletQuery at {address} did not publish visible tip {}:{} within fifteen seconds",
        expected_tip.height.value(),
        expected_hash
    ))
}

async fn await_p6a2_positive_dispatch(address: SocketAddr, chain: &ChainFixture) -> Result<()> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2));
    let mut last_failure = "Explorer did not accept a connection".to_owned();
    for _ in 0..150 {
        match endpoint.connect().await {
            Ok(channel) => {
                let mut client = ExplorerQueryClient::new(channel);
                match assert_p6a2_positive_dispatch(&mut client, chain).await {
                    Ok(()) => return Ok(()),
                    Err(error) => last_failure = error.to_string(),
                }
            }
            Err(error) => last_failure = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(eyre!(
        "Explorer at {address} did not expose the expected P6a.2 positive contracts within fifteen seconds: {last_failure}"
    ))
}

async fn await_reorg_history_with_replacement(
    address: SocketAddr,
    replaced_tip: &FixtureBlock,
    replacement_tip: &FixtureBlock,
) -> Result<()> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2));
    let mut last_failure = "Explorer did not accept a connection".to_owned();
    for _ in 0..150 {
        match endpoint.connect().await {
            Ok(channel) => {
                let mut client = ExplorerQueryClient::new(channel);
                match assert_reorg_history_with_replacement(
                    &mut client,
                    replaced_tip,
                    replacement_tip,
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(error) => last_failure = error.to_string(),
                }
            }
            Err(error) => last_failure = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(eyre!(
        "Explorer at {address} did not expose the replacement reorg history within fifteen seconds: {last_failure}"
    ))
}

async fn assert_healthz_keeps_frozen_capabilities(address: SocketAddr) -> Result<Vec<String>> {
    let healthz = get_explorer_healthz(address).await?;
    assert_eq!(healthz["status"], "alive");
    let capabilities = healthz["capabilities"]
        .as_array()
        .ok_or_else(|| eyre!("Explorer /healthz omitted capabilities"))?
        .iter()
        .map(|capability_value| {
            capability_value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| eyre!("Explorer /healthz capability was not a string"))
        })
        .collect::<Result<Vec<_>>>()?;
    let expected = P6A2_EXPLORER_CAPABILITIES
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(capabilities, expected);
    Ok(capabilities)
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_real_query_and_explorer_binaries_admit_the_exact_live_append_fence() -> Result<()> {
    let chain = p6a2_chain_fixture()?;
    let runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let proof: Result<()> = async {
        let mut client = runtime.explorer_client().await?;
        assert_p6a2_positive_dispatch(&mut client, &chain).await?;
        assert_p6a2_omitted_dispatch_rejects_before_handler_validation(&mut client).await
    }
    .await;
    let shutdown = runtime.stop().await;
    proof?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_real_binaries_follow_a_live_append_and_replaced_suffix() -> Result<()> {
    let initial_chain = p6a2_chain_fixture()?;
    let appended_chain = p6a2_chain_with_tip_coinbase(
        initial_chain.clone().extend_blocks(1),
        TransactionId::from_bytes([0x43; 32]),
    )?;
    let replacement_chain = p6a2_chain_with_tip_coinbase(
        appended_chain
            .fork_at(BlockHeight::new(3))?
            .extend_blocks(1),
        TransactionId::from_bytes([0x44; 32]),
    )?;
    let appended_tip = p6a2_fixture_tip(&appended_chain, "append")?;
    let replacement_tip = p6a2_fixture_tip(&replacement_chain, "replacement")?;
    assert_eq!(appended_tip.height, replacement_tip.height);
    assert_ne!(appended_tip.hash, replacement_tip.hash);

    let mut runtime = P6a2BinaryRuntime::spawn(&initial_chain).await?;
    let proof: Result<()> = async {
        await_explorer_readyz(runtime.ops_addr, 200, "ready").await?;
        await_wallet_query_visible_tip(
            runtime.query_addr,
            p6a2_fixture_tip(&initial_chain, "initial")?,
        )
        .await?;

        let append_fence = runtime.append_live_block(&appended_chain)?;
        assert_eq!(
            append_fence.visible_tip(),
            BlockId::new(appended_tip.height, appended_tip.hash)
        );
        await_wallet_query_visible_tip(runtime.query_addr, appended_tip).await?;
        await_p6a2_positive_dispatch(runtime.explorer_addr, &appended_chain).await?;

        let replacement_fence = runtime.replace_live_suffix(&replacement_chain)?;
        assert_eq!(
            replacement_fence.visible_tip(),
            BlockId::new(replacement_tip.height, replacement_tip.hash)
        );
        await_wallet_query_visible_tip(runtime.query_addr, replacement_tip).await?;
        await_p6a2_positive_dispatch(runtime.explorer_addr, &replacement_chain).await?;
        await_reorg_history_with_replacement(runtime.explorer_addr, appended_tip, replacement_tip)
            .await
    }
    .await;
    let shutdown = runtime.stop().await;
    proof?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_healthz_capability_order_matches_grpc_server_info() -> Result<()> {
    let chain = p6a2_chain_fixture()?;
    let runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let proof: Result<()> = async {
        let healthz = get_explorer_healthz(runtime.ops_addr).await?;
        assert_eq!(healthz["status"], "alive");
        assert_eq!(healthz["service"], "zinder-explorer");
        assert_eq!(healthz["network"], "zcash-regtest");
        let healthz_capabilities = healthz["capabilities"]
            .as_array()
            .ok_or_else(|| eyre!("Explorer /healthz omitted capabilities"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| eyre!("Explorer /healthz capability was not a string"))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut client = runtime.explorer_client().await?;
        let server_info = client
            .server_info(explorer::ServerInfoRequest {})
            .await?
            .into_inner();
        let grpc_capabilities = server_info
            .info
            .and_then(|server_info| server_info.common)
            .ok_or_else(|| eyre!("Explorer ServerInfo omitted common identity"))?
            .capabilities;
        let expected = P6A2_EXPLORER_CAPABILITIES
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(grpc_capabilities, expected);
        assert_eq!(healthz_capabilities, grpc_capabilities);
        Ok(())
    }
    .await;
    let shutdown = runtime.stop().await;
    proof?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_occupied_explorer_listener_fails_before_ops_capability_publication() -> Result<()> {
    let chain = p6a2_chain_fixture()?;
    let runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let proof: Result<()> = async {
        let occupied_listener = TcpListener::bind("127.0.0.1:0").await?;
        let occupied_addr = occupied_listener.local_addr()?;
        let candidate_ops_addr = unused_loopback_addr()?;
        let canonical_primary_path = runtime.canonical_store.canonical_primary_path();
        let candidate_secondary_root = canonical_primary_path
            .parent()
            .ok_or_else(|| eyre!("canonical fixture primary has no parent directory"))?
            .join("p6a2-colliding-explorer-secondary");
        fs::create_dir_all(&candidate_secondary_root)?;

        let mut candidate = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
        candidate
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("--network")
            .arg("zcash-regtest")
            .arg("--storage-path")
            .arg(&canonical_primary_path)
            .arg("--secondary-path")
            .arg(&candidate_secondary_root)
            .arg("--listen-addr")
            .arg(occupied_addr.to_string())
            .arg("--ops-listen-addr")
            .arg(candidate_ops_addr.to_string())
            .arg("--wallet-query-endpoint")
            .arg(format!("http://{}", runtime.query_addr));
        let output = tokio::time::timeout(Duration::from_secs(10), candidate.output())
            .await
            .map_err(|_| eyre!("colliding Explorer process did not exit within ten seconds"))??;
        assert!(
            !output.status.success(),
            "Explorer unexpectedly started with occupied gRPC listener {occupied_addr}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("failed to bind ExplorerQuery listener at {occupied_addr}")),
            "colliding Explorer did not report its occupied gRPC listener:\n{stderr}"
        );
        assert!(
            TcpStream::connect(candidate_ops_addr).await.is_err(),
            "a candidate that cannot bind ExplorerQuery must not publish /healthz capabilities or readiness"
        );
        drop(occupied_listener);
        Ok(())
    }
    .await;
    let shutdown = runtime.stop().await;
    proof?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_startup_rejects_wallet_construction_binding_mismatch_before_publication() -> Result<()>
{
    let chain = p6a2_chain_fixture()?;
    let runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let mut replacement = None;
    let proof: Result<()> = async {
        let replacement_query_addr = unused_loopback_addr()?;
        let node_url = runtime
            .node
            .as_ref()
            .ok_or_else(|| eyre!("P6a.2 node fixture is not running"))?
            .url()
            .to_owned();
        let incompatible = IncompatibleQueryReplacement::spawn(
            &runtime.query_binary,
            replacement_query_addr,
            node_url,
        )
        .await?;
        assert_ne!(
            incompatible
                .construction_identity
                .construction_manifest_binding(),
            runtime
                .construction_identity
                .construction_manifest_binding(),
            "the replacement must present a genuinely different Wallet construction binding"
        );
        replacement = Some(incompatible);

        let wallet_query_endpoint = format!("http://{replacement_query_addr}");
        let candidate = run_explorer_startup_candidate(
            &runtime,
            &wallet_query_endpoint,
            "p6a2-startup-construction-mismatch-secondary",
        )
        .await?;
        assert_explorer_startup_rejected_before_publication(
            candidate,
            "wallet and materialized-view store construction identities differ",
        )
        .await
    }
    .await;
    let replacement_shutdown = match replacement {
        Some(replacement) => replacement.stop().await,
        None => Ok(()),
    };
    let shutdown = runtime.stop().await;
    proof?;
    replacement_shutdown?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_startup_rejects_wallet_activation_fingerprint_mismatch_before_publication()
-> Result<()> {
    let chain = p6a2_chain_fixture()?;
    let runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let mut startup_wallet = None;
    let proof: Result<()> = async {
        let incompatible_activations = p6a2_same_network_incompatible_activations()?;
        let expected_fingerprint = runtime
            .construction_identity
            .network_upgrade_activations_fingerprint();
        assert_eq!(
            incompatible_activations.network(),
            runtime.construction_identity.network(),
            "the test Wallet must remain on the Explorer materialized-view network"
        );
        assert_ne!(
            incompatible_activations.fingerprint(expected_fingerprint.version()),
            expected_fingerprint,
            "the test Wallet activation evidence must differ from the materialized-view identity"
        );
        let wallet = StartupAdmissionWalletServer::spawn(StartupAdmissionWalletQuery {
            server_info: startup_admission_server_info(runtime.construction_identity),
            network_upgrade_activations: wallet_activation_response(&incompatible_activations),
        })
        .await?;
        let wallet_query_endpoint = wallet.endpoint.clone();
        startup_wallet = Some(wallet);

        let candidate = run_explorer_startup_candidate(
            &runtime,
            &wallet_query_endpoint,
            "p6a2-startup-activation-mismatch-secondary",
        )
        .await?;
        assert_explorer_startup_rejected_before_publication(
            candidate,
            "wallet activation-table fingerprint differs from materialized-view identity",
        )
        .await
    }
    .await;
    let startup_wallet_shutdown = match startup_wallet {
        Some(startup_wallet) => startup_wallet.stop().await,
        None => Ok(()),
    };
    let shutdown = runtime.stop().await;
    proof?;
    startup_wallet_shutdown?;
    shutdown
}

#[tokio::test]
#[ignore = "requires an explicitly built zinder-query binary via ZINDER_TEST_QUERY_BINARY"]
async fn p6a2_wallet_loss_recovery_and_contract_change_gate_every_new_explorer_call() -> Result<()>
{
    let chain = p6a2_chain_fixture()?;
    let mut runtime = P6a2BinaryRuntime::spawn(&chain).await?;
    let mut incompatible_replacement = None;
    let proof: Result<()> = async {
        let initial_healthz_capabilities =
            assert_healthz_keeps_frozen_capabilities(runtime.ops_addr).await?;
        await_explorer_readyz(runtime.ops_addr, 200, "ready").await?;
        let mut initial_client = runtime.explorer_client().await?;
        let initial_discovery = explorer_server_info_capabilities(&mut initial_client).await?;
        assert_eq!(initial_discovery, initial_healthz_capabilities);

        runtime.stop_query().await?;
        await_explorer_readyz(runtime.ops_addr, 503, "storage_unavailable").await?;
        assert_eq!(
            assert_healthz_keeps_frozen_capabilities(runtime.ops_addr).await?,
            initial_healthz_capabilities
        );
        let mut unavailable_client = runtime.explorer_client().await?;
        assert_every_explorer_rpc_is_unavailable(&mut unavailable_client).await?;

        runtime.restart_compatible_query().await?;
        await_explorer_readyz(runtime.ops_addr, 200, "ready").await?;
        assert_eq!(
            assert_healthz_keeps_frozen_capabilities(runtime.ops_addr).await?,
            initial_healthz_capabilities
        );
        let mut recovered_client = runtime.explorer_client().await?;
        assert_eq!(
            explorer_server_info_capabilities(&mut recovered_client).await?,
            initial_discovery
        );
        assert_p6a2_positive_dispatch(&mut recovered_client, &chain).await?;

        runtime.stop_query().await?;
        let node_url = runtime
            .node
            .as_ref()
            .ok_or_else(|| eyre!("P6a.2 node fixture is not running"))?
            .url()
            .to_owned();
        let replacement = IncompatibleQueryReplacement::spawn(
            &runtime.query_binary,
            runtime.query_addr,
            node_url,
        )
        .await?;
        assert_ne!(
            replacement
                .construction_identity
                .construction_manifest_binding(),
            runtime
                .construction_identity
                .construction_manifest_binding(),
            "the replacement must present a genuinely different Wallet construction binding"
        );
        incompatible_replacement = Some(replacement);
        await_explorer_readyz(runtime.ops_addr, 503, "schema_mismatch").await?;
        assert_eq!(
            assert_healthz_keeps_frozen_capabilities(runtime.ops_addr).await?,
            initial_healthz_capabilities
        );
        let mut contract_changed_client = runtime.explorer_client().await?;
        assert_every_explorer_rpc_is_unavailable(&mut contract_changed_client).await?;
        Ok(())
    }
    .await;
    let replacement_shutdown = match incompatible_replacement {
        Some(replacement) => replacement.stop().await,
        None => Ok(()),
    };
    let shutdown = runtime.stop().await;
    proof?;
    replacement_shutdown?;
    shutdown
}

#[allow(
    clippy::too_many_lines,
    reason = "The fixed P6a.2 allocation requires one explicit positive call per admitted RPC."
)]
async fn assert_p6a2_positive_dispatch(
    client: &mut ExplorerQueryClient<Channel>,
    chain: &ChainFixture,
) -> Result<()> {
    let expected_tip = p6a2_fixture_tip(chain, "positive dispatch")?;
    let expected_tip_height = expected_tip.height.value();
    let mut dispatched = Vec::with_capacity(P6A2_RPC_CAPABILITIES.len());
    let server_info = client
        .server_info(explorer::ServerInfoRequest {})
        .await?
        .into_inner();
    assert_freshness(
        server_info.freshness.as_ref(),
        EXPLORER_SERVER_INFO_V1,
        "ServerInfo",
    )?;
    let common = server_info
        .info
        .and_then(|server_info| server_info.common)
        .ok_or_else(|| eyre!("Explorer ServerInfo omitted common identity"))?;
    let expected = P6A2_EXPLORER_CAPABILITIES
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(common.capabilities, expected);
    dispatched.push(EXPLORER_SERVER_INFO_V1);

    let summaries = client
        .block_summaries_in_range(explorer::BlockSummariesInRangeRequest {
            start_height: 1,
            end_height: expected_tip_height,
        })
        .await?
        .into_inner();
    assert_freshness(
        summaries.freshness.as_ref(),
        EXPLORER_BLOCK_SUMMARY_V2,
        "BlockSummariesInRange",
    )?;
    assert!(summaries.summaries.iter().any(|summary| {
        summary.block_height == expected_tip_height
            && summary.block_hash == encode_rpc_block_hash_hex(expected_tip.hash)
    }));
    dispatched.push(EXPLORER_BLOCK_SUMMARY_V2);

    let reorg_history = client
        .chain_reorg_history(explorer::ChainReorgHistoryRequest {
            max_events: 10,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    assert_freshness(
        reorg_history.freshness.as_ref(),
        EXPLORER_CHAIN_REORG_HISTORY_V1,
        "ChainReorgHistory",
    )?;
    dispatched.push(EXPLORER_CHAIN_REORG_HISTORY_V1);

    let mut recent = client
        .recent_transactions(explorer::RecentTransactionsRequest {
            max_entries: 10,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    let mut paid_fee_seen = false;
    while let Some(chunk) = recent.message().await? {
        assert_freshness(
            chunk.freshness.as_ref(),
            EXPLORER_TRANSACTION_RECENT_V1,
            "RecentTransactions",
        )?;
        paid_fee_seen |= chunk
            .entries
            .iter()
            .any(|entry| entry.paid_fee_zat == Some(10_000));
    }
    assert!(
        paid_fee_seen,
        "the admitted transaction-fee field must carry the resolved transparent fee"
    );
    dispatched.push(EXPLORER_TRANSACTION_RECENT_V1);

    let mempool_counts = client
        .mempool_event_counts(explorer::MempoolEventCountsRequest { window_seconds: 60 })
        .await?
        .into_inner();
    assert_freshness(
        mempool_counts.freshness.as_ref(),
        EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
        "MempoolEventCounts",
    )?;
    dispatched.push(EXPLORER_MEMPOOL_EVENT_COUNTS_V1);

    let fee_summary = client
        .fee_summary(explorer::FeeSummaryRequest {
            start_height: 1,
            end_height: expected_tip_height,
        })
        .await?
        .into_inner();
    assert_freshness(
        fee_summary.freshness.as_ref(),
        EXPLORER_FEE_SUMMARY_V1,
        "FeeSummary",
    )?;
    dispatched.push(EXPLORER_FEE_SUMMARY_V1);

    let upgrades = client
        .network_upgrade_status(explorer::NetworkUpgradeStatusRequest {})
        .await?
        .into_inner();
    assert_freshness(
        upgrades.freshness.as_ref(),
        EXPLORER_NETWORK_UPGRADE_STATUS_V1,
        "NetworkUpgradeStatus",
    )?;
    assert_eq!(
        upgrades.upgrades.len(),
        p6a2_activations().activations().len()
    );
    dispatched.push(EXPLORER_NETWORK_UPGRADE_STATUS_V1);

    let migration_overview = client
        .migration_overview(explorer::MigrationOverviewRequest::default())
        .await?
        .into_inner();
    assert_freshness(
        migration_overview.freshness.as_ref(),
        EXPLORER_MIGRATION_OVERVIEW_V1,
        "MigrationOverview",
    )?;
    dispatched.push(EXPLORER_MIGRATION_OVERVIEW_V1);

    let migration_cohorts = client
        .migration_cohorts(explorer::MigrationCohortsRequest {
            start_height: 1,
            end_height: expected_tip_height,
        })
        .await?
        .into_inner();
    assert_freshness(
        migration_cohorts.freshness.as_ref(),
        EXPLORER_MIGRATION_COHORTS_V1,
        "MigrationCohorts",
    )?;
    dispatched.push(EXPLORER_MIGRATION_COHORTS_V1);

    let migration_denominations = client
        .migration_denominations(explorer::MigrationDenominationsRequest {
            start_height: 1,
            end_height: expected_tip_height,
        })
        .await?
        .into_inner();
    assert_freshness(
        migration_denominations.freshness.as_ref(),
        EXPLORER_MIGRATION_DENOMINATIONS_V1,
        "MigrationDenominations",
    )?;
    dispatched.push(EXPLORER_MIGRATION_DENOMINATIONS_V1);

    assert_eq!(dispatched, P6A2_RPC_CAPABILITIES);
    assert_eq!(P6A2_FIELD_CAPABILITIES, [EXPLORER_TRANSACTION_FEES_V1]);
    Ok(())
}

async fn assert_reorg_history_with_replacement(
    client: &mut ExplorerQueryClient<Channel>,
    replaced_tip: &FixtureBlock,
    replacement_tip: &FixtureBlock,
) -> Result<()> {
    let history = client
        .chain_reorg_history(explorer::ChainReorgHistoryRequest {
            max_events: 10,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    assert_freshness(
        history.freshness.as_ref(),
        EXPLORER_CHAIN_REORG_HISTORY_V1,
        "ChainReorgHistory replacement",
    )?;
    let event = history
        .events
        .last()
        .ok_or_else(|| eyre!("P6a.2 replacement did not create a reorg-history event"))?;
    assert!(
        !event.cursor.is_empty(),
        "P6a.2 replacement reorg history omitted its resume cursor"
    );
    let visible_tip = event
        .visible_tip
        .as_ref()
        .ok_or_else(|| eyre!("P6a.2 replacement reorg history omitted its visible tip"))?;
    assert_eq!(visible_tip.height, replacement_tip.height.value());
    assert_eq!(
        visible_tip.hash,
        encode_rpc_block_hash_hex(replacement_tip.hash)
    );
    let reverted = event
        .reverted
        .as_ref()
        .ok_or_else(|| eyre!("P6a.2 replacement reorg history omitted its reverted range"))?;
    assert_eq!(reverted.start_height, replaced_tip.height.value());
    assert_eq!(reverted.end_height, replaced_tip.height.value());
    let committed = event
        .committed
        .as_ref()
        .ok_or_else(|| eyre!("P6a.2 replacement reorg history omitted its committed range"))?;
    assert_eq!(committed.start_height, replacement_tip.height.value());
    assert_eq!(committed.end_height, replacement_tip.height.value());
    let committed_epoch = committed.chain_epoch.as_ref().ok_or_else(|| {
        eyre!("P6a.2 replacement reorg history omitted its committed chain epoch")
    })?;
    let committed_tip = committed_epoch
        .visible_tip
        .as_ref()
        .ok_or_else(|| eyre!("P6a.2 replacement committed chain epoch omitted its visible tip"))?;
    assert_eq!(committed_tip.height, replacement_tip.height.value());
    assert_eq!(
        committed_tip.hash,
        encode_rpc_block_hash_hex(replacement_tip.hash)
    );
    Ok(())
}

fn p6a2_fixture_tip<'chain>(
    chain: &'chain ChainFixture,
    phase: &'static str,
) -> Result<&'chain FixtureBlock> {
    chain
        .blocks()
        .last()
        .ok_or_else(|| eyre!("P6a.2 {phase} fixture omitted its visible tip"))
}

async fn explorer_server_info_capabilities(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<Vec<String>> {
    Ok(client
        .server_info(explorer::ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .and_then(|server_info| server_info.common)
        .ok_or_else(|| eyre!("Explorer ServerInfo omitted common identity"))?
        .capabilities)
}

#[allow(
    clippy::too_many_lines,
    reason = "The readiness gate must reject every generated ExplorerQuery RPC before a handler can run."
)]
async fn assert_every_explorer_rpc_is_unavailable(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<()> {
    let mut rejected =
        Vec::with_capacity(P6A2_RPC_CAPABILITIES.len() + P6A2_OMITTED_RPC_CAPABILITIES.len());

    // Default request shapes intentionally omit normal identifiers, ranges,
    // cursors, and filters. An UNAVAILABLE result for each therefore proves
    // that the outer readiness interceptor rejects before handler validation,
    // local storage, or the admitted Wallet dependency can be reached.
    macro_rules! assert_unavailable_rpc {
        ($capability:expr, $method:ident, $request:expr) => {{
            assert_unavailable(client.$method($request).await, $capability)?;
            rejected.push($capability);
        }};
    }

    assert_unavailable_rpc!(
        EXPLORER_SERVER_INFO_V1,
        server_info,
        explorer::ServerInfoRequest {}
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
        transaction_detail,
        explorer::TransactionDetailRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_BLOCK_SUMMARY_V2,
        block_summaries_in_range,
        explorer::BlockSummariesInRangeRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
        block_production_series,
        explorer::BlockProductionSeriesRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
        block_production_in_time_range,
        explorer::BlockProductionInTimeRangeRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
        block_activity_distribution,
        explorer::BlockActivityDistributionRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
        transaction_component_summary,
        explorer::TransactionComponentSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
        transparent_address_ranking,
        explorer::TransparentAddressRankingRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_BLOCK_DETAIL_V1,
        block_detail,
        explorer::BlockDetailRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
        block_transactions,
        explorer::BlockDetailRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_SEARCH_V1,
        search,
        explorer::SearchRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
        commitment_root_search,
        explorer::CommitmentRootSearchRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
        mempool_summary,
        explorer::MempoolSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
        mempool_snapshot,
        explorer::MempoolSnapshotRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
        mempool_activity,
        explorer::MempoolActivityRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
        transparent_address_activity,
        explorer::TransparentAddressActivityRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
        transparent_address_deltas,
        explorer::TransparentAddressDeltasRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_FEE_SUMMARY_V1,
        fee_summary,
        explorer::FeeSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
        conventional_fee_distribution,
        explorer::ConventionalFeeDistributionRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
        paid_fee_distribution,
        explorer::PaidFeeDistributionRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
        value_pool_summary,
        explorer::ValuePoolSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_NETWORK_UPGRADE_STATUS_V1,
        network_upgrade_status,
        explorer::NetworkUpgradeStatusRequest {}
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
        value_pool_flow_history,
        explorer::ValuePoolFlowHistoryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
        value_pool_flow_events_in_range,
        explorer::ValuePoolFlowEventsInRangeRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
        value_pool_flow_summary,
        explorer::ValuePoolFlowSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
        value_pool_flow_amount_threshold_summary,
        explorer::ValuePoolFlowAmountThresholdSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
        value_pool_flow_rounded_amount_summary,
        explorer::ValuePoolFlowRoundedAmountSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
        value_pool_balance_history,
        explorer::ValuePoolBalanceHistoryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
        utxo_set_summary,
        explorer::UtxoSetSummaryRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_CHAIN_REORG_HISTORY_V1,
        chain_reorg_history,
        explorer::ChainReorgHistoryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
        displaced_block_history,
        explorer::DisplacedBlockHistoryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
        displaced_block_detail,
        explorer::DisplacedBlockDetailRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
        mempool_event_counts,
        explorer::MempoolEventCountsRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_TRANSACTION_RECENT_V1,
        recent_transactions,
        explorer::RecentTransactionsRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
        transaction_history,
        explorer::TransactionHistoryRequest::default()
    );
    assert_unavailable_rpc!(
        capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
        overview_snapshot,
        explorer::OverviewSnapshotRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_MIGRATION_OVERVIEW_V1,
        migration_overview,
        explorer::MigrationOverviewRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_MIGRATION_COHORTS_V1,
        migration_cohorts,
        explorer::MigrationCohortsRequest::default()
    );
    assert_unavailable_rpc!(
        EXPLORER_MIGRATION_DENOMINATIONS_V1,
        migration_denominations,
        explorer::MigrationDenominationsRequest::default()
    );

    let mut expected = P6A2_RPC_CAPABILITIES
        .iter()
        .chain(P6A2_OMITTED_RPC_CAPABILITIES.iter())
        .copied()
        .collect::<Vec<_>>();
    expected.sort_unstable();
    rejected.sort_unstable();
    assert_eq!(rejected, expected);
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "The frozen P6a.2 omission contract requires one real RPC call per omitted endpoint."
)]
async fn assert_p6a2_omitted_dispatch_rejects_before_handler_validation(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<()> {
    let mut rejected = Vec::with_capacity(P6A2_OMITTED_RPC_CAPABILITIES.len());

    // Every request below omits the identifier, range, cursor, or filter a
    // normal handler would validate or normalize. `UNIMPLEMENTED` therefore
    // demonstrates that the local capability fence runs before request
    // validation and before an omitted handler can use a local or Wallet read.
    macro_rules! assert_omitted {
        ($capability:expr, $method:ident, $request:expr) => {{
            assert_unimplemented(client.$method($request).await, $capability)?;
            rejected.push($capability);
        }};
    }

    assert_omitted!(
        capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
        transaction_detail,
        explorer::TransactionDetailRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
        block_production_series,
        explorer::BlockProductionSeriesRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
        block_production_in_time_range,
        explorer::BlockProductionInTimeRangeRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_BLOCK_DETAIL_V1,
        block_detail,
        explorer::BlockDetailRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
        block_transactions,
        explorer::BlockDetailRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
        block_activity_distribution,
        explorer::BlockActivityDistributionRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_SEARCH_V1,
        search,
        explorer::SearchRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
        commitment_root_search,
        explorer::CommitmentRootSearchRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
        mempool_summary,
        explorer::MempoolSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
        mempool_snapshot,
        explorer::MempoolSnapshotRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
        mempool_activity,
        explorer::MempoolActivityRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
        transparent_address_activity,
        explorer::TransparentAddressActivityRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
        transparent_address_deltas,
        explorer::TransparentAddressDeltasRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
        conventional_fee_distribution,
        explorer::ConventionalFeeDistributionRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
        paid_fee_distribution,
        explorer::PaidFeeDistributionRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
        transaction_component_summary,
        explorer::TransactionComponentSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
        transparent_address_ranking,
        explorer::TransparentAddressRankingRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
        value_pool_summary,
        explorer::ValuePoolSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
        value_pool_flow_history,
        explorer::ValuePoolFlowHistoryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
        value_pool_flow_events_in_range,
        explorer::ValuePoolFlowEventsInRangeRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
        value_pool_flow_summary,
        explorer::ValuePoolFlowSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
        value_pool_flow_amount_threshold_summary,
        explorer::ValuePoolFlowAmountThresholdSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
        value_pool_flow_rounded_amount_summary,
        explorer::ValuePoolFlowRoundedAmountSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
        value_pool_balance_history,
        explorer::ValuePoolBalanceHistoryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
        utxo_set_summary,
        explorer::UtxoSetSummaryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
        displaced_block_history,
        explorer::DisplacedBlockHistoryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
        displaced_block_detail,
        explorer::DisplacedBlockDetailRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
        transaction_history,
        explorer::TransactionHistoryRequest::default()
    );
    assert_omitted!(
        capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
        overview_snapshot,
        explorer::OverviewSnapshotRequest::default()
    );

    assert_eq!(rejected, P6A2_OMITTED_RPC_CAPABILITIES);
    assert_eq!(
        P6A2_OMITTED_FIELD_CARRIERS
            .iter()
            .map(|(field, _)| *field)
            .collect::<Vec<_>>(),
        P6A2_OMITTED_FIELD_CAPABILITIES
    );
    for (field_capability, carrier_capabilities) in P6A2_OMITTED_FIELD_CARRIERS {
        assert!(
            P6A2_OMITTED_FIELD_CAPABILITIES.contains(&field_capability),
            "{field_capability} must remain an omitted field capability"
        );
        for carrier_capability in carrier_capabilities {
            assert!(
                rejected.contains(carrier_capability),
                "{field_capability} has a reachable carrier {carrier_capability}"
            );
        }
    }
    Ok(())
}

fn assert_unimplemented<T>(
    rpc_result: std::result::Result<Response<T>, Status>,
    capability: &str,
) -> Result<()> {
    let status = rpc_result
        .err()
        .ok_or_else(|| eyre!("{capability} unexpectedly dispatched"))?;
    assert_eq!(
        status.code(),
        Code::Unimplemented,
        "{capability} did not reject at its capability fence: {status}"
    );
    Ok(())
}

fn assert_unavailable<T>(
    rpc_result: std::result::Result<Response<T>, Status>,
    capability: &str,
) -> Result<()> {
    let status = rpc_result.err().ok_or_else(|| {
        eyre!("{capability} unexpectedly dispatched while Explorer was not ready")
    })?;
    assert_eq!(
        status.code(),
        Code::Unavailable,
        "{capability} did not reject at Explorer's traffic readiness gate: {status}"
    );
    Ok(())
}

fn assert_freshness(
    freshness: Option<&explorer::ExplorerFreshness>,
    expected_capability: &str,
    method: &str,
) -> Result<()> {
    let freshness = freshness.ok_or_else(|| eyre!("{method} omitted ExplorerFreshness"))?;
    assert_eq!(freshness.capability_version, expected_capability);
    Ok(())
}

fn p6a2_chain_fixture() -> Result<ChainFixture> {
    let base = ChainFixture::new(Network::ZcashRegtest)
        .with_raw_blob_retention(RawBlobRetention::Transactions)
        .extend_blocks(2);
    let block = base
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("P6a.2 live-append fixture omitted its final block"))?;
    let coinbase_transaction_id = TransactionId::from_bytes([0x41; 32]);
    let spend_transaction_id = TransactionId::from_bytes([0x42; 32]);
    let coinbase = coinbase_transaction_row(block.height, block.hash, coinbase_transaction_id);
    let spent_script_pub_key = vec![0x51];
    let spent_script_hash = TransparentAddressScriptHash::of_script_pub_key(&spent_script_pub_key);
    let output_script_pub_key = vec![0x52];
    let spent_outpoint = TransparentOutPoint::new(coinbase_transaction_id, 0);
    let mut facts = synthetic_transaction_public_facts(spend_transaction_id, 80);
    facts.version = TransactionVersion::V5;
    facts.counts = TransactionComponentCounts {
        transparent_input_count: 1,
        transparent_output_count: 1,
        ..TransactionComponentCounts::EMPTY
    };
    facts.privacy_shape = PrivacyShape::TransparentOnly;
    let mut spend = FixtureTransactionRows::from_raw_transaction(
        spend_transaction_id,
        block.height,
        block.hash,
        1,
        vec![0x42; 80],
    );
    spend.facts = TransactionFactsArtifact::new(spend.location, facts).with_transparent_facts(
        vec![TransparentInputFact::new(0, spent_outpoint)],
        vec![transparent_output_fact(0, 11_000, &output_script_pub_key)],
    );
    let transparent_spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        spend_transaction_id,
        1,
        block.height,
        block.hash,
        21_000,
        spent_script_hash,
        block.height,
        block.hash,
    );
    Ok(base
        .with_transaction_rows(coinbase)
        .with_transaction_rows(spend)
        .with_transparent_spend_fact(transparent_spend))
}

fn p6a2_chain_with_tip_coinbase(
    chain: ChainFixture,
    transaction_id: TransactionId,
) -> Result<ChainFixture> {
    let tip = p6a2_fixture_tip(&chain, "tip coinbase")?;
    let tip_id = BlockId::new(tip.height, tip.hash);
    Ok(chain.with_transaction_rows(coinbase_transaction_row(
        tip_id.height,
        tip_id.hash,
        transaction_id,
    )))
}

fn coinbase_transaction_row(
    block_height: BlockHeight,
    block_hash: BlockHash,
    transaction_id: TransactionId,
) -> FixtureTransactionRows {
    let mut facts = synthetic_transaction_public_facts(transaction_id, 120);
    facts.is_coinbase = true;
    facts.counts.transparent_input_count = 1;
    facts.counts.transparent_output_count = 2;
    let mut transaction = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block_height,
        block_hash,
        0,
        vec![0x41; 120],
    );
    transaction.facts = TransactionFactsArtifact::new(transaction.location, facts)
        .with_transparent_facts(
            Vec::new(),
            vec![
                transparent_output_fact(0, 21_000, &[0x51]),
                transparent_output_fact(1, 34_000, &[0x52]),
            ],
        );
    transaction
}

fn transparent_output_fact(
    output_index: u32,
    value_zat: u64,
    script_pub_key: &[u8],
) -> TransparentOutputFact {
    TransparentOutputFact::new(
        output_index,
        value_zat,
        script_pub_key.to_owned(),
        TransparentAddressScriptHash::of_script_pub_key(script_pub_key),
    )
}
