//! Native protobuf encoders for [`WalletQueryApi`] reads.
//!
//! These free functions translate epoch-bound query responses into
//! `zinder_proto::v1::wallet` messages. They take any `WalletQueryApi`
//! implementation by reference and never call upstream nodes or open storage
//! directly. Splitting these out as free functions instead of a blanket trait
//! impl keeps the [`WalletQueryApi`] boundary free of `zinder_proto` types.

use zinder_core::{
    BlockHeight, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding,
    BroadcastRejected, BroadcastUnknown, ChainEpoch, CompactBlockArtifact, RawTransactionBytes,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootRange, TransactionArtifact,
    TransactionBroadcastResult, TransactionId,
};
use zinder_proto::ZINDER_CAPABILITIES;
use zinder_proto::compat::lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT;
use zinder_proto::v1::wallet;

use crate::{
    ChainEvents, CompactBlock, LatestBlock, QueryError, SubtreeRoots, Transaction, TreeState,
    WalletQueryApi,
};
pub(crate) use zinder_store::chain_epoch_message as build_chain_epoch_message;
use zinder_store::{
    ChainEventEncodeError, ChainEventStreamFamily, StreamCursorTokenV1,
    chain_event_envelope_message,
};

/// Operator-configured snapshot used to build the `ServerCapabilities` descriptor.
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
    /// Configured non-finalized window depth in blocks.
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
        }
    }
}

/// Builds the `ServerCapabilities` descriptor from operator settings and the
/// statically advertised capability list in [`ZINDER_CAPABILITIES`].
#[must_use]
pub fn build_server_capabilities_message(
    settings: &ServerInfoSettings,
) -> wallet::ServerCapabilities {
    wallet::ServerCapabilities {
        network: settings.network.clone(),
        service_version: settings.service_version.clone(),
        lightwalletd_protocol_commit: LIGHTWALLETD_PROTOCOL_COMMIT.to_owned(),
        schema_version: settings.schema_version,
        reorg_window_blocks: settings.reorg_window_blocks,
        chain_event_retention_seconds: settings.chain_event_retention_seconds,
        mempool_mined_retention_seconds: settings.mempool_mined_retention_seconds,
        mempool_invalidated_retention_seconds: settings.mempool_invalidated_retention_seconds,
        capabilities: ZINDER_CAPABILITIES
            .iter()
            .filter(|capability| capability_enabled(capability, settings))
            .map(|cap| (*cap).to_owned())
            .collect(),
        deprecated_capabilities: Vec::new(),
        node: Some(wallet::NodeCapabilitiesDescriptor {
            version: None,
            capabilities: Vec::new(),
        }),
        mcp_endpoint: None,
    }
}

fn capability_enabled(capability: &&str, settings: &ServerInfoSettings) -> bool {
    match *capability {
        "wallet.broadcast.transaction_v1" => settings.transaction_broadcast_enabled,
        "wallet.events.chain_v1" => settings.chain_events_enabled,
        _ => true,
    }
}

/// Reads the latest visible block and encodes the native wallet response.
pub async fn latest_block_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::LatestBlockResponse, QueryError> {
    query_api
        .latest_block_at_epoch(at_epoch)
        .await
        .map(build_latest_block_response)
}

/// Reads the compact block at `height` and encodes the native wallet response.
pub async fn compact_block_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    height: BlockHeight,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::CompactBlockResponse, QueryError> {
    query_api
        .compact_block_at_epoch(height, at_epoch)
        .await
        .map(build_compact_block_response)
}

/// Reads the indexed transaction at `transaction_id` and encodes the native wallet response.
pub async fn transaction_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    transaction_id: TransactionId,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TransactionResponse, QueryError> {
    query_api
        .transaction_at_epoch(transaction_id, at_epoch)
        .await
        .map(build_transaction_response)
}

/// Reads the tree-state at `height` and encodes the native wallet response.
pub async fn tree_state_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    height: BlockHeight,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TreeStateResponse, QueryError> {
    query_api
        .tree_state_at_epoch(height, at_epoch)
        .await
        .map(build_tree_state_response)
}

/// Reads the latest tree-state and encodes the native wallet response.
pub async fn latest_tree_state_response<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    at_epoch: Option<ChainEpoch>,
) -> Result<wallet::TreeStateResponse, QueryError> {
    query_api
        .latest_tree_state_at_epoch(at_epoch)
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
        .subtree_roots_at_epoch(subtree_root_range, at_epoch)
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

fn build_latest_block_response(latest_block: LatestBlock) -> wallet::LatestBlockResponse {
    wallet::LatestBlockResponse {
        chain_epoch: Some(build_chain_epoch_message(latest_block.chain_epoch)),
        latest_block: Some(build_block_metadata_message(
            latest_block.height,
            latest_block.block_hash,
        )),
    }
}

fn build_compact_block_response(compact_block: CompactBlock) -> wallet::CompactBlockResponse {
    wallet::CompactBlockResponse {
        chain_epoch: Some(build_chain_epoch_message(compact_block.chain_epoch)),
        compact_block: Some(build_compact_block_message(compact_block.compact_block)),
    }
}

fn build_transaction_response(transaction: Transaction) -> wallet::TransactionResponse {
    wallet::TransactionResponse {
        chain_epoch: Some(build_chain_epoch_message(transaction.chain_epoch)),
        transaction: Some(build_transaction_message(transaction.transaction)),
    }
}

fn build_transaction_message(transaction: TransactionArtifact) -> wallet::Transaction {
    wallet::Transaction {
        transaction_id: transaction.transaction_id.as_bytes().into(),
        block_height: transaction.block_height.value(),
        block_hash: transaction.block_hash.as_bytes().into(),
        payload_bytes: transaction.payload_bytes,
    }
}

fn build_tree_state_response(tree_state: TreeState) -> wallet::TreeStateResponse {
    wallet::TreeStateResponse {
        chain_epoch: Some(build_chain_epoch_message(tree_state.chain_epoch)),
        height: tree_state.height.value(),
        block_hash: tree_state.block_hash.as_bytes().into(),
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
        transaction_id: accepted.transaction_id.as_bytes().into(),
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
        block_hash: block_hash.as_bytes().into(),
    }
}

pub(crate) fn build_compact_block_message(
    compact_block: CompactBlockArtifact,
) -> wallet::CompactBlock {
    wallet::CompactBlock {
        height: compact_block.height.value(),
        block_hash: compact_block.block_hash.as_bytes().into(),
        payload_bytes: compact_block.payload_bytes,
    }
}

fn build_subtree_root_message(subtree_root: &SubtreeRootArtifact) -> wallet::SubtreeRoot {
    wallet::SubtreeRoot {
        subtree_index: subtree_root.subtree_index.value(),
        root_hash: subtree_root.root_hash.as_bytes().into(),
        completing_block_hash: subtree_root.completing_block_hash.as_bytes().into(),
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
