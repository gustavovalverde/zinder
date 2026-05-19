//! Construction and keep-alive policy for every long-lived HTTP/gRPC
//! client this crate opens to a Zebra full node.
//!
//! This module is the canonical home for the contract Zinder enforces
//! when talking to Zebra: which keep-alive settings each protocol
//! receives, which knobs are deliberately *not* set (and why), and how
//! callers convert a transport-layer failure into the
//! [`crate::SourceError`] vocabulary the upstream-source contract uses.
//!
//! Direct construction of [`jsonrpsee::http_client::HttpClientBuilder`],
//! [`tonic::transport::Endpoint::from_shared`], or
//! [`reqwest::Client::builder`] outside this module is a forbidden
//! pattern; the `transport_invariants.rs` structural test rejects new
//! call sites. New Zebra-facing adapters add a factory here and consume
//! it.
//!
//! See [ADR-0019](../../../docs/adrs/0019-transport-policy-ownership.md)
//! for the boundary and policy decisions this module enforces.

use std::num::NonZeroU64;
use std::time::Duration;

use jsonrpsee::http_client::{HeaderMap, HttpClient, HttpClientBuilder};
use thiserror::Error;
use tonic::transport::{Channel, Endpoint};

use crate::ZebraIndexerSourceTarget;

/// HTTP/2 ping interval on Zebra Indexer gRPC channels.
///
/// The Indexer's two server streams (`MempoolChange`, `ChainTipChange`)
/// can go quiet between mined blocks (~75 s on testnet). A 30 s ping
/// gives the kernel a chance to surface a dead peer well within the
/// typical NAT timeout window. We deliberately omit
/// `keep_alive_while_idle(true)` to avoid tonic issue #258
/// (`ENHANCE_YOUR_CALM` from servers that don't permit
/// keep-alive-without-calls); the streams are always active, so pings
/// flow regardless. See ADR-0019.
const ZEBRA_INDEXER_HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time a single HTTP/2 keep-alive ping waits for an ACK before
/// the channel treats the connection as dead.
const ZEBRA_INDEXER_HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// TCP keep-alive probe interval. Catches dead-peer cases the HTTP/2
/// layer's PING/PONG cannot detect on every platform.
const ZEBRA_INDEXER_TCP_KEEPALIVE: Duration = Duration::from_secs(45);

/// Failure modes a [`build_zebra_json_rpc_client`] or
/// [`connect_zebra_indexer_channel`] call can raise.
///
/// Adapters translate this at the boundary into the
/// [`crate::SourceError`] variant their `Source*Stream` contract
/// expects.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ZebraTransportError {
    /// The endpoint URL could not be parsed.
    #[error("Zebra endpoint URL is invalid: {0}")]
    InvalidEndpoint(String),
    /// The transport could not connect to the endpoint.
    #[error("Zebra transport connect failed: {0}")]
    ConnectFailed(String),
    /// The HTTP/JSON-RPC client could not be built (header or builder error).
    #[error("Zebra HTTP client build failed: {0}")]
    ClientBuildFailed(String),
}

/// Builds a jsonrpsee HTTP client for a Zebra full node's JSON-RPC.
///
/// jsonrpsee 0.26 deliberately hides the hyper connection-pool config
/// from `HttpClientBuilder`. To compensate, callers should:
///
/// - Set a tight `request_timeout` so hangs fail fast (the reconnect
///   loop above the client treats the resulting error as a transport
///   failure and rebuilds the client through `ResilientClient<C>`).
/// - Avoid sharing a single client across long network-availability
///   events. The `ResilientClient` wrapper enforces this; this function
///   produces the client it wraps.
///
/// `headers` typically carries a single `authorization` entry; pass an
/// empty `HeaderMap` for unauthenticated endpoints.
pub fn build_zebra_json_rpc_client(
    json_rpc_addr: &str,
    request_timeout: Duration,
    max_response_bytes: NonZeroU64,
    headers: HeaderMap,
) -> Result<HttpClient, ZebraTransportError> {
    let max_response_size = u32::try_from(max_response_bytes.get()).unwrap_or(u32::MAX);
    HttpClientBuilder::default()
        .request_timeout(request_timeout)
        .max_response_size(max_response_size)
        .set_headers(headers)
        .build(json_rpc_addr)
        .map_err(|error| ZebraTransportError::ClientBuildFailed(error.to_string()))
}

/// Runtime options for [`connect_zebra_indexer_channel`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZebraIndexerChannelOptions {
    /// Connect timeout for the indexer endpoint.
    pub connect_timeout: Duration,
    /// Per-RPC request timeout. The Indexer's two streams are
    /// long-lived, so adapters typically set this to a generous
    /// duration (≥ 1 minute).
    pub request_timeout: Duration,
}

/// Opens a long-lived gRPC channel to a Zebra Indexer endpoint.
///
/// The returned [`Channel`] carries the HTTP/2 + TCP keep-alive policy
/// mandated by ADR-0019. Callers wrap it in the generated Indexer
/// client (`IndexerClient::new(channel)`) and hold the channel through
/// stream re-subscribes; the `ResilientClient<C>` wrapper rebuilds the
/// channel after N consecutive transport errors.
pub async fn connect_zebra_indexer_channel(
    target: &ZebraIndexerSourceTarget,
    options: ZebraIndexerChannelOptions,
) -> Result<Channel, ZebraTransportError> {
    let endpoint = Endpoint::from_shared(target.endpoint_url.clone())
        .map_err(|error| ZebraTransportError::InvalidEndpoint(error.to_string()))?
        .connect_timeout(options.connect_timeout)
        .timeout(options.request_timeout)
        .http2_keep_alive_interval(ZEBRA_INDEXER_HTTP2_KEEPALIVE_INTERVAL)
        .keep_alive_timeout(ZEBRA_INDEXER_HTTP2_KEEPALIVE_TIMEOUT)
        .tcp_keepalive(Some(ZEBRA_INDEXER_TCP_KEEPALIVE));
    endpoint
        .connect()
        .await
        .map_err(|error| ZebraTransportError::ConnectFailed(error.to_string()))
}
