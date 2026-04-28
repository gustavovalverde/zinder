//! CLI string-to-value parsers for the `zinder-ingest` binary.
//!
//! These helpers convert validated string and integer inputs from the CLI and
//! TOML config layer into the typed values the ingest library expects. They
//! are intentionally crate-private to the binary: the library boundary should
//! not leak CLI parsing concerns.

use std::num::{NonZeroU32, NonZeroU64};

use zinder_core::Network;
use zinder_ingest::{IngestError, NodeSourceKind};

/// Parses the public network configuration name.
pub(crate) fn parse_network(network_name: &str) -> Result<Network, IngestError> {
    Network::from_name(network_name).ok_or_else(|| IngestError::UnknownNetwork {
        network_name: network_name.to_owned(),
    })
}

/// Parses the public node source configuration name.
pub(crate) fn parse_node_source(node_source: &str) -> Result<NodeSourceKind, IngestError> {
    match node_source {
        "zebra-json-rpc" => Ok(NodeSourceKind::ZebraJsonRpc),
        _ => Err(IngestError::UnknownNodeSource {
            node_source: node_source.to_owned(),
        }),
    }
}

/// Parses the maximum commit batch size.
pub(crate) fn parse_commit_batch_blocks(
    commit_batch_blocks: u32,
) -> Result<NonZeroU32, IngestError> {
    NonZeroU32::new(commit_batch_blocks).ok_or(IngestError::InvalidCommitBatchBlocks)
}

/// Parses the maximum replaceable reorg-window size.
pub(crate) fn parse_reorg_window_blocks(reorg_window_blocks: u32) -> Result<u32, IngestError> {
    if reorg_window_blocks == 0 {
        return Err(IngestError::InvalidReorgWindowBlocks);
    }

    Ok(reorg_window_blocks)
}

/// Parses the maximum JSON-RPC response body size.
pub(crate) fn parse_max_response_bytes(max_response_bytes: u64) -> Result<NonZeroU64, IngestError> {
    NonZeroU64::new(max_response_bytes).ok_or(IngestError::InvalidMaxResponseBytes)
}

/// Parses the tip-follow poll interval in milliseconds.
pub(crate) fn parse_poll_interval_ms(
    poll_interval_ms: u64,
) -> Result<std::time::Duration, IngestError> {
    if poll_interval_ms == 0 {
        return Err(IngestError::InvalidTipFollowPollInterval);
    }

    Ok(std::time::Duration::from_millis(poll_interval_ms))
}
