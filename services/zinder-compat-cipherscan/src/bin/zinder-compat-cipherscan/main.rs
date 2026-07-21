//! Cipherscan-compatible REST adapter entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use zinder_compat_cipherscan::{CipherscanRealtime, CipherscanRealtimeMode, CipherscanRestAdapter};
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_runtime::{
    AuthenticatedChannel, Readiness, ReadinessState, RuntimeService, StartupPhase,
    StartupPhaseGuard, cancel_on_terminating_signal, connect_zinder_grpc,
    install_tracing_subscriber, spawn_ops_endpoint_for,
};

mod config;
mod upstream_contract;

#[derive(Debug)]
struct TrackedStartApiPhase {
    guard: Option<StartupPhaseGuard>,
}

impl TrackedStartApiPhase {
    fn start() -> Self {
        Self {
            guard: Some(StartupPhase::StartApi.start()),
        }
    }

    fn record<T, E>(&mut self, outcome: Result<T, E>) -> Result<T, E>
    where
        E: std::fmt::Display,
    {
        if let Err(error) = &outcome
            && let Some(guard) = self.guard.take()
        {
            guard.fail(error);
        }
        outcome
    }

    fn complete(mut self) {
        if let Some(guard) = self.guard.take() {
            guard.complete();
        }
    }
}

use config::{CipherscanConfig, CipherscanConfigError, CipherscanConfigOverrides};

#[derive(Parser)]
#[command(name = "zinder-compat-cipherscan")]
#[command(about = "Cipherscan-compatible REST adapter over Zinder native gRPC services")]
struct Cli {
    /// TOML configuration file loaded before environment variables and CLI overrides.
    #[arg(long = "config", global = true)]
    config_path: Option<PathBuf>,
    /// Print the resolved command configuration without opening network listeners.
    #[arg(long = "print-config", global = true)]
    print_config: bool,
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Cipherscan-compatible REST listen address, such as 127.0.0.1:9070.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
    /// Zinder `ExplorerQuery` gRPC endpoint backing Cipherscan REST reads.
    #[arg(long = "explorer-query-endpoint")]
    explorer_query_endpoint: Option<String>,
    /// Zinder `WalletQuery` gRPC endpoint backing Cipherscan REST reads and broadcast.
    #[arg(long = "wallet-query-endpoint")]
    wallet_query_endpoint: Option<String>,
    /// External endpoint serving the current ZEC/USD market price.
    #[arg(long = "current-price-endpoint")]
    current_price_endpoint: Option<String>,
    /// External historical ZEC/USD endpoint template containing `{date}`.
    #[arg(long = "historical-price-endpoint-template")]
    historical_price_endpoint_template: Option<String>,
    /// Path to a file containing the shared-secret bearer token used by Zinder gRPC services.
    #[arg(long = "bearer-token-path")]
    bearer_token_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    install_tracing_subscriber();

    if cli.print_config {
        return run_print_config(cli);
    }

    run_runtime(cli).await
}

#[allow(
    clippy::print_stdout,
    reason = "--print-config is a structured TOML data dump, not a log event"
)]
fn run_print_config(cli: Cli) -> ExitCode {
    let config_path = cli.config_path.clone();
    let render_result = config::load_cipherscan_config(config_path, cli.into())
        .and_then(|cipherscan_config| config::cipherscan_config_toml(&cipherscan_config));

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    match run_cipherscan_adapter(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_cipherscan_adapter(cli: Cli) -> Result<(), CipherscanConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let config_path = cli.config_path.clone();
    let cipherscan_config = match config::load_cipherscan_config(config_path, cli.into()) {
        Ok(config) => {
            load_config_phase.complete();
            config
        }
        Err(error) => {
            load_config_phase.fail(&error);
            return Err(error);
        }
    };

    let readiness = Readiness::default();
    readiness.set(ReadinessState::starting());
    let mut start_api_phase = TrackedStartApiPhase::start();
    let ops_handle = spawn_ops_endpoint_for(
        RuntimeService::CompatCipherscan,
        cipherscan_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(cipherscan_config.network),
        readiness.clone(),
        Vec::new(),
    );

    let (explorer_channel, wallet_channel, upstream_admission) =
        start_api_phase.record(connect_and_preflight_upstreams(&cipherscan_config).await)?;
    let listener_result = TcpListener::bind(cipherscan_config.listen_addr)
        .await
        .map_err(|source| CipherscanConfigError::Bind {
            listen_addr: cipherscan_config.listen_addr,
            source,
        });
    let listener = start_api_phase.record(listener_result)?;

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let adapter_result = CipherscanRestAdapter::new(
        cipherscan_config.network,
        explorer_channel,
        wallet_channel,
        cipherscan_config.market_price_endpoints.clone(),
        CipherscanRealtime::new(
            realtime_mode(upstream_admission.realtime_websocket_enabled),
            cancel.clone(),
        ),
    )
    .map_err(CipherscanConfigError::from);
    let adapter = start_api_phase.record(adapter_result)?;
    let app = adapter.clone().router();

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    readiness.set(ReadinessState::ready(None));

    tracing::info!(
        target: "zinder::compat::cipherscan",
        event = "cipherscan_adapter_started",
        network = encode_zinder_native_chain_name(cipherscan_config.network),
        listen_addr = %cipherscan_config.listen_addr,
        explorer_query_endpoint = %cipherscan_config.explorer_query_endpoint,
        wallet_query_endpoint = %cipherscan_config.wallet_query_endpoint,
        current_price_endpoint = %cipherscan_config.market_price_endpoints.current,
        historical_price_endpoint_template = %cipherscan_config.market_price_endpoints.historical_template,
        "Cipherscan-compatible REST adapter started"
    );

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await;
    adapter.shutdown_realtime().await;

    tracing::info!(
        target: "zinder::compat::cipherscan",
        event = "cipherscan_adapter_stopped",
        "Cipherscan-compatible REST adapter stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    serve_result.map_err(CipherscanConfigError::Serve)
}

const fn realtime_mode(is_enabled: bool) -> CipherscanRealtimeMode {
    if is_enabled {
        CipherscanRealtimeMode::Full
    } else {
        CipherscanRealtimeMode::Unavailable
    }
}

async fn connect_and_preflight_upstreams(
    config: &CipherscanConfig,
) -> Result<
    (
        AuthenticatedChannel,
        AuthenticatedChannel,
        upstream_contract::UpstreamAdmission,
    ),
    CipherscanConfigError,
> {
    let explorer_channel = connect_zinder_grpc(
        &config.explorer_query_endpoint,
        config.bearer_token.as_ref(),
    )
    .await?;
    let wallet_channel =
        connect_zinder_grpc(&config.wallet_query_endpoint, config.bearer_token.as_ref()).await?;
    let admission = upstream_contract::preflight_upstream_contract_pair(
        config.network,
        explorer_channel.clone(),
        wallet_channel.clone(),
    )
    .await?;
    Ok((explorer_channel, wallet_channel, admission))
}

fn emit_runtime_error(error: &CipherscanConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::compat::cipherscan",
        event = "cipherscan_adapter_run_failed",
        error = %error,
        "Cipherscan-compatible REST adapter failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for CipherscanConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
            explorer_query_endpoint: cli.explorer_query_endpoint,
            wallet_query_endpoint: cli.wallet_query_endpoint,
            current_price_endpoint: cli.current_price_endpoint,
            historical_price_endpoint_template: cli.historical_price_endpoint_template,
            bearer_token_path: cli.bearer_token_path,
        }
    }
}

#[cfg(test)]
mod startup_phase_tests {
    use std::{io, net::SocketAddr};

    use zinder_compat_cipherscan::MarketPriceInitializationError;
    use zinder_testkit::LogCapture;

    use super::{CipherscanConfigError, TrackedStartApiPhase};

    #[test]
    fn listener_bind_failure_marks_start_api_failed() {
        let capture = LogCapture::install_for_target("zinder::startup");
        let mut phase = TrackedStartApiPhase::start();
        let listen_addr = SocketAddr::from(([127, 0, 0, 1], 9070));
        let error = CipherscanConfigError::Bind {
            listen_addr,
            source: io::Error::new(io::ErrorKind::AddrInUse, "address is already in use"),
        };

        let outcome: Result<(), _> = phase.record(Err(error));

        assert!(matches!(outcome, Err(CipherscanConfigError::Bind { .. })));
        assert_failed_start_api_exit(&capture);
    }

    #[test]
    fn adapter_construction_failure_marks_start_api_failed() {
        let capture = LogCapture::install_for_target("zinder::startup");
        let mut phase = TrackedStartApiPhase::start();
        let error = CipherscanConfigError::MarketPriceClient(
            MarketPriceInitializationError::InvalidHistoricalEndpointTemplate,
        );

        let outcome: Result<(), _> = phase.record(Err(error));

        assert!(matches!(
            outcome,
            Err(CipherscanConfigError::MarketPriceClient(_))
        ));
        assert_failed_start_api_exit(&capture);
    }

    fn assert_failed_start_api_exit(capture: &LogCapture) {
        let events = capture.events();
        assert_eq!(events.len(), 2, "StartApi must emit entry and exit events");
        let exit = &events[1];
        assert_eq!(exit.field("phase_state"), Some("exit"));
        assert_eq!(exit.field("phase"), Some("start_api"));
        assert_eq!(exit.field("outcome"), Some("failed"));
        assert!(exit.field("reason").is_some());
    }
}
