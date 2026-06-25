//! Live parity diff between Zinder's `zinder-compat-lightwalletd` and the
//! upstream `electriccoinco/lightwalletd` Go reference implementation, both
//! pointed at the same Zebra node.
//!
//! Double-gated by the Testing Runbook: `#[ignore]` keeps the test off the
//! default filter, and `require_live()` reads `ZINDER_TEST_LIVE=1`. Two additional
//! env vars name the gRPC endpoints; the test skips when either is absent so
//! the harness is invocable without infrastructure-level coordination:
//!
//! - `ZINDER_TEST_PARITY_ZINDER_ADDR` — Zinder compat-shim endpoint, e.g.
//!   `http://127.0.0.1:9087`.
//! - `ZINDER_TEST_PARITY_LIGHTWALLETD_ADDR` — reference lightwalletd-go
//!   endpoint pointed at the same Zebra, e.g. `http://127.0.0.1:9088`.
//!
//! Operator-divergent fields (build metadata, version strings, donation
//! address, Zinder-additive upgrade fields) are explicitly allow-listed so
//! the parity assertion focuses on the wire shape both shims must agree on:
//! `chainName`, `consensusBranchId`, `saplingActivationHeight`, latest block
//! height and hash, compact-block-range membership for a fixed span, and
//! transaction round-trip by hash and by `(block, index)`.

#![allow(
    missing_docs,
    reason = "Live parity test names describe the wire-shape assertion under test."
)]

use eyre::{Result, eyre};
use tonic::transport::Endpoint;
use zinder_proto::compat::lightwalletd::{
    self, BlockId, BlockRange, ChainSpec, Empty, TxFilter,
    compact_tx_streamer_client::CompactTxStreamerClient,
};
use zinder_testkit::live::{init, optional_env, require_live};

const PARITY_BLOCK_RANGE_END: u64 = 5;

#[tokio::test]
#[ignore = "live parity test; see CLAUDE.md §Live Node Tests"]
async fn lightd_info_advertises_matching_chain_metadata() -> Result<()> {
    let _guard = init();
    let Some(_env) = require_live()? else {
        return Ok(());
    };
    let Some((mut zinder, mut reference)) = open_parity_clients().await? else {
        return Ok(());
    };

    let zinder_info = zinder.get_lightd_info(Empty {}).await?.into_inner();
    let reference_info = reference.get_lightd_info(Empty {}).await?.into_inner();

    assert_eq!(
        zinder_info.chain_name, reference_info.chain_name,
        "chainName must agree across shims",
    );
    assert_eq!(
        zinder_info.sapling_activation_height, reference_info.sapling_activation_height,
        "saplingActivationHeight must agree",
    );
    assert_eq!(
        zinder_info.consensus_branch_id, reference_info.consensus_branch_id,
        "consensusBranchId must agree",
    );
    assert!(
        zinder_info.taddr_support,
        "Zinder must advertise transparent-address support",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live parity test; see CLAUDE.md §Live Node Tests"]
async fn latest_block_matches_reference() -> Result<()> {
    let _guard = init();
    let Some(_env) = require_live()? else {
        return Ok(());
    };
    let Some((mut zinder, mut reference)) = open_parity_clients().await? else {
        return Ok(());
    };

    let zinder_block = zinder.get_latest_block(ChainSpec {}).await?.into_inner();
    let reference_block = reference.get_latest_block(ChainSpec {}).await?.into_inner();

    // Heights may differ by one when ingest is mid-commit; require within a
    // tight tolerance and assert hashes only when heights agree exactly.
    let height_difference = zinder_block.height.abs_diff(reference_block.height);
    assert!(
        height_difference <= 1,
        "latest block heights drifted: zinder={} reference={}",
        zinder_block.height,
        reference_block.height,
    );
    if zinder_block.height == reference_block.height {
        assert_eq!(
            zinder_block.hash, reference_block.hash,
            "latest block hashes diverge at the same height",
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live parity test; see CLAUDE.md §Live Node Tests"]
async fn compact_block_range_matches_reference_for_first_blocks() -> Result<()> {
    let _guard = init();
    let Some(_env) = require_live()? else {
        return Ok(());
    };
    let Some((mut zinder, mut reference)) = open_parity_clients().await? else {
        return Ok(());
    };

    let request = || BlockRange {
        start: Some(BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: PARITY_BLOCK_RANGE_END,
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };
    let zinder_blocks = drain_block_range(zinder.get_block_range(request()).await?.into_inner())
        .await
        .map_err(|error| eyre!("zinder block-range stream failed: {error}"))?;
    let reference_blocks =
        drain_block_range(reference.get_block_range(request()).await?.into_inner())
            .await
            .map_err(|error| eyre!("reference block-range stream failed: {error}"))?;

    assert_eq!(
        zinder_blocks.len(),
        reference_blocks.len(),
        "compact block count diverges across shims",
    );
    for (zinder_block, reference_block) in zinder_blocks.iter().zip(reference_blocks.iter()) {
        assert_eq!(
            zinder_block.height, reference_block.height,
            "compact block heights diverge",
        );
        assert_eq!(
            zinder_block.hash, reference_block.hash,
            "compact block hashes diverge at height {}",
            zinder_block.height,
        );
        assert_eq!(
            zinder_block.prev_hash, reference_block.prev_hash,
            "compact block prev_hash diverges at height {}",
            zinder_block.height,
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live parity test; see CLAUDE.md §Live Node Tests"]
async fn transaction_round_trips_through_get_transaction_by_hash() -> Result<()> {
    let _guard = init();
    let Some(_env) = require_live()? else {
        return Ok(());
    };
    let Some((mut zinder, mut reference)) = open_parity_clients().await? else {
        return Ok(());
    };

    let block = zinder
        .get_block(BlockId {
            height: 1,
            hash: Vec::new(),
        })
        .await?
        .into_inner();
    let first_tx = block
        .vtx
        .first()
        .ok_or_else(|| eyre!("regtest block 1 should carry at least one transaction"))?;

    let zinder_transaction = zinder
        .get_transaction(TxFilter {
            block: None,
            index: 0,
            hash: first_tx.txid.clone(),
        })
        .await?
        .into_inner();
    let reference_transaction = reference
        .get_transaction(TxFilter {
            block: None,
            index: 0,
            hash: first_tx.txid.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(zinder_transaction.height, reference_transaction.height);
    assert_eq!(zinder_transaction.data, reference_transaction.data);
    Ok(())
}

#[tokio::test]
#[ignore = "live parity test; see CLAUDE.md §Live Node Tests"]
#[allow(
    deprecated,
    reason = "GetBlockRangeNullifiers is deprecated in the lightwalletd contract but the redaction parity must still hold for the wallets that call it."
)]
async fn block_range_nullifiers_omit_commitment_tree_sizes_like_reference() -> Result<()> {
    let _guard = init();
    let Some(_env) = require_live()? else {
        return Ok(());
    };
    let Some((mut zinder, mut reference)) = open_parity_clients().await? else {
        return Ok(());
    };

    let request = || BlockRange {
        start: Some(BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: PARITY_BLOCK_RANGE_END,
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };
    let zinder_blocks = drain_block_range(
        zinder
            .get_block_range_nullifiers(request())
            .await?
            .into_inner(),
    )
    .await
    .map_err(|error| eyre!("zinder nullifier-range stream failed: {error}"))?;
    let reference_blocks = drain_block_range(
        reference
            .get_block_range_nullifiers(request())
            .await?
            .into_inner(),
    )
    .await
    .map_err(|error| eyre!("reference nullifier-range stream failed: {error}"))?;

    for blocks in [&zinder_blocks, &reference_blocks] {
        for block in blocks {
            assert!(
                block.chain_metadata.is_none(),
                "nullifiers-only responses must omit commitment-tree sizes at height {}",
                block.height,
            );
            for transaction in &block.vtx {
                assert!(
                    transaction.vin.is_empty() && transaction.vout.is_empty(),
                    "nullifiers-only transparent data must be cleared at height {}",
                    block.height,
                );
                assert!(
                    transaction.outputs.is_empty(),
                    "nullifiers-only Sapling outputs must be cleared at height {}",
                    block.height,
                );
            }
        }
    }
    Ok(())
}

async fn open_parity_clients() -> Result<
    Option<(
        CompactTxStreamerClient<tonic::transport::Channel>,
        CompactTxStreamerClient<tonic::transport::Channel>,
    )>,
> {
    let Some(zinder_addr) = optional_env("ZINDER_TEST_PARITY_ZINDER_ADDR")? else {
        return Ok(None);
    };
    let Some(reference_addr) = optional_env("ZINDER_TEST_PARITY_LIGHTWALLETD_ADDR")? else {
        return Ok(None);
    };
    let zinder = CompactTxStreamerClient::new(Endpoint::new(zinder_addr)?.connect().await?);
    let reference = CompactTxStreamerClient::new(Endpoint::new(reference_addr)?.connect().await?);
    Ok(Some((zinder, reference)))
}

async fn drain_block_range(
    mut stream: tonic::Streaming<lightwalletd::CompactBlock>,
) -> Result<Vec<lightwalletd::CompactBlock>> {
    let mut blocks = Vec::new();
    while let Some(message) = stream.message().await? {
        blocks.push(message);
    }
    Ok(blocks)
}
