//! Native protobuf encoders for [`WalletQueryApi`] reads.
//!
//! These free functions translate epoch-bound query responses into
//! `zinder_proto::v1::wallet` messages. They take any `WalletQueryApi`
//! implementation by reference and never call upstream nodes or open storage
//! directly. Splitting these out as free functions instead of a blanket trait
//! impl keeps the [`WalletQueryApi`] boundary free of `zinder_proto` types.

use zebra_chain::transparent::Address as ZebraTransparentAddress;
use zinder_core::{
    BlockHeight, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
    BroadcastRejected, BroadcastRejectionReason, BroadcastUnknown, ChainEpoch,
    CompactBlockArtifact, MinedDetails, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootRange, TransactionBroadcastResult, TransactionId,
    TransactionLocation, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentOutPoint, TransparentOutputsByOutpointResponse, TransparentUnspentOutput, TxStatus,
    wire::{encode_rpc_block_hash_hex, encode_rpc_merkle_root_hex, encode_rpc_transaction_id_hex},
};
use zinder_proto::capabilities::{CapabilitySurface, capabilities_for_surface};
use zinder_proto::compat::lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT;
use zinder_proto::v1::{ops, wallet};
use zinder_source::transparent_address_matches_network;

use crate::{
    BlockHeaderResponseValue, BlockIdResponseValue, ChainEvents, CompactBlock, LatestBlock,
    LatestSafeBlock, QueryError, SubtreeRoots, TransactionStatus, TransparentAddressTxIds,
    TransparentAddressTxIdsInRangeRequest, TransparentAddressUnspentOutputs,
    TransparentAddressUnspentOutputsRequest, TreeState, WalletQueryApi,
};
pub(crate) use zinder_store::chain_epoch_message as build_chain_epoch_message;
use zinder_store::{
    ChainEventEncodeError, ChainEventStreamFamily, StreamCursorTokenV1,
    chain_event_envelope_message, outpoint_message, transparent_output_entry_message,
};

/// Operator-configured snapshot used to build the `WalletServerInfo` descriptor.
///
/// Populated once at startup; the adapter does not call config-rs on each
/// `ServerInfo` request.
#[derive(Clone, Debug)]
pub struct ServerInfoSettings {
    /// Network identifier such as `"zcash-mainnet"` or `"zcash-regtest"`.
    pub network: String,
    /// Semver of the running binary, sourced from `CARGO_PKG_VERSION`.
    pub service_version: String,
    /// Canonical artifact schema version reported by `PrimaryChainStore`.
    pub schema_version: u32,
    /// Configured reorg window depth in blocks.
    pub reorg_window_blocks: u32,
    /// Whether this deployment has a transaction broadcaster configured.
    pub transaction_broadcast_enabled: bool,
    /// Whether this deployment serves the chain-event stream.
    pub chain_events_enabled: bool,
    /// Chain-event retention window in seconds. Zero means unbounded retention.
    pub chain_event_retention_seconds: u64,
    /// Mempool retention windows in seconds. Zero means the corresponding event
    /// family is not retained on this deployment.
    pub mempool_mined_retention_seconds: u64,
    /// Mempool invalidated-event retention window in seconds.
    pub mempool_invalidated_retention_seconds: u64,
    /// Upstream-node capability snapshot captured by the source probe.
    ///
    /// `None` for storage-only deployments that have no source handle
    /// (e.g. a query service running purely off a `RocksDB` secondary).
    /// When `Some`, the contents are surfaced through the `node` field of
    /// the `WalletServerInfo` response and used to compute the
    /// cross-service `ops.ServerInfo.upstream_node_fingerprint`.
    pub upstream_node_capabilities: Option<UpstreamNodeCapabilities>,
    /// Whether this deployment can proxy chain value-pool reads through the
    /// ingest writer's source handle.
    pub chain_value_pools_enabled: bool,
}

/// Snapshot of the upstream-node capability probe used by `ServerInfo`.
///
/// Kept in this crate to avoid pulling the `zinder-source` crate's
/// `NodeCapability` enum across every consumer of `ServerInfoSettings`.
/// Operators set this from the live probe at startup (see
/// `services/zinder-query/src/bin/zinder-query/main.rs`).
#[derive(Clone, Debug, Default)]
pub struct UpstreamNodeCapabilities {
    /// Node-reported semantic version when available.
    pub version: Option<String>,
    /// Stable capability names observed on the upstream node.
    pub capabilities: Vec<String>,
}

impl Default for ServerInfoSettings {
    /// Returns development-mode defaults safe for tests and local composition.
    ///
    /// Production deployments replace these through the runtime config loader,
    /// whose default retention window is bounded.
    fn default() -> Self {
        Self {
            network: "zcash-regtest".to_owned(),
            service_version: env!("CARGO_PKG_VERSION").to_owned(),
            schema_version: u32::from(zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value()),
            reorg_window_blocks: 100,
            transaction_broadcast_enabled: false,
            chain_events_enabled: true,
            chain_event_retention_seconds: 0,
            mempool_mined_retention_seconds: 0,
            mempool_invalidated_retention_seconds: 0,
            upstream_node_capabilities: None,
            chain_value_pools_enabled: false,
        }
    }
}

/// Builds the `WalletServerInfo` descriptor from operator settings.
///
/// Embeds the cross-service [`ops::ServerInfo`] shape every Zinder gRPC
/// surface returns, with capabilities folded from the shared capability table
/// and filtered against the operator settings.
#[must_use]
pub fn build_wallet_server_info(settings: &ServerInfoSettings) -> wallet::WalletServerInfo {
    wallet::WalletServerInfo {
        common: Some(build_ops_server_info(settings)),
        lightwalletd_protocol_commit: LIGHTWALLETD_PROTOCOL_COMMIT.to_owned(),
        schema_version: settings.schema_version,
        reorg_window_blocks: settings.reorg_window_blocks,
        chain_event_retention_seconds: settings.chain_event_retention_seconds,
        mempool_mined_retention_seconds: settings.mempool_mined_retention_seconds,
        mempool_invalidated_retention_seconds: settings.mempool_invalidated_retention_seconds,
        node: Some(build_node_capabilities_descriptor(
            settings.upstream_node_capabilities.as_ref(),
        )),
    }
}

/// Builds the cross-service [`ops::ServerInfo`] descriptor for `zinder-query`.
///
/// Capability strings are filtered against the operator settings (e.g.
/// broadcast is only advertised when a broadcaster is configured).
#[must_use]
fn build_ops_server_info(settings: &ServerInfoSettings) -> ops::ServerInfo {
    ops::ServerInfo {
        network: settings.network.clone(),
        service_name: env!("CARGO_PKG_NAME").to_owned(),
        service_version: settings.service_version.clone(),
        capabilities: capabilities_for_surface(CapabilitySurface::Wallet)
            .filter(|spec| {
                spec.policy.wallet_satisfied(
                    settings.transaction_broadcast_enabled,
                    settings.chain_events_enabled,
                    settings.chain_value_pools_enabled,
                )
            })
            .map(|spec| spec.string.to_owned())
            .collect(),
    }
}

fn build_node_capabilities_descriptor(
    upstream: Option<&UpstreamNodeCapabilities>,
) -> wallet::NodeCapabilitiesDescriptor {
    upstream.map_or_else(
        || wallet::NodeCapabilitiesDescriptor {
            version: None,
            capabilities: Vec::new(),
        },
        |capabilities| wallet::NodeCapabilitiesDescriptor {
            version: capabilities.version.clone(),
            capabilities: capabilities.capabilities.clone(),
        },
    )
}

/// Reads the latest visible block and encodes the native wallet response.
pub async fn latest_block_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::LatestBlockResponse, QueryError> {
    query_api
        .latest_block(at_epoch)
        .await
        .map(build_latest_block_response)
}

/// Reads the block at the chain epoch's safe tip and encodes the native
/// wallet response.
pub(super) async fn latest_safe_block_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::LatestSafeBlockResponse, QueryError> {
    query_api
        .latest_safe_block(at_epoch)
        .await
        .map(build_latest_safe_block_response)
}

/// Reads the compact block at `height` and encodes the native wallet response.
pub async fn compact_block_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    height: BlockHeight,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::CompactBlockResponse, QueryError> {
    query_api
        .compact_block_at(height, at_epoch)
        .await
        .map(build_compact_block_response)
}

/// Resolves a typed block selector and encodes the native wallet response.
pub async fn block_id_by_selector_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    selector: zinder_core::BlockSelector,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::BlockIdResponse, QueryError> {
    query_api
        .block_id_by_selector(selector, at_epoch)
        .await
        .map(build_block_id_response)
}

/// Reads the typed block-header read model at a typed block selector and
/// encodes the native wallet response.
pub async fn block_header_by_selector_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    selector: zinder_core::BlockSelector,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::BlockHeaderResponse, QueryError> {
    query_api
        .block_header_by_selector(selector, at_epoch)
        .await
        .map(|response| build_block_header_response(&response))
}

/// Reads the typed transaction status at `transaction_id` and encodes
/// the native wallet response.
///
/// Returns `Ok(None)` when the canonical chain and the live mempool both
/// have no record of the transaction. The wire contract maps that case to
/// gRPC `NOT_FOUND` at the adapter layer, where the original transaction
/// id is in scope for the error message; the encoder never has to
/// fabricate a transaction id for a status that does not appear on the
/// wire oneof.
pub async fn transaction_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    transaction_id: TransactionId,
    at_epoch: Option<ChainEpoch>,
) -> Result<Option<wallet::TransactionStatusResponse>, QueryError> {
    let status = query_api.transaction(transaction_id, at_epoch).await?;
    build_transaction_status_response(status)
}

/// Reads the tree-state at exactly `height` and encodes the native wallet response.
pub async fn tree_state_at_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    height: BlockHeight,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TreeStateResponse, QueryError> {
    query_api
        .tree_state_at(height, at_epoch)
        .await
        .map(build_tree_state_response)
}

/// Reads the latest tree-state and encodes the native wallet response.
pub async fn latest_tree_state_checkpoint_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TreeStateResponse, QueryError> {
    query_api
        .latest_tree_state_checkpoint(at_epoch)
        .await
        .map(build_tree_state_response)
}

/// Reads subtree roots in `subtree_root_range` and encodes the native wallet response.
pub async fn subtree_roots_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    subtree_root_range: SubtreeRootRange,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::SubtreeRootsResponse, QueryError> {
    query_api
        .subtree_roots(subtree_root_range, at_epoch)
        .await
        .and_then(|subtree_roots| build_subtree_roots_response(&subtree_roots))
}

/// Broadcasts `raw_transaction` and encodes the native wallet response.
pub async fn broadcast_transaction_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    raw_transaction: RawTransactionBytes,
) -> Result<wallet::BroadcastTransactionResponse, QueryError> {
    query_api
        .broadcast_transaction(raw_transaction)
        .await
        .map(build_broadcast_transaction_response)
}

/// Reads one bounded chain-event page and encodes the native wallet messages.
pub async fn chain_events_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    from_cursor: Option<StreamCursorTokenV1>,
    family: ChainEventStreamFamily,
) -> Result<Vec<wallet::ChainEventEnvelope>, QueryError> {
    query_api
        .chain_events(from_cursor, family)
        .await
        .and_then(|chain_events| build_chain_events_response(&chain_events))
}

/// Resolves a batch of canonical-chain transparent outpoints to their
/// referenced outputs and encodes the native wallet response.
///
pub async fn transparent_outputs_by_outpoint_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    outpoints: Vec<TransparentOutPoint>,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TransparentOutputsByOutpointResponse, QueryError> {
    query_api
        .transparent_outputs_by_outpoint(outpoints, at_epoch)
        .await
        .map(build_transparent_outputs_by_outpoint_response)
}

/// Reads the complete unspent transparent output set for `request` at one
/// pinned chain epoch. The gRPC adapter streams the returned set one
/// message per output.
pub async fn transparent_address_unspent_outputs_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    request: TransparentAddressUnspentOutputsRequest,
    at_epoch: Option<ChainEpoch>,
) -> Result<TransparentAddressUnspentOutputs, QueryError> {
    query_api
        .transparent_address_unspent_outputs(request, at_epoch)
        .await
}

/// Resolves a `wallet::AddressLookup` oneof to the typed
/// `TransparentAddressScriptHash`. String addresses are parsed against
/// `network` and SHA-256-hashed; raw script-hash bytes are taken verbatim.
pub fn address_lookup_to_script_hash(
    address: Option<wallet::AddressLookup>,
    network: Network,
) -> Result<TransparentAddressScriptHash, QueryError> {
    let lookup = address.ok_or(QueryError::InvalidAddress {
        reason: "address selector is required",
    })?;
    let selector = lookup.selector.ok_or(QueryError::InvalidAddress {
        reason: "address selector is empty",
    })?;
    match selector {
        wallet::address_lookup::Selector::ScriptHash(bytes) => {
            let hash_bytes: [u8; 32] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| QueryError::InvalidAddress {
                        reason: "script_hash must be 32 bytes",
                    })?;
            Ok(TransparentAddressScriptHash::from_bytes(hash_bytes))
        }
        wallet::address_lookup::Selector::Address(address_text) => {
            let zebra_address = address_text
                .parse::<ZebraTransparentAddress>()
                .map_err(|_| QueryError::InvalidAddress {
                    reason: "transparent address could not be parsed",
                })?;
            if !transparent_address_matches_network(zebra_address.network_kind(), network) {
                return Err(QueryError::InvalidAddress {
                    reason: "transparent address network does not match server network",
                });
            }
            let script_pub_key = zebra_address.script().as_raw_bytes().to_vec();
            if script_pub_key.is_empty() {
                return Err(QueryError::InvalidAddress {
                    reason: "transparent address does not produce a receivable script",
                });
            }
            Ok(TransparentAddressScriptHash::of_script_pub_key(
                &script_pub_key,
            ))
        }
    }
}

/// Reads a page of transparent-address tx-history index artifacts and
/// surfaces them through the typed query layer for the gRPC adapter to
/// chunk into a server-streamed response.
pub async fn transparent_address_tx_ids_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    request: TransparentAddressTxIdsInRangeRequest,
) -> Result<TransparentAddressTxIds, QueryError> {
    query_api.transparent_address_tx_ids_in_range(request).await
}

/// Builds one streamed tx-history chunk message.
#[must_use]
pub fn build_transparent_address_tx_ids_chunk(
    chain_epoch: ChainEpoch,
    artifact: &TransparentAddressTxIndexArtifact,
    cursor: Vec<u8>,
) -> wallet::TransparentAddressTxIdsChunk {
    wallet::TransparentAddressTxIdsChunk {
        chain_epoch: Some(build_chain_epoch_message(chain_epoch)),
        transaction_id: encode_rpc_transaction_id_hex(artifact.transaction_id),
        block_height: artifact.block_height.value(),
        tx_index_in_block: artifact.tx_index_in_block,
        block_hash: encode_rpc_block_hash_hex(artifact.block_hash),
        cursor,
    }
}

/// Builds one streamed unspent-output message bound to the stream's pinned
/// chain epoch.
#[must_use]
pub fn build_transparent_unspent_output_message(
    chain_epoch: ChainEpoch,
    output: &TransparentUnspentOutput,
) -> wallet::TransparentUnspentOutput {
    wallet::TransparentUnspentOutput {
        chain_epoch: Some(build_chain_epoch_message(chain_epoch)),
        address_script_hash: output.address_script_hash.as_bytes().to_vec(),
        script_pub_key: output.script_pub_key.clone(),
        outpoint: Some(outpoint_message(&output.outpoint)),
        value_zat: output.value_zat,
        block_height: output.block_height.value(),
        block_hash: encode_rpc_block_hash_hex(output.block_hash),
    }
}

fn build_transparent_outputs_by_outpoint_response(
    response: TransparentOutputsByOutpointResponse,
) -> wallet::TransparentOutputsByOutpointResponse {
    wallet::TransparentOutputsByOutpointResponse {
        chain_epoch: Some(build_chain_epoch_message(response.chain_epoch)),
        entries: response
            .entries
            .into_iter()
            .map(transparent_output_entry_message)
            .collect(),
    }
}

fn build_latest_block_response(latest_block: LatestBlock) -> wallet::LatestBlockResponse {
    wallet::LatestBlockResponse {
        chain_epoch: Some(build_chain_epoch_message(latest_block.chain_epoch)),
        latest_block: Some(build_block_metadata_message(
            latest_block.height,
            latest_block.block_hash,
        )),
    }
}

fn build_latest_safe_block_response(
    safe_block: LatestSafeBlock,
) -> wallet::LatestSafeBlockResponse {
    wallet::LatestSafeBlockResponse {
        chain_epoch: Some(build_chain_epoch_message(safe_block.chain_epoch)),
        safe_tip_block: Some(build_block_metadata_message(
            safe_block.height,
            safe_block.block_hash,
        )),
    }
}

fn build_block_id_response(response: BlockIdResponseValue) -> wallet::BlockIdResponse {
    wallet::BlockIdResponse {
        chain_epoch: Some(build_chain_epoch_message(response.chain_epoch)),
        block_id: Some(build_block_metadata_message(
            response.block_id.height,
            response.block_id.hash,
        )),
    }
}

fn build_block_header_response(response: &BlockHeaderResponseValue) -> wallet::BlockHeaderResponse {
    let header = &response.block_header;
    wallet::BlockHeaderResponse {
        chain_epoch: Some(build_chain_epoch_message(response.chain_epoch)),
        block_header: Some(wallet::BlockHeaderInfo {
            block_id: Some(build_block_metadata_message(
                header.block_id.height,
                header.block_id.hash,
            )),
            previous_block_hash: encode_rpc_block_hash_hex(header.previous_block_hash),
            merkle_root_hash: encode_rpc_merkle_root_hex(header.merkle_root_hash),
            commitment_bytes: header.commitment_bytes.to_vec(),
            block_time: header.block_time,
            bits: header.bits,
            nonce: header.nonce.to_vec(),
            version: header.version,
        }),
    }
}

fn build_compact_block_response(compact_block: CompactBlock) -> wallet::CompactBlockResponse {
    wallet::CompactBlockResponse {
        chain_epoch: Some(build_chain_epoch_message(compact_block.chain_epoch)),
        compact_block: Some(build_compact_block_message(compact_block.compact_block)),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TxStatus is #[non_exhaustive]; new arms must be wired into the proto oneof in a deliberate change, not folded into a default branch."
)]
fn build_transaction_status_response(
    status: TransactionStatus,
) -> Result<Option<wallet::TransactionStatusResponse>, QueryError> {
    let chain_epoch = status.chain_epoch;
    let oneof = match status.status {
        TxStatus::Mined(mined) => {
            wallet::transaction_status_response::Status::Mined(wallet::MinedTransaction {
                location: Some(build_transaction_location_message(mined.location)),
                details: Some(build_mined_details_message(mined.details)),
            })
        }
        TxStatus::InMempool(entry) => {
            wallet::transaction_status_response::Status::InMempool(wallet::MempoolTransaction {
                payload_bytes: entry.raw_transaction_bytes.as_slice().to_vec(),
                first_seen_unix_seconds: i64::try_from(entry.first_seen_unix_millis.value() / 1000)
                    .unwrap_or(i64::MAX),
            })
        }
        TxStatus::ConflictingChain => wallet::transaction_status_response::Status::Conflicting(
            wallet::ConflictingChainTransaction {},
        ),
        TxStatus::NotFound => return Ok(None),
        _ => {
            return Err(QueryError::UnsupportedTransactionStatus {
                reason: "transaction status variant has no wire representation",
            });
        }
    };
    Ok(Some(wallet::TransactionStatusResponse {
        chain_epoch: Some(build_chain_epoch_message(chain_epoch)),
        status: Some(oneof),
    }))
}

fn build_mined_details_message(details: MinedDetails) -> wallet::MinedDetails {
    wallet::MinedDetails {
        consensus_branch_id: details.consensus_branch_id.value(),
        block_time: details.block_time,
        confirmations: details.confirmations,
    }
}

fn build_transaction_location_message(
    transaction: TransactionLocation,
) -> wallet::TransactionLocation {
    wallet::TransactionLocation {
        transaction_id: encode_rpc_transaction_id_hex(transaction.transaction_id),
        block_height: transaction.block_height.value(),
        block_hash: encode_rpc_block_hash_hex(transaction.block_hash),
        tx_index_in_block: transaction.tx_index_in_block,
    }
}

fn build_tree_state_response(tree_state: TreeState) -> wallet::TreeStateResponse {
    wallet::TreeStateResponse {
        chain_epoch: Some(build_chain_epoch_message(tree_state.chain_epoch)),
        height: tree_state.height.value(),
        block_hash: encode_rpc_block_hash_hex(tree_state.block_hash),
        payload_bytes: tree_state.payload_bytes,
    }
}

fn build_subtree_roots_response(
    subtree_roots: &SubtreeRoots,
) -> Result<wallet::SubtreeRootsResponse, QueryError> {
    Ok(wallet::SubtreeRootsResponse {
        chain_epoch: Some(build_chain_epoch_message(subtree_roots.chain_epoch)),
        shielded_protocol: native_shielded_protocol(subtree_roots.protocol)? as i32,
        start_index: subtree_roots.start_index.value(),
        subtree_roots: subtree_roots
            .subtree_roots
            .iter()
            .map(build_subtree_root_message)
            .collect(),
    })
}

fn build_broadcast_transaction_response(
    broadcast_result: TransactionBroadcastResult,
) -> wallet::BroadcastTransactionResponse {
    use wallet::broadcast_transaction_response::Outcome;

    let outcome = match broadcast_result {
        TransactionBroadcastResult::Accepted(accepted) => {
            Outcome::Accepted(build_broadcast_accepted_message(accepted))
        }
        TransactionBroadcastResult::Duplicate(duplicate) => {
            Outcome::Duplicate(build_broadcast_duplicate_message(duplicate))
        }
        TransactionBroadcastResult::InvalidEncoding(invalid_encoding) => {
            Outcome::InvalidEncoding(build_broadcast_invalid_encoding_message(invalid_encoding))
        }
        TransactionBroadcastResult::Queued(queued) => {
            Outcome::Queued(build_broadcast_queued_message(queued))
        }
        TransactionBroadcastResult::Rejected(rejected) => {
            Outcome::Rejected(build_broadcast_rejected_message(rejected))
        }
        TransactionBroadcastResult::Unknown(unknown) => {
            Outcome::Unknown(build_broadcast_unknown_message(unknown))
        }
        _ => Outcome::Unknown(wallet::BroadcastUnknown {
            error_code: None,
            message: "unknown transaction broadcast result variant".to_owned(),
        }),
    };

    wallet::BroadcastTransactionResponse {
        outcome: Some(outcome),
    }
}

fn build_broadcast_accepted_message(accepted: BroadcastAccepted) -> wallet::BroadcastAccepted {
    wallet::BroadcastAccepted {
        transaction_id: encode_rpc_transaction_id_hex(accepted.transaction_id),
    }
}

fn build_broadcast_duplicate_message(duplicate: BroadcastDuplicate) -> wallet::BroadcastDuplicate {
    wallet::BroadcastDuplicate {
        error_code: duplicate.error_code,
        message: duplicate.message,
    }
}

fn build_broadcast_invalid_encoding_message(
    invalid_encoding: BroadcastInvalidEncoding,
) -> wallet::BroadcastInvalidEncoding {
    wallet::BroadcastInvalidEncoding {
        error_code: invalid_encoding.error_code,
        message: invalid_encoding.message,
    }
}

fn build_broadcast_rejected_message(rejected: BroadcastRejected) -> wallet::BroadcastRejected {
    wallet::BroadcastRejected {
        error_code: rejected.error_code,
        message: rejected.message,
        kind: broadcast_rejection_reason_to_message(rejected.kind) as i32,
    }
}

fn build_broadcast_queued_message(queued: BroadcastQueued) -> wallet::BroadcastQueued {
    wallet::BroadcastQueued {
        message: queued.message,
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "BroadcastRejectionReason is #[non_exhaustive]; new variants must be wired into the proto enum in a deliberate change."
)]
fn broadcast_rejection_reason_to_message(
    kind: BroadcastRejectionReason,
) -> wallet::BroadcastRejectionReason {
    match kind {
        BroadcastRejectionReason::InvalidSignature => {
            wallet::BroadcastRejectionReason::InvalidSignature
        }
        BroadcastRejectionReason::BadExpiryHeight => {
            wallet::BroadcastRejectionReason::BadExpiryHeight
        }
        BroadcastRejectionReason::BadConsensusBranch => {
            wallet::BroadcastRejectionReason::BadConsensusBranch
        }
        BroadcastRejectionReason::MempoolFull => wallet::BroadcastRejectionReason::MempoolFull,
        // BroadcastRejectionReason::Unknown and every future non-exhaustive
        // variant both collapse to the wire's Unknown enumerator.
        _ => wallet::BroadcastRejectionReason::Unknown,
    }
}

fn build_broadcast_unknown_message(unknown: BroadcastUnknown) -> wallet::BroadcastUnknown {
    wallet::BroadcastUnknown {
        error_code: unknown.error_code,
        message: unknown.message,
    }
}

fn build_chain_events_response(
    chain_events: &ChainEvents,
) -> Result<Vec<wallet::ChainEventEnvelope>, QueryError> {
    chain_events
        .event_envelopes
        .iter()
        .map(|event_envelope| {
            chain_event_envelope_message(event_envelope).map_err(map_chain_event_encode_error)
        })
        .collect()
}

fn map_chain_event_encode_error(error: ChainEventEncodeError) -> QueryError {
    match error {
        ChainEventEncodeError::UnsupportedChainEvent { event } => {
            QueryError::UnsupportedChainEvent { event }
        }
        _ => QueryError::UnsupportedChainEvent {
            event: "unknown chain event encode error",
        },
    }
}

fn build_block_metadata_message(
    height: BlockHeight,
    block_hash: zinder_core::BlockHash,
) -> wallet::BlockMetadata {
    wallet::BlockMetadata {
        height: height.value(),
        block_hash: encode_rpc_block_hash_hex(block_hash),
    }
}

pub(crate) fn build_compact_block_message(
    compact_block: CompactBlockArtifact,
) -> wallet::CompactBlock {
    wallet::CompactBlock {
        height: compact_block.height.value(),
        block_hash: encode_rpc_block_hash_hex(compact_block.block_hash),
        payload_bytes: compact_block.payload_bytes,
    }
}

fn build_subtree_root_message(subtree_root: &SubtreeRootArtifact) -> wallet::SubtreeRoot {
    wallet::SubtreeRoot {
        subtree_index: subtree_root.subtree_index.value(),
        root_hash: subtree_root.root_hash.as_bytes().into(),
        completing_block_hash: encode_rpc_block_hash_hex(subtree_root.completing_block_hash),
        completing_block_height: subtree_root.completing_block_height.value(),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive core protocols must fail closed until the native proto supports them"
)]
fn native_shielded_protocol(
    protocol: ShieldedProtocol,
) -> Result<wallet::ShieldedProtocol, QueryError> {
    match protocol {
        ShieldedProtocol::Sapling => Ok(wallet::ShieldedProtocol::Sapling),
        ShieldedProtocol::Orchard => Ok(wallet::ShieldedProtocol::Orchard),
        _ => Err(QueryError::UnsupportedShieldedProtocol { protocol }),
    }
}

#[cfg(test)]
mod server_info_tests {
    use super::{ServerInfoSettings, UpstreamNodeCapabilities, build_wallet_server_info};

    #[test]
    fn build_wallet_server_info_populates_node_when_upstream_known() {
        let settings = ServerInfoSettings {
            upstream_node_capabilities: Some(UpstreamNodeCapabilities {
                version: Some("2.4.0".to_owned()),
                capabilities: vec!["tx_broadcast".to_owned(), "subtree_roots".to_owned()],
            }),
            ..ServerInfoSettings::default()
        };

        let descriptor = build_wallet_server_info(&settings);
        let Some(node) = descriptor.node else {
            unreachable!("node field must always be set")
        };
        assert_eq!(node.version.as_deref(), Some("2.4.0"));
        assert_eq!(node.capabilities.len(), 2);
        assert!(node.capabilities.iter().any(|cap| cap == "tx_broadcast"));

        let Some(common) = descriptor.common else {
            unreachable!("common ops.ServerInfo field must always be set")
        };
        assert_eq!(common.service_name, env!("CARGO_PKG_NAME"));
        assert!(!common.capabilities.is_empty());
    }

    #[test]
    fn build_wallet_server_info_emits_empty_node_when_no_upstream() {
        let settings = ServerInfoSettings::default();
        let descriptor = build_wallet_server_info(&settings);
        let Some(node) = descriptor.node else {
            unreachable!("node field must always be set")
        };
        assert!(node.version.is_none());
        assert!(node.capabilities.is_empty());

        let Some(common) = descriptor.common else {
            unreachable!("common ops.ServerInfo field must always be set")
        };
        assert_eq!(common.network, "zcash-regtest");
    }
}
