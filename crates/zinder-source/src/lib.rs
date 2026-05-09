//! Source values and adapters for Zinder ingestion.
//!
//! This crate normalizes upstream node observations into source-domain values. It
//! does not decide canonical chain state and does not build durable artifacts.

mod chain_checkpoint;
mod json_rpc_mempool;
mod mempool_source;
mod node_auth;
mod node_capabilities;
mod node_source;
mod node_target;
mod source_block;
mod source_error;
mod source_subtree_root;
mod zebra_indexer_mempool;
mod zebra_json_rpc;

pub use chain_checkpoint::{SourceChainCheckpoint, SourceNetworkUpgradeHeights};
pub use json_rpc_mempool::{
    DEFAULT_MEMPOOL_POLL_INTERVAL, JsonRpcMempoolSource, JsonRpcMempoolSourceOptions,
};
pub use mempool_source::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceBackend, MempoolSourceCapabilities,
    MempoolSourceEntry, MempoolSourceEvent, MempoolSourceEventStream,
};
pub use node_auth::NodeAuth;
pub use node_capabilities::{NodeCapabilities, NodeCapabilitiesError, NodeCapability};
pub use node_source::{NodeSource, TransactionBroadcaster};
pub use node_target::{
    DEFAULT_NODE_REQUEST_TIMEOUT_SECS, NodeAuthSection, NodeConfigError, NodeSection, NodeTarget,
};
pub use source_block::{
    SourceBlock, SourceBlockHeader, block_header_info_from_raw_block_bytes, consensus_branch_id_at,
    decode_display_block_hash, encode_display_block_hash, zebra_network,
};
pub use source_error::SourceError;
pub use source_subtree_root::{SourceSubtreeRoot, SourceSubtreeRoots};
pub use zebra_indexer_mempool::{
    ZebraIndexerMempoolSource, ZebraIndexerMempoolSourceOptions, ZebraIndexerSourceTarget,
};
pub use zebra_json_rpc::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, UpstreamTransactionLookup, ZebraJsonRpcSource,
    ZebraJsonRpcSourceOptions,
};
