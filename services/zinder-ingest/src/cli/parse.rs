//! CLI string-to-value parsers for the `zinder-ingest` binary.
//!
//! These helpers convert validated string and integer inputs from the CLI and
//! TOML config layer into the typed values the ingest library expects. They
//! are intentionally crate-private to the binary: the library boundary should
//! not leak CLI parsing concerns.

use std::num::NonZeroU32;

use zinder_ingest::{IngestError, NodeSourceKind};

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

/// Parses the tip-follow poll interval in milliseconds.
pub(crate) fn parse_poll_interval_ms(
    poll_interval_ms: u64,
) -> Result<std::time::Duration, IngestError> {
    if poll_interval_ms == 0 {
        return Err(IngestError::InvalidTipFollowPollInterval);
    }

    Ok(std::time::Duration::from_millis(poll_interval_ms))
}
