//! Zebra JSON-RPC source adapter.

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{
    fs,
    num::{NonZeroU32, NonZeroU64},
    path::Path,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use jsonrpsee::core::ClientError;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ArrayParams;
use jsonrpsee::http_client::{HeaderMap, HeaderValue, HttpClient, HttpClientBuilder};
use jsonrpsee::types::ErrorObjectOwned;
use parking_lot::Mutex;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, BroadcastAccepted, BroadcastDuplicate,
    BroadcastInvalidEncoding, BroadcastRejected, BroadcastUnknown, Network, RawTransactionBytes,
    ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex, TransactionBroadcastResult, TransactionId,
};

use crate::{
    NodeAuth, NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceChainCheckpoint,
    SourceError, SourceNetworkUpgradeHeights, SourceSubtreeRoot, SourceSubtreeRoots,
    TransactionBroadcaster, decode_display_block_hash, source_block::decode_display_hash_32,
};

/// Default capability set assumed for Zebra JSON-RPC sources.
///
/// Used until [`ZebraJsonRpcSource::probe_capabilities`] runs. Operators
/// should treat this as a baseline; the probed value is the source of truth
/// at runtime.
fn default_zebra_capabilities() -> NodeCapabilities {
    NodeCapabilities::from_trusted([
        NodeCapability::JsonRpc,
        NodeCapability::BestChainBlocks,
        NodeCapability::TipId,
        NodeCapability::TreeState,
        NodeCapability::SubtreeRoots,
        NodeCapability::TransactionBroadcast,
    ])
}

/// Default maximum JSON-RPC response body size.
pub const DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES: NonZeroU64 =
    NonZeroU64::MIN.saturating_add((16 * 1024 * 1024) - 1);

/// JSON-RPC warming-up error code returned by Zcash nodes while syncing.
const JSON_RPC_WARMING_UP_CODE: i32 = -28;
/// JSON-RPC error code returned for invalid transaction encodings.
const JSON_RPC_INVALID_ENCODING_CODE: i32 = -22;
/// JSON-RPC error code returned for transactions already in the mempool.
const JSON_RPC_DUPLICATE_TRANSACTION_CODE: i32 = -27;

/// Node source backed by Zebra's JSON-RPC API.
#[derive(Clone)]
pub struct ZebraJsonRpcSource {
    network: Network,
    client: HttpClient,
    cached_capabilities: Arc<Mutex<NodeCapabilities>>,
}

/// Runtime options for [`ZebraJsonRpcSource`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZebraJsonRpcSourceOptions {
    /// Maximum total time spent on one JSON-RPC request.
    pub request_timeout: Duration,
    /// Maximum JSON-RPC response body size accepted from the node.
    pub max_response_bytes: NonZeroU64,
}

impl Default for ZebraJsonRpcSourceOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_response_bytes: DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        }
    }
}

impl ZebraJsonRpcSource {
    /// Returns the static baseline capability set Zebra JSON-RPC sources
    /// are assumed to support before runtime discovery runs.
    ///
    /// Production code should prefer [`ZebraJsonRpcSource::probe_capabilities`]
    /// (which writes the discovered set into the per-source cache); this
    /// constant exists for tests and for compile-time defaults.
    #[must_use]
    pub fn baseline_capabilities() -> NodeCapabilities {
        default_zebra_capabilities()
    }

    /// Fetches the chain checkpoint (block hash and commitment-tree sizes)
    /// at `height` from the node.
    ///
    /// This is the data Zinder needs to bootstrap canonical storage from a
    /// recent height instead of replaying the chain from genesis. The values
    /// come from Zebra's `getblock` RPC at verbosity 1, which exposes
    /// `trees.{sapling,orchard}.size` for every height.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::BlockUnavailable`] when the node does not
    /// have the requested height (e.g. because it is still syncing) and
    /// [`SourceError::SourceProtocolMismatch`] when the response shape is
    /// missing the expected `trees` field. Capability discovery
    /// ([`ZebraJsonRpcSource::probe_capabilities`]) does not gate this call;
    /// older Zebra deployments without `trees` in `getblock` will surface a
    /// protocol mismatch the first time a checkpoint is requested.
    pub async fn fetch_chain_checkpoint(
        &self,
        height: BlockHeight,
    ) -> Result<SourceChainCheckpoint, SourceError> {
        let block_unavailable = |error: JsonRpcCallError| SourceError::BlockUnavailable {
            height,
            is_retryable: error.is_retryable(),
            reason: error.message,
        };

        let block_hash_hex: String = self
            .call_typed(
                "getblockhash",
                positional_params([Value::from(height.value())])?,
                block_unavailable,
            )
            .await?;
        let block_response: ZebraGetBlockTrees = self
            .call_typed(
                "getblock",
                positional_params([Value::from(block_hash_hex.clone()), Value::from(1)])?,
                block_unavailable,
            )
            .await?;
        if block_response.height != height.value() {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "getblock height does not match requested checkpoint height",
            });
        }
        let trees = block_response
            .trees
            .ok_or(SourceError::SourceProtocolMismatch {
                reason: "getblock response is missing the trees field; \
                         Zinder checkpoint bootstrap requires Zebra >= 2.x",
            })?;
        let sapling = trees.sapling.size;
        let orchard = trees.orchard.size;
        let sapling_size =
            u32::try_from(sapling).map_err(|_| SourceError::SourceProtocolMismatch {
                reason: "sapling commitment tree size does not fit u32",
            })?;
        let orchard_size =
            u32::try_from(orchard).map_err(|_| SourceError::SourceProtocolMismatch {
                reason: "orchard commitment tree size does not fit u32",
            })?;
        let block_hash = decode_display_block_hash(&block_hash_hex)?;
        Ok(SourceChainCheckpoint::new(
            height,
            zinder_core::BlockHash::from_bytes(block_hash.as_bytes()),
            zinder_core::ChainTipMetadata::new(sapling_size, orchard_size),
        ))
    }

    /// Fetches node-advertised network upgrade activation heights.
    ///
    /// The values come from Zebra's `getblockchaininfo.upgrades` field so
    /// custom Testnet and Regtest activation heights stay node-owned.
    pub async fn fetch_network_upgrade_activation_heights(
        &self,
    ) -> Result<SourceNetworkUpgradeHeights, SourceError> {
        let blockchain_info: ZebraGetBlockchainInfoUpgrades = self
            .call_typed("getblockchaininfo", ArrayParams::new(), |error| {
                SourceError::NodeUnavailable {
                    is_retryable: error.is_retryable(),
                    reason: error.message,
                }
            })
            .await?;

        Ok(SourceNetworkUpgradeHeights::new(
            activation_height_by_name(&blockchain_info.upgrades, "sapling"),
            activation_height_by_name(&blockchain_info.upgrades, "nu5"),
        ))
    }

    /// Probes the node's `rpc.discover` (`OpenRPC`) endpoint and updates
    /// the cached capability set returned by
    /// [`NodeSource::capabilities`].
    ///
    /// On probe success, the cache reflects the methods the node
    /// advertises plus [`NodeCapability::JsonRpc`] and
    /// [`NodeCapability::OpenRpcDiscovery`].
    ///
    /// On probe failure (older Zebra without `rpc.discover`, transient
    /// transport error), the cache falls back to the baseline capability set.
    /// Operators that require strict capability discovery can pair this call
    /// with a [`NodeCapabilities::supports`] check against
    /// [`NodeCapability::OpenRpcDiscovery`].
    pub async fn probe_capabilities(&self) -> Result<NodeCapabilities, SourceError> {
        let started_at = Instant::now();
        let response_body = self
            .client
            .request::<Value, _>("rpc.discover", ArrayParams::new())
            .await;
        record_json_rpc_client_result("rpc.discover", started_at, &response_body);

        let probed_capabilities = match response_body {
            Ok(openrpc_response) => parse_openrpc_capabilities(&openrpc_response),
            Err(probe_error) => {
                tracing::warn!(
                    target: "zinder::source",
                    event = "node_capability_probe_fallback",
                    reason = %probe_error,
                    "node does not expose rpc.discover; falling back to trusted defaults"
                );
                default_zebra_capabilities()
            }
        };

        *self.cached_capabilities.lock() = probed_capabilities;
        Ok(probed_capabilities)
    }

    /// Creates a Zebra JSON-RPC source.
    pub fn new(
        network: Network,
        json_rpc_addr: impl Into<String>,
        node_auth: NodeAuth,
        request_timeout: Duration,
    ) -> Result<Self, SourceError> {
        Self::with_options(
            network,
            json_rpc_addr,
            node_auth,
            ZebraJsonRpcSourceOptions {
                request_timeout,
                ..ZebraJsonRpcSourceOptions::default()
            },
        )
    }

    /// Creates a Zebra JSON-RPC source with explicit runtime options.
    pub fn with_options(
        network: Network,
        json_rpc_addr: impl Into<String>,
        node_auth: NodeAuth,
        options: ZebraJsonRpcSourceOptions,
    ) -> Result<Self, SourceError> {
        let authorization = match node_auth {
            NodeAuth::None => None,
            NodeAuth::Basic { username, password } => {
                Some(basic_authorization_header(&username, &password))
            }
            NodeAuth::Cookie { path } => Some(cookie_authorization_header(&path)?),
        };

        let client = build_http_client(
            &json_rpc_addr.into(),
            authorization,
            options.request_timeout,
            options.max_response_bytes,
        )?;

        Ok(Self {
            network,
            client,
            cached_capabilities: Arc::new(Mutex::new(default_zebra_capabilities())),
        })
    }

    async fn call_typed<Response>(
        &self,
        method: &'static str,
        params: ArrayParams,
        map_call_error: impl FnOnce(JsonRpcCallError) -> SourceError,
    ) -> Result<Response, SourceError>
    where
        Response: for<'de> Deserialize<'de>,
    {
        let started_at = Instant::now();
        let rpc_outcome = match self.client.request::<Response, _>(method, params).await {
            Ok(response) => Ok(response),
            Err(ClientError::Call(error)) => Err(map_call_error(JsonRpcCallError::from(error))),
            Err(error) => Err(map_transport_error(&error)),
        };
        record_json_rpc_source_outcome(method, started_at, &rpc_outcome);

        rpc_outcome
    }
}

#[async_trait]
impl NodeSource for ZebraJsonRpcSource {
    fn capabilities(&self) -> NodeCapabilities {
        *self.cached_capabilities.lock()
    }

    async fn fetch_block_by_height(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        let block_unavailable = |error: JsonRpcCallError| SourceError::BlockUnavailable {
            height,
            is_retryable: error.is_retryable(),
            reason: error.message,
        };

        let anchor_block_hash: String = self
            .call_typed(
                "getblockhash",
                positional_params([Value::from(height.value())])?,
                block_unavailable,
            )
            .await?;
        let anchor_block_hash_bytes = decode_display_block_hash(&anchor_block_hash)?;
        let header: ZebraBlockHeader = self
            .call_typed(
                "getblockheader",
                positional_params([Value::from(anchor_block_hash.clone()), Value::from(true)])?,
                block_unavailable,
            )
            .await?;

        if header.height != height.value() {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "block header height does not match requested height",
            });
        }
        if decode_display_block_hash(&header.hash)? != anchor_block_hash_bytes {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "block header hash does not match height lookup hash",
            });
        }

        let raw_block_hex: String = self
            .call_typed(
                "getblock",
                positional_params([Value::from(anchor_block_hash.clone()), Value::from(0)])?,
                block_unavailable,
            )
            .await?;
        let raw_block_bytes = hex::decode(raw_block_hex)
            .map_err(|source| SourceError::InvalidRawBlockHex { source })?;
        let source_block =
            SourceBlock::from_raw_block_bytes(self.network, height, raw_block_bytes)?;
        validate_zebra_header(&source_block, &header)?;

        let tree_state: Value = self
            .call_typed(
                "z_gettreestate",
                positional_params([Value::from(anchor_block_hash)])?,
                block_unavailable,
            )
            .await?;
        validate_zebra_tree_state(&tree_state, height, source_block.hash)?;
        // Store the logical tree-state response, not Zebra's original JSON
        // bytes. Do not use this payload as hash or signature material.
        let tree_state_payload = serde_json::to_vec(&tree_state)
            .map_err(|source| SourceError::SourcePayloadEncodingFailed { source })?;

        Ok(source_block.with_tree_state_payload_bytes(tree_state_payload))
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        let best_block_hash_hex: String = self
            .call_typed("getbestblockhash", ArrayParams::new(), map_node_unavailable)
            .await?;
        let best_block_hash = decode_display_block_hash(&best_block_hash_hex)?;
        let header: ZebraBlockHeader = self
            .call_typed(
                "getblockheader",
                positional_params([Value::from(best_block_hash_hex), Value::from(true)])?,
                map_node_unavailable,
            )
            .await?;
        if decode_display_block_hash(&header.hash)? != best_block_hash {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "best block header hash does not match tip hash",
            });
        }

        Ok(BlockId::new(
            BlockHeight::new(header.height),
            best_block_hash,
        ))
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        let subtree_response: ZebraSubtreeRootsByIndex = self
            .call_typed(
                "z_getsubtreesbyindex",
                positional_params([
                    Value::from(protocol.rpc_pool_name()),
                    Value::from(start_index.value()),
                    Value::from(max_entries.get()),
                ])?,
                |error| SourceError::SubtreeRootsUnavailable {
                    protocol,
                    start_index,
                    is_retryable: error.is_retryable(),
                    reason: error.message,
                },
            )
            .await?;

        if subtree_response.pool != protocol.rpc_pool_name() {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "subtree roots pool does not match requested protocol",
            });
        }

        if subtree_response.start_index != start_index.value() {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "subtree roots start index does not match requested index",
            });
        }

        let mut subtree_roots = Vec::with_capacity(subtree_response.subtrees.len());
        for (offset, subtree) in subtree_response.subtrees.into_iter().enumerate() {
            let offset =
                u32::try_from(offset).map_err(|_| SourceError::SourceProtocolMismatch {
                    reason: "subtree roots response has too many entries",
                })?;
            let subtree_index = start_index
                .value()
                .checked_add(offset)
                .map(SubtreeRootIndex::new)
                .ok_or(SourceError::SourceProtocolMismatch {
                    reason: "subtree roots response exceeds the SubtreeRootIndex range",
                })?;
            subtree_roots.push(SourceSubtreeRoot::new(
                subtree_index,
                decode_subtree_root_hash(&subtree.root)?,
                BlockHeight::new(subtree.end_height),
            ));
        }

        Ok(SourceSubtreeRoots::new(
            protocol,
            start_index,
            subtree_roots,
        ))
    }
}

#[async_trait]
impl TransactionBroadcaster for ZebraJsonRpcSource {
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, SourceError> {
        let raw_transaction_hex = hex::encode(raw_transaction.as_slice());
        let params = positional_params([Value::from(raw_transaction_hex)])?;

        let started_at = Instant::now();
        let response = self
            .client
            .request::<String, _>("sendrawtransaction", params)
            .await;
        record_json_rpc_client_result("sendrawtransaction", started_at, &response);

        match response {
            Ok(transaction_id_hex) => Ok(TransactionBroadcastResult::Accepted(BroadcastAccepted {
                transaction_id: decode_display_transaction_id(&transaction_id_hex)?,
            })),
            Err(ClientError::Call(error)) => {
                Ok(classify_broadcast_error(JsonRpcCallError::from(error)))
            }
            Err(error) => Err(map_transport_error(&error)),
        }
    }
}

fn map_node_unavailable(error: JsonRpcCallError) -> SourceError {
    SourceError::NodeUnavailable {
        is_retryable: error.is_retryable(),
        reason: error.message,
    }
}

fn record_json_rpc_source_outcome<Response>(
    method: &'static str,
    started_at: Instant,
    rpc_outcome: &Result<Response, SourceError>,
) {
    metrics::histogram!(
        "zinder_node_request_duration_seconds",
        "source" => "zebra_json_rpc",
        "method" => method,
        "status" => outcome_status(rpc_outcome),
        "error_class" => source_error_class(rpc_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_node_request_total",
        "source" => "zebra_json_rpc",
        "method" => method,
        "status" => outcome_status(rpc_outcome),
        "error_class" => source_error_class(rpc_outcome.as_ref().err())
    )
    .increment(1);
}

fn record_json_rpc_client_result<Response>(
    method: &'static str,
    started_at: Instant,
    rpc_outcome: &Result<Response, ClientError>,
) {
    metrics::histogram!(
        "zinder_node_request_duration_seconds",
        "source" => "zebra_json_rpc",
        "method" => method,
        "status" => outcome_status(rpc_outcome),
        "error_class" => client_error_class(rpc_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_node_request_total",
        "source" => "zebra_json_rpc",
        "method" => method,
        "status" => outcome_status(rpc_outcome),
        "error_class" => client_error_class(rpc_outcome.as_ref().err())
    )
    .increment(1);
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn source_error_class(error: Option<&SourceError>) -> &'static str {
    match error {
        None => "none",
        Some(SourceError::NodeUnavailable { .. }) => "node_unavailable",
        Some(SourceError::BlockUnavailable { .. }) => "block_unavailable",
        Some(SourceError::SubtreeRootsUnavailable { .. }) => "subtree_roots_unavailable",
        Some(SourceError::SourceProtocolMismatch { .. }) => "source_protocol_mismatch",
        Some(SourceError::SourcePayloadEncodingFailed { .. }) => "source_payload_encoding_failed",
        Some(SourceError::NodeCapabilityMissing { .. }) => "node_capability_missing",
        Some(SourceError::TransactionBroadcastDisabled) => "transaction_broadcast_disabled",
        Some(SourceError::UnsupportedNodeAuth { .. }) => "unsupported_node_auth",
        Some(
            SourceError::InvalidBlockHashHex { .. }
            | SourceError::InvalidRawBlockHex { .. }
            | SourceError::InvalidBlockHashLength { .. }
            | SourceError::InvalidTransactionIdHex { .. }
            | SourceError::InvalidTransactionIdLength { .. }
            | SourceError::InvalidSubtreeRootHex { .. }
            | SourceError::InvalidSubtreeRootLength { .. }
            | SourceError::RawBlockParseFailed { .. }
            | SourceError::RawBlockCoinbaseHeightMissing
            | SourceError::RawBlockHeightMismatch { .. }
            | SourceError::RawBlockTimeOutOfRange,
        ) => "source_decode_failed",
    }
}

fn client_error_class(error: Option<&ClientError>) -> &'static str {
    match error {
        None => "none",
        Some(ClientError::Call(_)) => "json_rpc_call_error",
        Some(_) => "transport_error",
    }
}

/// Parses an `OpenRPC` `rpc.discover` response into a
/// [`NodeCapabilities`] set.
///
/// The probe always grants [`NodeCapability::JsonRpc`] and
/// [`NodeCapability::OpenRpcDiscovery`] on success; remaining variants
/// are granted when the corresponding RPC methods appear in the response.
fn parse_openrpc_capabilities(openrpc_response: &Value) -> NodeCapabilities {
    let mut probed_capabilities = vec![NodeCapability::JsonRpc, NodeCapability::OpenRpcDiscovery];

    let method_names = openrpc_method_names(openrpc_response);
    if openrpc_supports_all_methods(
        &method_names,
        &["getblock", "getblockhash", "getblockheader"],
    ) {
        probed_capabilities.push(NodeCapability::BestChainBlocks);
    }
    if openrpc_supports_all_methods(&method_names, &["getbestblockhash", "getblockheader"]) {
        probed_capabilities.push(NodeCapability::TipId);
    }
    if method_names.contains(&"z_gettreestate") {
        probed_capabilities.push(NodeCapability::TreeState);
    }
    if method_names.contains(&"z_getsubtreesbyindex") {
        probed_capabilities.push(NodeCapability::SubtreeRoots);
    }
    if method_names.contains(&"sendrawtransaction") {
        probed_capabilities.push(NodeCapability::TransactionBroadcast);
    }

    NodeCapabilities::from_trusted(probed_capabilities)
}

fn openrpc_method_names(openrpc_response: &Value) -> Vec<&str> {
    openrpc_response
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|method| method.get("name").and_then(Value::as_str))
        .collect()
}

fn openrpc_supports_all_methods(method_names: &[&str], required_methods: &[&str]) -> bool {
    required_methods
        .iter()
        .all(|required_method| method_names.contains(required_method))
}

/// Builds the `Authorization: Basic ...` header value from node credentials.
fn basic_authorization_header(username: &str, password: &SecretString) -> String {
    let credentials = format!("{}:{}", username, password.expose_secret());
    basic_authorization_header_from_credentials(&credentials)
}

/// Builds the `Authorization: Basic ...` header value from a node cookie file.
fn cookie_authorization_header(path: &Path) -> Result<String, SourceError> {
    let cookie = fs::read_to_string(path).map_err(|_| SourceError::NodeUnavailable {
        is_retryable: false,
        reason: "node cookie file could not be read".to_owned(),
    })?;
    let credentials = cookie.trim();
    if credentials.is_empty() {
        return Err(SourceError::NodeUnavailable {
            is_retryable: false,
            reason: "node cookie file is empty".to_owned(),
        });
    }
    Ok(basic_authorization_header_from_credentials(credentials))
}

/// Builds the `Authorization: Basic ...` header value from raw `username:password` credentials.
fn basic_authorization_header_from_credentials(credentials: &str) -> String {
    format!("Basic {}", BASE64_STANDARD.encode(credentials))
}

/// Builds the configured jsonrpsee HTTP client used by the adapter.
fn build_http_client(
    json_rpc_addr: &str,
    authorization: Option<String>,
    request_timeout: Duration,
    max_response_bytes: NonZeroU64,
) -> Result<HttpClient, SourceError> {
    let mut headers = HeaderMap::new();
    if let Some(authorization) = authorization {
        let header_value =
            HeaderValue::from_str(&authorization).map_err(|_| SourceError::NodeUnavailable {
                is_retryable: false,
                reason: "node authorization header is not a valid HTTP header value".to_owned(),
            })?;
        headers.insert("authorization", header_value);
    }

    let max_response_size = u32::try_from(max_response_bytes.get()).unwrap_or(u32::MAX);

    HttpClientBuilder::default()
        .request_timeout(request_timeout)
        .max_response_size(max_response_size)
        .set_headers(headers)
        .build(json_rpc_addr)
        .map_err(|source| SourceError::NodeUnavailable {
            is_retryable: false,
            reason: source.to_string(),
        })
}

/// Builds positional JSON-RPC parameters from an iterator of pre-typed JSON values.
///
/// `ArrayParams::insert` returns an error only on serialization failure. Inputs
/// are already `serde_json::Value`, so the error path is logically unreachable;
/// any unexpected failure is surfaced through `SourcePayloadEncodingFailed`.
fn positional_params(
    param_values: impl IntoIterator<Item = Value>,
) -> Result<ArrayParams, SourceError> {
    let mut params = ArrayParams::new();
    for param_value in param_values {
        params
            .insert(param_value)
            .map_err(|source| SourceError::SourcePayloadEncodingFailed { source })?;
    }
    Ok(params)
}

fn validate_zebra_header(
    source_block: &SourceBlock,
    header: &ZebraBlockHeader,
) -> Result<(), SourceError> {
    let hash = decode_display_block_hash(&header.hash)?;
    if source_block.hash != hash {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "block header hash does not match raw block header",
        });
    }

    let parent_hash = decode_display_block_hash(header.previous_block_hash.as_deref().ok_or(
        SourceError::SourceProtocolMismatch {
            reason: "block header is missing previousblockhash",
        },
    )?)?;
    if source_block.parent_hash != parent_hash {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "block header parent hash does not match raw block header",
        });
    }

    if source_block.block_time_seconds != header.time {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "block header time does not match raw block header",
        });
    }

    Ok(())
}

fn validate_zebra_tree_state(
    tree_state: &Value,
    requested_height: BlockHeight,
    block_hash: BlockHash,
) -> Result<(), SourceError> {
    let tree_state_hash = tree_state
        .get("hash")
        .and_then(Value::as_str)
        .ok_or(SourceError::SourceProtocolMismatch {
            reason: "tree-state response is missing block hash",
        })
        .and_then(decode_display_block_hash)?;
    if tree_state_hash != block_hash {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "tree-state hash does not match raw block hash",
        });
    }

    let tree_state_height = tree_state.get("height").and_then(Value::as_u64).ok_or(
        SourceError::SourceProtocolMismatch {
            reason: "tree-state response is missing block height",
        },
    )?;
    if tree_state_height != u64::from(requested_height.value()) {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "tree-state height does not match requested height",
        });
    }

    Ok(())
}

fn decode_display_transaction_id(
    display_transaction_id: &str,
) -> Result<TransactionId, SourceError> {
    decode_display_hash_32(
        display_transaction_id,
        |source| SourceError::InvalidTransactionIdHex { source },
        |byte_count| SourceError::InvalidTransactionIdLength { byte_count },
    )
    .map(TransactionId::from_bytes)
}

fn decode_subtree_root_hash(root_hash: &str) -> Result<SubtreeRootHash, SourceError> {
    let root_hash_bytes =
        hex::decode(root_hash).map_err(|source| SourceError::InvalidSubtreeRootHex { source })?;
    let byte_count = root_hash_bytes.len();
    let root_hash_bytes = <[u8; 32]>::try_from(root_hash_bytes.as_slice())
        .map_err(|_| SourceError::InvalidSubtreeRootLength { byte_count })?;

    Ok(SubtreeRootHash::from_bytes(root_hash_bytes))
}

fn classify_broadcast_error(error: JsonRpcCallError) -> TransactionBroadcastResult {
    let JsonRpcCallError { code, message } = error;

    if let Some(numeric_error_code) = code {
        let error_code = Some(numeric_error_code);
        return match i32::try_from(numeric_error_code).ok() {
            Some(JSON_RPC_INVALID_ENCODING_CODE) => {
                TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
                    error_code,
                    message,
                })
            }
            Some(JSON_RPC_DUPLICATE_TRANSACTION_CODE) => {
                TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
                    error_code,
                    message,
                })
            }
            _ => TransactionBroadcastResult::Rejected(BroadcastRejected {
                error_code,
                message,
            }),
        };
    }

    TransactionBroadcastResult::Unknown(BroadcastUnknown {
        error_code: None,
        message,
    })
}

/// Domain-shaped JSON-RPC `error` object after we strip jsonrpsee internals.
struct JsonRpcCallError {
    code: Option<i64>,
    message: String,
}

impl JsonRpcCallError {
    fn is_retryable(&self) -> bool {
        matches!(self.code, Some(code) if i32::try_from(code).ok() == Some(JSON_RPC_WARMING_UP_CODE))
    }
}

impl From<ErrorObjectOwned> for JsonRpcCallError {
    fn from(error: ErrorObjectOwned) -> Self {
        Self {
            code: Some(i64::from(error.code())),
            message: error.message().to_owned(),
        }
    }
}

fn map_transport_error(error: &ClientError) -> SourceError {
    let is_retryable = matches!(
        error,
        ClientError::RequestTimeout | ClientError::Transport(_) | ClientError::RestartNeeded(_)
    );

    SourceError::NodeUnavailable {
        is_retryable,
        reason: error.to_string(),
    }
}

#[derive(Deserialize)]
struct ZebraBlockHeader {
    hash: String,
    height: u32,
    #[serde(rename = "previousblockhash")]
    previous_block_hash: Option<String>,
    time: u32,
}

#[derive(Deserialize)]
struct ZebraSubtreeRootsByIndex {
    pool: String,
    start_index: u32,
    subtrees: Vec<ZebraSubtreeRoot>,
}

#[derive(Deserialize)]
struct ZebraGetBlockTrees {
    height: u32,
    trees: Option<ZebraTrees>,
}

#[derive(Deserialize)]
struct ZebraGetBlockchainInfoUpgrades {
    upgrades: std::collections::BTreeMap<String, ZebraNetworkUpgradeInfo>,
}

#[derive(Deserialize)]
struct ZebraNetworkUpgradeInfo {
    name: String,
    #[serde(rename = "activationheight")]
    activation_height: u32,
}

fn activation_height_by_name(
    upgrades: &std::collections::BTreeMap<String, ZebraNetworkUpgradeInfo>,
    name: &str,
) -> Option<BlockHeight> {
    upgrades
        .values()
        .find(|upgrade| upgrade.name.eq_ignore_ascii_case(name))
        .map(|upgrade| BlockHeight::new(upgrade.activation_height))
}

/// Zebra omits `sapling`/`orchard` from `trees` on regtest blocks with no
/// shielded payload, so both fields default to a zero-size pool.
#[derive(Default, Deserialize)]
#[serde(default)]
struct ZebraTrees {
    sapling: ZebraTreeSize,
    orchard: ZebraTreeSize,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ZebraTreeSize {
    size: u64,
}

#[derive(Deserialize)]
struct ZebraSubtreeRoot {
    root: String,
    end_height: u32,
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;

    #[test]
    fn cookie_auth_builds_basic_authorization_from_cookie_file() -> Result<(), SourceError> {
        let cookie_path = std::env::temp_dir().join(format!(
            "zinder-zebra-cookie-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| SourceError::NodeUnavailable {
                    is_retryable: false,
                    reason: "system clock is before unix epoch".to_owned(),
                })?
                .as_nanos()
        ));
        fs::write(&cookie_path, "zebra:secret\n").map_err(|_| SourceError::NodeUnavailable {
            is_retryable: false,
            reason: "test cookie file could not be written".to_owned(),
        })?;

        let authorization = cookie_authorization_header(&cookie_path)?;
        let _ = fs::remove_file(cookie_path);

        assert_eq!(authorization, "Basic emVicmE6c2VjcmV0");
        Ok(())
    }

    #[test]
    fn rpc_basic_auth_allows_basic_or_no_auth() -> Result<(), SourceError> {
        ZebraJsonRpcSource::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:18232",
            NodeAuth::basic("zebra", "zebra"),
            Duration::from_secs(1),
        )?;
        ZebraJsonRpcSource::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:18232",
            NodeAuth::None,
            Duration::from_secs(1),
        )?;

        Ok(())
    }
}
