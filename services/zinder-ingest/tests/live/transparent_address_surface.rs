#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Network-agnostic acceptance for the transparent-UTXO + tx-history
//! surfaces.
//!
//! The test bulk-catches-up a small window ending at the upstream tip on whatever
//! network the operator points at (regtest, testnet, or mainnet), samples the
//! tip block's first transparent coinbase output, derives its
//! `address_script_hash` with the same `SHA-256(scriptPubKey)` rule the ingest
//! pipeline uses, and asserts that:
//!
//! - `WalletQueryApi::transparent_address_unspent_outputs` returns a UTXO whose outpoint,
//!   `script_pub_key`, value, and block fields match the sampled output;
//! - `WalletQueryApi::transparent_address_tx_ids_in_range` returns the same
//!   transaction id in ascending order, and the descending response returns the
//!   reversed list of the same artifacts;
//!
//! Mainnet runs require explicit opt-in via `ZINDER_NETWORK=zcash-mainnet`
//! and `workflow_dispatch` in CI; the runtime gate is
//! [`require_live_for`].

use std::{num::NonZeroU32, sync::Arc};

use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{
    BlockHash, BlockHeight, Network, NetworkUpgradeActivations, TransactionId,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TransparentUnspentOutput,
};
use zinder_ingest::{
    DeriveReplayPolicy, IngestDeriveConfig, catch_up_derive_store_to_canonical,
    open_primary_derive_store_for_canonical, run_bulk_catchup,
};
use zinder_query::{
    TransparentAddressTxIdsInRangeRequest, TransparentAddressUnspentOutputsRequest, WalletQuery,
    WalletQueryApi,
};
use zinder_source::{NodeSource, SourceBlock};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    zebra_source_from_bulk_catchup,
};

/// Number of blocks below the tip to bulk catchup.
///
/// Small enough to keep the test under a minute against mainnet; large enough
/// that the sampled coinbase has crossed the safe tip by the time the wallet API
/// reads it back.
const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn sampled_coinbase_address_round_trips_through_transparent_address_apis() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let network = env.network();
    let (storage_path_owner, store, sample, activations) =
        bulk_catchup_and_sample_tip_coinbase(&env).await?;
    let storage_path = storage_path_owner.path().join("zinder-store");
    let derive_secondary = zinder_derive::DeriveStore::open_secondary(
        zinder_derive::DeriveStore::path_for_canonical(&storage_path),
        storage_path_owner
            .path()
            .join("zinder-derive-secondary-history"),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumers: zinder_derive::DeriveStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    derive_secondary.try_catch_up()?;
    let wallet_query = WalletQuery::new(store, (), activations).with_derive_store(derive_secondary);

    assert_utxo_round_trip(&wallet_query, &sample).await?;
    assert_tx_history_round_trip(&wallet_query, &sample).await?;
    assert_tx_history_descending_matches_ascending(&wallet_query, &sample).await?;

    tracing::info!(
        target: "zinder::live",
        event = "transparent_address_surface_validated",
        network = %encode_zinder_native_chain_name(network),
        height = sample.block_height.value(),
        "transparent address surface validated against live node"
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct SampledCoinbase {
    address_script_hash: TransparentAddressScriptHash,
    script_pub_key: Vec<u8>,
    transaction_id: TransactionId,
    output_index: u32,
    value_zat: u64,
    block_height: BlockHeight,
    block_hash: BlockHash,
    bulk_catchup_from_height: BlockHeight,
    tip_height: BlockHeight,
}

async fn bulk_catchup_and_sample_tip_coinbase(
    env: &LiveTestEnv,
) -> Result<(
    tempfile::TempDir,
    PrimaryChainStore,
    SampledCoinbase,
    Arc<NetworkUpgradeActivations>,
)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}; \
             upstream node is not synced or {network} is too young",
            tip_height.value(),
            network = encode_zinder_native_chain_name(env.network()),
        ));
    }
    let checkpoint_height = BlockHeight::new(tip_height.value() - BACKFILL_DEPTH_BLOCKS - 1);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let activations = fetch_live_network_upgrade_activations(env).await?;
    let mut bulk_catchup_config = live_bulk_catchup_run_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_bulk_catchup(&bulk_catchup_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    bulk_catchup_config.checkpoint = Some(checkpoint);
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;

    let block_at_tip = source.fetch_block_at(tip_height).await?;
    let sample = sample_first_coinbase_output(&block_at_tip, from_height, tip_height)?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let derive_primary = open_primary_derive_store_for_canonical(
        &storage_path,
        zinder_store::RocksDbResourceBudget::for_local_tests(),
    )?;
    catch_up_derive_store_to_canonical(&store, &derive_primary, derive_replay_config()?).await?;
    drop(derive_primary);
    Ok((tempdir, store, sample, activations))
}

/// Builds the one-shot derive replay configuration used to populate the
/// derive primary before the transparent-history reader attaches its
/// secondary.
fn derive_replay_config() -> Result<IngestDeriveConfig> {
    Ok(IngestDeriveConfig {
        replay_batch_blocks: NonZeroU32::new(500)
            .ok_or_else(|| eyre!("invalid derive replay batch"))?,
        min_replay_batch_blocks: NonZeroU32::new(10)
            .ok_or_else(|| eyre!("invalid minimum derive replay batch"))?,
        replay_policy: DeriveReplayPolicy::Continuous,
        memory_budget_bytes: None,
        memory_degrade_ratio: 0.85,
        memory_pause_ratio: 0.95,
        memory_resume_ratio: 0.75,
    })
}

fn sample_first_coinbase_output(
    block: &SourceBlock,
    bulk_catchup_from_height: BlockHeight,
    tip_height: BlockHeight,
) -> Result<SampledCoinbase> {
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
    let mut hasher = Sha256::new();
    hasher.update(&script_pub_key);
    let address_script_hash = TransparentAddressScriptHash::from_bytes(hasher.finalize().into());
    Ok(SampledCoinbase {
        address_script_hash,
        script_pub_key,
        transaction_id: TransactionId::from_bytes(coinbase_tx.hash().0),
        output_index: 0,
        value_zat,
        block_height: block.height,
        block_hash: block.hash,
        bulk_catchup_from_height,
        tip_height,
    })
}

async fn assert_utxo_round_trip(
    wallet_query: &WalletQuery<PrimaryChainStore>,
    sample: &SampledCoinbase,
) -> Result<()> {
    let response = wallet_query
        .transparent_address_unspent_outputs(
            TransparentAddressUnspentOutputsRequest {
                address_script_hash: sample.address_script_hash,
                start_height: sample.bulk_catchup_from_height,
            },
            None,
        )
        .await?;
    let matched = response
        .outputs
        .iter()
        .find(|utxo| {
            utxo.outpoint.transaction_id == sample.transaction_id
                && utxo.outpoint.output_index == sample.output_index
        })
        .ok_or_else(|| {
            eyre!(
                "sampled coinbase output is absent from the UTXO response; \
                 returned_count={} sample_height={}",
                response.outputs.len(),
                sample.block_height.value(),
            )
        })?;
    assert_eq!(matched.address_script_hash, sample.address_script_hash);
    assert_eq!(matched.script_pub_key, sample.script_pub_key);
    assert_eq!(matched.value_zat, sample.value_zat);
    assert_eq!(matched.block_height, sample.block_height);
    assert_eq!(matched.block_hash, sample.block_hash);
    assert_response_addresses_are_uniform(&response.outputs, sample.address_script_hash)?;
    Ok(())
}

async fn assert_tx_history_round_trip(
    wallet_query: &WalletQuery<PrimaryChainStore>,
    sample: &SampledCoinbase,
) -> Result<()> {
    let response = wallet_query
        .transparent_address_tx_ids_in_range(TransparentAddressTxIdsInRangeRequest {
            address_script_hash: sample.address_script_hash,
            start_height: sample.bulk_catchup_from_height,
            end_height: sample.tip_height,
            max_entries: NonZeroU32::new(100).ok_or_else(|| eyre!("invalid max entries"))?,
            descending: false,
            from_cursor: None,
        })
        .await?;
    assert!(
        response
            .artifacts
            .iter()
            .any(|artifact| artifact.transaction_id == sample.transaction_id),
        "sampled txid is absent from the tx-history response; returned_count={}",
        response.artifacts.len()
    );
    assert_history_addresses_are_uniform(&response.artifacts, sample.address_script_hash)?;
    Ok(())
}

async fn assert_tx_history_descending_matches_ascending(
    wallet_query: &WalletQuery<PrimaryChainStore>,
    sample: &SampledCoinbase,
) -> Result<()> {
    let ascending = wallet_query
        .transparent_address_tx_ids_in_range(TransparentAddressTxIdsInRangeRequest {
            address_script_hash: sample.address_script_hash,
            start_height: sample.bulk_catchup_from_height,
            end_height: sample.tip_height,
            max_entries: NonZeroU32::new(100).ok_or_else(|| eyre!("invalid max entries"))?,
            descending: false,
            from_cursor: None,
        })
        .await?;
    let descending = wallet_query
        .transparent_address_tx_ids_in_range(TransparentAddressTxIdsInRangeRequest {
            address_script_hash: sample.address_script_hash,
            start_height: sample.bulk_catchup_from_height,
            end_height: sample.tip_height,
            max_entries: NonZeroU32::new(100).ok_or_else(|| eyre!("invalid max entries"))?,
            descending: true,
            from_cursor: None,
        })
        .await?;
    assert_eq!(
        ascending.artifacts.len(),
        descending.artifacts.len(),
        "ascending and descending pages must have the same length",
    );
    for (asc, desc) in ascending
        .artifacts
        .iter()
        .zip(descending.artifacts.iter().rev())
    {
        assert_eq!(asc.transaction_id, desc.transaction_id);
        assert_eq!(asc.block_height, desc.block_height);
        assert_eq!(asc.tx_index_in_block, desc.tx_index_in_block);
    }
    Ok(())
}

fn assert_response_addresses_are_uniform(
    utxos: &[TransparentUnspentOutput],
    expected: TransparentAddressScriptHash,
) -> Result<()> {
    for utxo in utxos {
        if utxo.address_script_hash != expected {
            return Err(eyre!(
                "UTXO response contains a foreign address_script_hash; \
                 expected {expected:?}, got {:?}",
                utxo.address_script_hash
            ));
        }
    }
    Ok(())
}

fn assert_history_addresses_are_uniform(
    artifacts: &[TransparentAddressTxIndexArtifact],
    expected: TransparentAddressScriptHash,
) -> Result<()> {
    for artifact in artifacts {
        if artifact.address_script_hash != expected {
            return Err(eyre!(
                "tx-history response contains a foreign address_script_hash; \
                 expected {expected:?}, got {:?}",
                artifact.address_script_hash
            ));
        }
    }
    Ok(())
}
