//! Background tasks that drive derive consumers against `WalletQuery`.
//!
//! One async task per consumer. All tasks share a single
//! [`AuthenticatedChannel`] to `zinder-query` (HTTP/2 multiplexes per-stream)
//! and, for chain-events consumers, one [`BlockSource`] (so per-envelope
//! fan-out across consumers parses each block exactly once).
//!
//! On a transient stream failure each runner sleeps for
//! [`RECONNECT_BACKOFF`] and reconnects from the latest persisted cursor.
//! Cancellation through the binary's shared token unblocks both the sleep
//! and the subscriber loop.
//!
//! Adding a new chain-events consumer requires only a new
//! [`ChainEventsConsumerSpec`] entry; the runner does the rest.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use zinder_explorer::{
    BLOCK_SUMMARY_CONSUMER_NAME, BlockCommitContextError, BlockSource, BlockSummaryConsumer,
    DeriveConsumer, DeriveError, DeriveMempoolConsumer, DeriveStore,
    MEMPOOL_EVENT_COUNTS_CONSUMER_NAME, MempoolEventCountsConsumer, PrevoutResolver,
    RECENT_TRANSACTIONS_CONSUMER_NAME, RecentTransactionsConsumer, TRANSACTION_FEES_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TransactionFeesConsumer,
    TransparentAddressActivityConsumer, run_chain_events_subscriber, run_mempool_events_subscriber,
};
use zinder_proto::v1::wallet::{
    ChainEventStreamFamily as WireChainEventStreamFamily, ChainEventsRequest,
    MempoolEventStreamFamily as WireMempoolEventStreamFamily, MempoolEventsRequest,
    ServerInfoRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::{AuthenticatedChannel, connect_zinder_grpc};

/// Backoff used between reconnect attempts after a transient stream failure.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Maximum time the initial `chain_events` or `mempool_events` subscribe
/// call may take before the reconnect loop treats the attempt as failed.
///
/// Belt-and-suspenders for the channel-level HTTP/2 + TCP keep-alives
/// configured in [`zinder_runtime::connect_zinder_grpc`]: if a
/// keep-alive somehow misses a half-dead connection, the subscribe-await
/// still surfaces as a timeout instead of wedging the consumer task
/// indefinitely.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared per-process state every consumer task needs.
///
/// Built once at startup, cloned cheaply into each spawned consumer. The
/// `wallet_channel` field is the single HTTP/2 connection to
/// `zinder-query`; the `block_source` field is the single parsed-block
/// cache.
#[derive(Clone)]
pub(crate) struct ConsumerRunnerEnvironment {
    pub store: DeriveStore,
    pub wallet_channel: AuthenticatedChannel,
    pub block_source: BlockSource,
}

/// Builds the shared environment by dialing `wallet_endpoint` once and
/// probing the upstream's prevout-resolution capability.
///
/// Returns `Ok((env, prevouts_online))`; the binary uses `prevouts_online`
/// to decide which capabilities the gRPC adapter advertises.
pub(crate) async fn build_environment(
    store: DeriveStore,
    wallet_endpoint: &str,
) -> Result<(ConsumerRunnerEnvironment, bool), ConsumerRunnerError> {
    let wallet_channel = connect_zinder_grpc(wallet_endpoint, None)
        .await
        .map_err(|error| ConsumerRunnerError::Connect(error.to_string()))?;
    let mut probe_client = WalletQueryClient::new(wallet_channel.clone());
    let prevouts_online = probe_prevouts_capability(&mut probe_client)
        .await
        .unwrap_or(false);
    // BlockSource uses its own dedicated channel so the long-lived
    // chain-events server-streams cannot share a connection-driver task
    // with the per-height FullBlock unary RPCs. Sharing one channel
    // showed pathological hangs where every concurrent FullBlock froze
    // mid-stream once the chain-events streams had been open for ~1 s.
    let block_source_channel = connect_zinder_grpc(wallet_endpoint, None)
        .await
        .map_err(|error| ConsumerRunnerError::Connect(error.to_string()))?;
    let prevout_resolver = if prevouts_online {
        let prevouts_channel = connect_zinder_grpc(wallet_endpoint, None)
            .await
            .map_err(|error| ConsumerRunnerError::Connect(error.to_string()))?;
        PrevoutResolver::online(prevouts_channel)
    } else {
        PrevoutResolver::Offline
    };
    let block_source = BlockSource::new(block_source_channel, prevout_resolver);
    Ok((
        ConsumerRunnerEnvironment {
            store,
            wallet_channel,
            block_source,
        },
        prevouts_online,
    ))
}

/// Spawns every chain-events and mempool-events consumer the explorer
/// runs. Returns one [`JoinHandle`] per consumer so the binary can await
/// graceful shutdown.
pub(crate) fn spawn_all(
    env: ConsumerRunnerEnvironment,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::with_capacity(5);

    let block_summary_source = env.block_source.clone();
    handles.push(spawn_chain_events_consumer(
        ChainEventsConsumerSpec {
            label: "BlockSummary",
            consumer_name: BLOCK_SUMMARY_CONSUMER_NAME,
        },
        env.clone(),
        cancel.clone(),
        move || BlockSummaryConsumer::new(block_summary_source.clone()),
    ));

    let transaction_fees_source = env.block_source.clone();
    handles.push(spawn_chain_events_consumer(
        ChainEventsConsumerSpec {
            label: "TransactionFees",
            consumer_name: TRANSACTION_FEES_CONSUMER_NAME,
        },
        env.clone(),
        cancel.clone(),
        move || TransactionFeesConsumer::new(transaction_fees_source.clone()),
    ));

    let recent_transactions_source = env.block_source.clone();
    handles.push(spawn_chain_events_consumer(
        ChainEventsConsumerSpec {
            label: "RecentTransactions",
            consumer_name: RECENT_TRANSACTIONS_CONSUMER_NAME,
        },
        env.clone(),
        cancel.clone(),
        move || RecentTransactionsConsumer::new(recent_transactions_source.clone()),
    ));

    let address_activity_source = env.block_source.clone();
    handles.push(spawn_chain_events_consumer(
        ChainEventsConsumerSpec {
            label: "TransparentAddressActivity",
            consumer_name: TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME,
        },
        env.clone(),
        cancel.clone(),
        move || TransparentAddressActivityConsumer::new(address_activity_source.clone()),
    ));

    handles.push(spawn_mempool_events_consumer(
        MempoolEventsConsumerSpec {
            label: "MempoolEventCounts",
            consumer_name: MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
        },
        env,
        cancel,
        MempoolEventCountsConsumer::new,
    ));

    handles
}

#[derive(Clone, Copy)]
struct ChainEventsConsumerSpec {
    label: &'static str,
    consumer_name: zinder_explorer::DeriveConsumerName,
}

#[derive(Clone, Copy)]
struct MempoolEventsConsumerSpec {
    label: &'static str,
    consumer_name: zinder_explorer::DeriveConsumerName,
}

fn spawn_chain_events_consumer<C, F>(
    spec: ChainEventsConsumerSpec,
    env: ConsumerRunnerEnvironment,
    cancel: CancellationToken,
    build_consumer: F,
) -> JoinHandle<()>
where
    C: DeriveConsumer + 'static,
    F: Fn() -> C + Send + Sync + 'static,
{
    let build_consumer = Arc::new(build_consumer);
    tokio::spawn(async move {
        run_with_reconnect(
            spec.label,
            cancel.clone(),
            move |env_ref, cancel_ref| {
                let build = Arc::clone(&build_consumer);
                let env_ref = env_ref.clone();
                let cancel_ref = cancel_ref.clone();
                async move {
                    dispatch_chain_events_once(
                        spec.consumer_name,
                        &env_ref,
                        &cancel_ref,
                        build.as_ref()(),
                    )
                    .await
                }
            },
            env,
        )
        .await;
    })
}

fn spawn_mempool_events_consumer<C, F>(
    spec: MempoolEventsConsumerSpec,
    env: ConsumerRunnerEnvironment,
    cancel: CancellationToken,
    build_consumer: F,
) -> JoinHandle<()>
where
    C: DeriveMempoolConsumer + 'static,
    F: Fn() -> C + Send + Sync + 'static,
{
    let build_consumer = Arc::new(build_consumer);
    tokio::spawn(async move {
        run_with_reconnect(
            spec.label,
            cancel.clone(),
            move |env_ref, cancel_ref| {
                let build = Arc::clone(&build_consumer);
                let env_ref = env_ref.clone();
                let cancel_ref = cancel_ref.clone();
                async move {
                    dispatch_mempool_events_once(
                        spec.consumer_name,
                        &env_ref,
                        &cancel_ref,
                        build.as_ref()(),
                    )
                    .await
                }
            },
            env,
        )
        .await;
    })
}

async fn run_with_reconnect<DispatchFn, DispatchFut>(
    label: &'static str,
    cancel: CancellationToken,
    dispatch_once: DispatchFn,
    env: ConsumerRunnerEnvironment,
) where
    DispatchFn:
        Fn(&ConsumerRunnerEnvironment, &CancellationToken) -> DispatchFut + Send + Sync + 'static,
    DispatchFut: Future<Output = Result<(), ConsumerRunnerError>> + Send + 'static,
{
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let outcome = dispatch_once(&env, &cancel).await;
        if cancel.is_cancelled() {
            break;
        }
        match outcome {
            Ok(()) => {
                // Chain-events and mempool-events streams are server-streams
                // intended to deliver indefinitely. A clean close from the
                // server means the retained buffer ran out; we reconnect so
                // the consumer picks up subsequent events. Cancellation is
                // handled by the `cancel.is_cancelled()` check above.
                tracing::debug!(
                    target: "zinder::explorer",
                    event = "derive_consumer_resubscribe",
                    consumer = label,
                    "derive consumer stream closed cleanly; resubscribing after backoff"
                );
                tokio::select! {
                    () = sleep(RECONNECT_BACKOFF) => {}
                    () = cancel.cancelled() => break,
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::explorer",
                    event = "derive_consumer_reconnect",
                    consumer = label,
                    error = %error,
                    "derive consumer stream errored; reconnecting after backoff"
                );
                tokio::select! {
                    () = sleep(RECONNECT_BACKOFF) => {}
                    () = cancel.cancelled() => break,
                }
            }
        }
    }
}

async fn dispatch_chain_events_once<C: DeriveConsumer>(
    consumer_name: zinder_explorer::DeriveConsumerName,
    env: &ConsumerRunnerEnvironment,
    cancel: &CancellationToken,
    mut consumer: C,
) -> Result<(), ConsumerRunnerError> {
    let mut wallet_client = WalletQueryClient::new(env.wallet_channel.clone());
    let cursor_bytes = env
        .store
        .get_cursor(consumer_name)
        .map_err(|error| ConsumerRunnerError::Setup(error.to_string()))?
        .unwrap_or_default();
    let stream = tokio::time::timeout(
        SUBSCRIBE_TIMEOUT,
        wallet_client.chain_events(Request::new(ChainEventsRequest {
            from_cursor: cursor_bytes,
            family: WireChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })),
    )
    .await
    .map_err(|_| {
        ConsumerRunnerError::Subscribe(format!(
            "chain_events subscribe exceeded {SUBSCRIBE_TIMEOUT:?}"
        ))
    })?
    .map_err(|status| ConsumerRunnerError::Subscribe(status.to_string()))?
    .into_inner();
    tokio::select! {
        outcome = run_chain_events_subscriber(&mut consumer, &env.store, stream) => {
            outcome
                .map(|_| ())
                .map_err(|error: DeriveError| ConsumerRunnerError::Subscriber(error.to_string()))
        }
        () = cancel.cancelled() => Ok(()),
    }
}

async fn dispatch_mempool_events_once<C: DeriveMempoolConsumer>(
    consumer_name: zinder_explorer::DeriveConsumerName,
    env: &ConsumerRunnerEnvironment,
    cancel: &CancellationToken,
    mut consumer: C,
) -> Result<(), ConsumerRunnerError> {
    let mut wallet_client = WalletQueryClient::new(env.wallet_channel.clone());
    let cursor_bytes = env
        .store
        .get_cursor(consumer_name)
        .map_err(|error| ConsumerRunnerError::Setup(error.to_string()))?
        .unwrap_or_default();
    let stream = tokio::time::timeout(
        SUBSCRIBE_TIMEOUT,
        wallet_client.mempool_events(Request::new(MempoolEventsRequest {
            from_cursor: cursor_bytes,
            family: WireMempoolEventStreamFamily::Mempool as i32,
        })),
    )
    .await
    .map_err(|_| {
        ConsumerRunnerError::Subscribe(format!(
            "mempool_events subscribe exceeded {SUBSCRIBE_TIMEOUT:?}"
        ))
    })?
    .map_err(|status| ConsumerRunnerError::Subscribe(status.to_string()))?
    .into_inner();
    tokio::select! {
        outcome = run_mempool_events_subscriber(&mut consumer, &env.store, stream) => {
            outcome
                .map(|_| ())
                .map_err(|error: DeriveError| ConsumerRunnerError::Subscriber(error.to_string()))
        }
        () = cancel.cancelled() => Ok(()),
    }
}

async fn probe_prevouts_capability(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<bool, tonic::Status> {
    let response = wallet_client
        .server_info(Request::new(ServerInfoRequest {}))
        .await?
        .into_inner();
    Ok(response
        .info
        .and_then(|server_info| server_info.common)
        .is_some_and(|common| {
            common
                .capabilities
                .iter()
                .any(|cap| cap == zinder_proto::capabilities::WALLET_READ_TRANSPARENT_PREVOUTS_V1)
        }))
}

/// Errors a consumer task can raise before triggering a reconnect.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConsumerRunnerError {
    /// Failed to dial the configured `WalletQuery` endpoint.
    #[error("derive consumer connect failed: {0}")]
    Connect(String),
    /// Failed to subscribe to the chain-events or mempool-events stream.
    #[error("derive consumer subscribe failed: {0}")]
    Subscribe(String),
    /// Subscriber loop returned an error.
    #[error("derive consumer subscriber failed: {0}")]
    Subscriber(String),
    /// Per-task setup failed (cursor read, etc).
    #[error("derive consumer setup failed: {0}")]
    Setup(String),
}

impl From<BlockCommitContextError> for ConsumerRunnerError {
    fn from(error: BlockCommitContextError) -> Self {
        Self::Subscriber(error.to_string())
    }
}
