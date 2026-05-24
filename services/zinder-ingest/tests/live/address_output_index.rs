#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live regtest assertion that the typed
//! `WalletQueryApi::address_output_index` surfaces UTXOs paid to the
//! seed-derived test address after a fresh backfill.
//!
//! # Operator precondition
//!
//! Zebra's regtest sidecar must mine to the test address derived from
//! [`BROADCAST_TEST_SEED`] (see `live::mempool_broadcast_cycle`). The exact
//! address is logged by the test on startup.

use std::num::NonZeroU32;
use std::sync::Arc;

use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use zinder_core::TransparentAddressScriptHash;
use zinder_core::{BlockHeight, Network};
use zinder_ingest::backfill;
use zinder_query::{AddressOutputIndexRequest, WalletQuery, WalletQueryApi};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::TransparentTestKey;
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_backfill_config,
    regtest_generate_blocks, zebra_source_from_backfill,
};

const BROADCAST_TEST_SEED: [u8; 32] = [0x42_u8; 32];

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn address_output_index_surface_through_typed_wallet_query() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };
    let test_key = TransparentTestKey::from_seed(&BROADCAST_TEST_SEED)
        .map_err(|error| eyre!("could not derive test key: {error}"))?;
    let test_address = test_key.address_base58();
    tracing::info!(
        target: "zinder::live",
        event = "address_output_index_test_address",
        address = %test_address,
        "regtest UTXO query: configure mining.miner_address to this value"
    );

    // Mine enough blocks that the seed-derived address has at least one
    // confirmed coinbase before backfill.
    let tip_before = fetch_live_tip_height(&env).await?;
    if tip_before.value() == 0 {
        regtest_generate_blocks(&env, 1).await?;
    }
    let tip_height = fetch_live_tip_height(&env).await?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    let backfill_config = live_backfill_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        tip_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    backfill(&backfill_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed backfill outcome"))?;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let wallet_query = WalletQuery::new(store, (), activations);
    let mut hasher = Sha256::new();
    hasher.update(test_key.address_script_bytes());
    let address_script_hash = TransparentAddressScriptHash::from_bytes(hasher.finalize().into());
    let response = wallet_query
        .address_output_index(
            AddressOutputIndexRequest {
                address_script_hash,
                start_height: BlockHeight::new(0),
                max_entries: NonZeroU32::MIN.saturating_add(99),
                from_cursor: None,
            },
            None,
        )
        .await?;

    assert!(
        !response.outputs.is_empty(),
        "expected at least one UTXO paid to {test_address}; ensure Zebra mines its coinbase to that address"
    );
    for utxo in &response.outputs {
        assert_eq!(utxo.address_script_hash, address_script_hash);
    }
    Ok(())
}
