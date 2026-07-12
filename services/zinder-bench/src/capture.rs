//! Fixed-range capture: pull raw source payloads from a live Zebra node and
//! write them into a deterministic fixture directory.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    time::Duration,
};

use futures_util::StreamExt as _;
use zinder_core::{
    BlockHeight, Network, ShieldedProtocol, SubtreeRootIndex, wire::encode_zinder_native_chain_name,
};
use zinder_source::{
    NodeAuth, NodeSource, SourceBlock, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION;

use crate::{
    error::BenchError,
    fixture::{
        ActivationRecord, FIXTURE_FORMAT_VERSION, FixtureManifest, SegmentDescriptor,
        SubtreeRootRecord, SubtreeRootSet, write_segment,
    },
};

const SUBTREE_ROOT_PAGE_SIZE: u32 = 1_000;

/// Inputs for one fixed-range capture.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// Network the node answers for.
    pub network: Network,
    /// Node JSON-RPC base URL.
    pub json_rpc_addr: String,
    /// Node authentication mode.
    pub node_auth: NodeAuth,
    /// First block height to capture.
    pub from_height: BlockHeight,
    /// Last block height to capture.
    pub to_height: BlockHeight,
    /// Blocks written per segment file.
    pub segment_blocks: NonZeroU32,
    /// Concurrent block fetches issued against the node.
    pub fetch_concurrency: NonZeroU32,
    /// Per-request source timeout.
    pub request_timeout: Duration,
    /// Maximum JSON-RPC response body size.
    pub max_response_bytes: NonZeroU64,
    /// Destination fixture directory.
    pub output_directory: PathBuf,
}

/// Captures the configured block range into a fixture directory and returns the
/// manifest that was written.
pub async fn capture_fixed_range(config: CaptureConfig) -> Result<FixtureManifest, BenchError> {
    if config.from_height.value() > config.to_height.value() {
        return Err(BenchError::invalid_argument(
            "from_height must be less than or equal to to_height",
        ));
    }
    std::fs::create_dir_all(&config.output_directory)
        .map_err(|source| BenchError::io(&config.output_directory, source))?;

    let source = ZebraJsonRpcSource::with_options(
        config.network,
        &config.json_rpc_addr,
        config.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: config.request_timeout,
            max_response_bytes: config.max_response_bytes,
            broadcast_timeout: None,
        },
    )?;

    let activations = source
        .discover_network_upgrade_activations("zinder-bench")
        .await?;
    let activation_records = activations
        .activations()
        .iter()
        .map(|activation| ActivationRecord {
            branch_id: activation.branch_id.value(),
            activation_height: activation.activation_height.value(),
            name: activation.name.clone(),
        })
        .collect();

    let CapturedSegments {
        segments,
        tip_hash_hex,
    } = capture_segments(&source, &config).await?;

    let subtree_roots = SubtreeRootSet {
        sapling: capture_subtree_roots(&source, ShieldedProtocol::Sapling, config.to_height)
            .await?,
        orchard: capture_subtree_roots(&source, ShieldedProtocol::Orchard, config.to_height)
            .await?,
        ironwood: capture_subtree_roots(&source, ShieldedProtocol::Ironwood, config.to_height)
            .await?,
    };

    let block_count = config
        .to_height
        .value()
        .saturating_sub(config.from_height.value())
        .saturating_add(1);
    let manifest = FixtureManifest {
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(config.network).to_owned(),
        from_height: config.from_height.value(),
        to_height: config.to_height.value(),
        block_count,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        tip_hash_hex,
        network_upgrade_activations: activation_records,
        segments,
        subtree_roots,
    };
    manifest.write(&config.output_directory)?;
    Ok(manifest)
}

struct CapturedSegments {
    segments: Vec<SegmentDescriptor>,
    tip_hash_hex: String,
}

async fn capture_segments(
    source: &ZebraJsonRpcSource,
    config: &CaptureConfig,
) -> Result<CapturedSegments, BenchError> {
    let mut segments = Vec::new();
    let mut tip_hash_hex = String::new();
    let mut segment_index = 0_u32;
    let mut segment_from = config.from_height.value();
    while segment_from <= config.to_height.value() {
        let segment_to = segment_end_height(segment_from, config.segment_blocks, config.to_height);
        let blocks =
            fetch_segment_blocks(source, segment_from, segment_to, config.fetch_concurrency)
                .await?;
        if let Some(last) = blocks.last()
            && last.height.value() == config.to_height.value()
        {
            tip_hash_hex = hex::encode(last.hash.as_bytes());
        }
        let descriptor = write_segment(&config.output_directory, segment_index, &blocks)?;
        tracing::info!(
            target: "zinder::bench",
            event = "segment_captured",
            index = descriptor.index,
            from_height = descriptor.from_height,
            to_height = descriptor.to_height,
            block_count = descriptor.block_count,
            "captured source segment"
        );
        segments.push(descriptor);
        segment_index = segment_index.saturating_add(1);
        segment_from = segment_to.saturating_add(1);
    }
    Ok(CapturedSegments {
        segments,
        tip_hash_hex,
    })
}

const fn segment_end_height(
    segment_from: u32,
    segment_blocks: NonZeroU32,
    to_height: BlockHeight,
) -> u32 {
    let last = segment_from.saturating_add(segment_blocks.get().saturating_sub(1));
    if last < to_height.value() {
        last
    } else {
        to_height.value()
    }
}

async fn fetch_segment_blocks(
    source: &ZebraJsonRpcSource,
    segment_from: u32,
    segment_to: u32,
    fetch_concurrency: NonZeroU32,
) -> Result<Vec<SourceBlock>, BenchError> {
    let heights = segment_from..=segment_to;
    let ordered = futures_util::stream::iter(heights)
        .map(|height| source.fetch_block_at(BlockHeight::new(height)))
        .buffered(fetch_concurrency.get() as usize)
        .collect::<Vec<_>>()
        .await;
    let mut blocks = Vec::with_capacity(ordered.len());
    for outcome in ordered {
        blocks.push(outcome?);
    }
    Ok(blocks)
}

async fn capture_subtree_roots(
    source: &ZebraJsonRpcSource,
    protocol: ShieldedProtocol,
    to_height: BlockHeight,
) -> Result<Vec<SubtreeRootRecord>, BenchError> {
    let Some(page_size) = NonZeroU32::new(SUBTREE_ROOT_PAGE_SIZE) else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let mut next_index = 0_u32;
    loop {
        let response = source
            .fetch_subtree_roots(protocol, SubtreeRootIndex::new(next_index), page_size)
            .await?;
        let returned = response.subtree_roots.len();
        if returned == 0 {
            break;
        }
        let mut passed_range = false;
        for root in response.subtree_roots {
            if root.completing_block_height.value() > to_height.value() {
                passed_range = true;
                break;
            }
            records.push(SubtreeRootRecord {
                index: root.subtree_index.value(),
                root_hash_hex: hex::encode(root.root_hash.as_bytes()),
                completing_height: root.completing_block_height.value(),
            });
            next_index = root.subtree_index.value().saturating_add(1);
        }
        if passed_range || returned < page_size.get() as usize {
            break;
        }
    }
    Ok(records)
}
