#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Network-agnostic live acceptance for the prevout-resolution surface.
//!
//! The test backfills a small window ending at the upstream tip on whatever
//! network the operator points at (regtest, testnet, or mainnet), samples the
//! tip block's first transparent coinbase output, and asserts that:
//!
//! - `WalletQueryApi::transparent_prevouts` resolves the sampled outpoint to a
//!   prevout whose `value_zat` and `script_pub_key` match the source-observed
//!   transaction's `vout[0]`;
//! - an unknown outpoint (random transaction id) resolves to `None`;
//! - duplicate request entries emit duplicate response entries in input order.
//!
//! The sampled "coinbase output" is just a stable, easy-to-find transparent
//! output on every network. The prevout surface resolves any
//! [`TransparentOutPoint`] to its referenced [`TransparentPrevout`]
//! regardless of whether the `OutPoint` has been spent; what consumers do
//! with the result (label it "prevout", "txout", or "utxo") is their concern.
//!
//! Mainnet runs require explicit opt-in via `ZINDER_NETWORK=zcash-mainnet`
//! and the runtime gate [`require_live_for`].

use std::num::NonZeroU32;
use std::sync::Arc;

use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::{BlockHash, BlockHeight, Network, TransactionId, TransparentOutPoint};
use zinder_ingest::{BackfillOutcome, backfill};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_source::{NodeSource, SourceBlock};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

/// Number of blocks below the tip to backfill.
///
/// Mirrors the transparent-address live test's depth: small enough to keep
/// the test under a minute against mainnet; large enough that the sampled
/// coinbase has been finalized by the time the wallet API reads it back.
const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn sampled_coinbase_outpoint_resolves_through_transparent_prevouts() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?;
    let network = env.network();
    let (storage_path_owner, store, sample) = backfill_and_sample_tip_coinbase(&env).await?;
    let _storage_path_owner = storage_path_owner;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));

    assert_known_outpoint_resolves(&wallet_query, &sample).await?;
    assert_unknown_outpoint_returns_none(&wallet_query).await?;
    assert_duplicate_outpoints_preserve_order(&wallet_query, &sample).await?;

    tracing::info!(
        target: "zinder::live",
        event = "transparent_prevouts_validated",
        network = %network.name(),
        height = sample.block_height.value(),
        "transparent prevout-resolution surface validated against live node"
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct SampledOutput {
    outpoint: TransparentOutPoint,
    script_pub_key: Vec<u8>,
    value_zat: u64,
    block_height: BlockHeight,
    block_hash: BlockHash,
}

async fn backfill_and_sample_tip_coinbase(
    env: &LiveTestEnv,
) -> Result<(tempfile::TempDir, PrimaryChainStore, SampledOutput)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}; \
             upstream node is not synced or {network} is too young",
            tip_height.value(),
            network = env.network().name(),
        ));
    }
    let checkpoint_height = BlockHeight::new(tip_height.value() - BACKFILL_DEPTH_BLOCKS - 1);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);
    let BackfillOutcome::Committed(_) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected committed backfill outcome"));
    };

    let block_at_tip = source.fetch_block_by_height(tip_height).await?;
    let sample = sample_first_coinbase_output(&block_at_tip)?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, sample))
}

fn sample_first_coinbase_output(block: &SourceBlock) -> Result<SampledOutput> {
    let zebra_block: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase_tx = zebra_block.transactions.first().ok_or_else(|| {
        eyre!(
            "block at height {} has no transactions",
            block.height.value()
        )
    })?;
    let coinbase_output = coinbase_tx.outputs().first().ok_or_else(|| {
        eyre!(
            "coinbase at height {} has no transparent outputs",
            block.height.value()
        )
    })?;
    let script_pub_key = coinbase_output.lock_script.as_raw_bytes().to_vec();
    let value_zat = u64::try_from(i64::from(coinbase_output.value))
        .map_err(|error| eyre!("coinbase output value did not fit u64: {error}"))?;
    Ok(SampledOutput {
        outpoint: TransparentOutPoint::new(TransactionId::from_bytes(coinbase_tx.hash().0), 0),
        script_pub_key,
        value_zat,
        block_height: block.height,
        block_hash: block.hash,
    })
}

async fn assert_known_outpoint_resolves(
    wallet_query: &WalletQuery<PrimaryChainStore>,
    sample: &SampledOutput,
) -> Result<()> {
    let response = wallet_query
        .transparent_prevouts(vec![sample.outpoint], None)
        .await?;
    assert_eq!(
        response.entries.len(),
        1,
        "single-outpoint request must return exactly one entry",
    );
    let entry = &response.entries[0];
    assert_eq!(entry.outpoint, sample.outpoint);
    let prevout = entry.prevout.as_ref().ok_or_else(|| {
        eyre!(
            "sampled coinbase outpoint did not resolve; height={}, hash={:?}",
            sample.block_height.value(),
            sample.block_hash,
        )
    })?;
    assert_eq!(prevout.value_zat, sample.value_zat);
    assert_eq!(prevout.script_pub_key, sample.script_pub_key);
    Ok(())
}

async fn assert_unknown_outpoint_returns_none(
    wallet_query: &WalletQuery<PrimaryChainStore>,
) -> Result<()> {
    let response = wallet_query
        .transparent_prevouts(
            vec![TransparentOutPoint::new(
                TransactionId::from_bytes(unknown_transaction_id_bytes()),
                0,
            )],
            None,
        )
        .await?;
    assert_eq!(response.entries.len(), 1);
    assert!(
        response.entries[0].prevout.is_none(),
        "unknown txid must resolve to None against the live store",
    );
    Ok(())
}

async fn assert_duplicate_outpoints_preserve_order(
    wallet_query: &WalletQuery<PrimaryChainStore>,
    sample: &SampledOutput,
) -> Result<()> {
    let unknown =
        TransparentOutPoint::new(TransactionId::from_bytes(unknown_transaction_id_bytes()), 0);
    let outpoints = vec![sample.outpoint, unknown, sample.outpoint];
    let response = wallet_query
        .transparent_prevouts(outpoints.clone(), None)
        .await?;
    assert_eq!(response.entries.len(), 3);
    for (entry, requested) in response.entries.iter().zip(outpoints.iter()) {
        assert_eq!(entry.outpoint, *requested);
    }
    assert!(response.entries[0].prevout.is_some());
    assert!(response.entries[1].prevout.is_none());
    assert_eq!(response.entries[0].prevout, response.entries[2].prevout);
    Ok(())
}

/// Returns 32 deterministic but on-chain-unlikely bytes for an "unknown" txid.
///
/// The bytes are SHA-256 of a static string seeded with the test name; the
/// preimage is documented in the function so a future debugger can confirm the
/// digest by hand.
fn unknown_transaction_id_bytes() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"zinder::live::transparent_prevouts::unknown_outpoint");
    hasher.finalize().into()
}
