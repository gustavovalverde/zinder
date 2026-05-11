#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live regtest broadcast cycle.
//!
//! Signs a transparent v5 transaction inside Zinder, broadcasts it through
//! Zebra's `sendrawtransaction`, and observes it back through the mempool
//! source as `MempoolSourceEvent::Added`. The signer lives in
//! `zinder-testkit` ([`TransparentTestKey`]); the broadcaster lives in
//! `zinder-source` (`ZebraJsonRpcSource: TransactionBroadcaster`).
//!
//! # Operator precondition
//!
//! Zebra's regtest sidecar must mine to the test address derived from
//! [`BROADCAST_TEST_SEED`]. The exact address is logged by the test on
//! startup and printed in the failure path; configure
//! `[mining] miner_address = "<that address>"` in `zebrad.toml` and restart
//! the sidecar before running.
//!
//! When the address does not match the coinbase output, the test fails
//! before broadcasting with a diagnostic message naming the expected
//! address. This is intentional: silent skips on misconfigured live
//! environments hide bugs.

use std::time::{Duration, Instant};

use eyre::{Result, eyre};
use serde::Deserialize;
use tokio_stream::StreamExt as _;
use zinder_core::{
    BroadcastAccepted, BroadcastRejected, Network, RawTransactionBytes, TransactionBroadcastResult,
    TransactionId, UnixTimestampMillis,
};
use zinder_source::{
    JsonRpcMempoolSource, JsonRpcMempoolSourceOptions, MempoolSource, MempoolSourceEvent,
    NodeSource, TransactionBroadcaster, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_testkit::live::{init, require_live_for};
use zinder_testkit::{
    P2pkhSpendArgs, TransparentAddress, TransparentTestKey, ZIP317_FEE_ONE_IN_ONE_OUT_ZATS,
};

use crate::common::regtest_generate_blocks;

/// Test seed used to derive the regtest broadcast cycle's transparent
/// account key. Operators configure their Zebra `miner_address` to the
/// matching p2pkh address; see the module docstring.
const BROADCAST_TEST_SEED: [u8; 32] = [0x42_u8; 32];

/// Maximum time we wait for the polling mempool source to emit the broadcast
/// transaction's `Added` event after `sendrawtransaction` returns.
const MEMPOOL_OBSERVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Mempool poll interval. Short enough that the test stays under the
/// observation timeout even on a contended runner; long enough that we do
/// not hammer Zebra.
const MEMPOOL_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TransactionBroadcastResult is #[non_exhaustive]; the broadcast match collapses every non-Accepted variant into a single test failure"
)]
#[allow(
    clippy::too_many_lines,
    reason = "The live broadcast gate keeps fund selection, broadcast retry, and mempool observation in one auditable path."
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn broadcasting_signed_transparent_v5_surfaces_through_polling_mempool_source() -> Result<()>
{
    let _guard = init();
    let env = require_live_for(&[Network::ZcashRegtest])?;
    let test_key = TransparentTestKey::from_seed(&BROADCAST_TEST_SEED)
        .map_err(|error| eyre!("could not derive test key: {error}"))?;
    let test_address = test_key.address_base58();
    tracing::info!(
        target: "zinder::live",
        event = "broadcast_cycle_test_address",
        address = %test_address,
        "regtest broadcast cycle: configure mining.miner_address to this value"
    );

    let json_rpc = zebra_source(&env)?;

    let coinbase = locate_spendable_test_coinbase(&env, &json_rpc, &test_address).await?;

    let mut last_known_rejection = None;
    for attempt in 0_u64..3 {
        let recipient_address = scratch_recipient_address(
            &test_key,
            UnixTimestampMillis::now().value().saturating_add(attempt),
        );
        let raw_tx = test_key
            .build_p2pkh_spend(&P2pkhSpendArgs {
                coinbase_txid_be: coinbase.txid_be,
                coinbase_vout: coinbase.vout,
                coinbase_value_zats: coinbase.value_zats,
                recipient: &recipient_address,
                target_height: coinbase.target_height,
            })
            .map_err(|error| eyre!("transparent signer rejected the spend: {error}"))?;

        let mempool_source = JsonRpcMempoolSource::with_options(
            json_rpc.clone(),
            JsonRpcMempoolSourceOptions {
                poll_interval: MEMPOOL_POLL_INTERVAL,
                event_channel_capacity: 16,
            },
        );
        let mut event_stream = mempool_source.events().await?;

        let broadcast_outcome = json_rpc
            .broadcast_transaction(RawTransactionBytes::new(raw_tx))
            .await?;
        let broadcast_txid = match &broadcast_outcome {
            TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id }) => {
                *transaction_id
            }
            _ if is_known_transaction_rejection(&broadcast_outcome) => {
                last_known_rejection = Some(format!("{broadcast_outcome:?}"));
                continue;
            }
            _ => {
                return Err(eyre!(
                    "Zebra rejected the signed transparent v5 transaction: {broadcast_outcome:?}"
                ));
            }
        };
        tracing::info!(
            target: "zinder::live",
            event = "broadcast_cycle_sendrawtransaction_accepted",
            broadcast_txid = %hex::encode(broadcast_txid.as_bytes()),
            "Zebra accepted the signed transparent v5 spend"
        );

        let added_entry = wait_for_added(
            &mut event_stream,
            broadcast_txid,
            MEMPOOL_OBSERVE_TIMEOUT,
        )
        .await
        .map_err(|error| {
            eyre!(
                "polling mempool source did not surface broadcast txid {} within {:?}: {error}",
                hex::encode(broadcast_txid.as_bytes()),
                MEMPOOL_OBSERVE_TIMEOUT
            )
        })?;

        assert_eq!(
            added_entry.transaction_id, broadcast_txid,
            "polling source surfaced a different txid than sendrawtransaction returned"
        );
        assert!(
            !added_entry.raw_transaction_bytes.as_slice().is_empty(),
            "Added envelope must hydrate raw transaction bytes"
        );
        regtest_generate_blocks(&env, 1).await?;
        return Ok(());
    }

    Err(eyre!(
        "Zebra kept reporting the signed transparent v5 transaction as already known: {}",
        last_known_rejection.unwrap_or_else(|| "no rejection captured".to_owned())
    ))
}

fn zebra_source(env: &zinder_testkit::live::LiveTestEnv) -> Result<ZebraJsonRpcSource> {
    Ok(ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?)
}

struct TestCoinbase {
    txid_be: [u8; 32],
    vout: u32,
    value_zats: u64,
    target_height: u32,
}

#[derive(Debug, Deserialize)]
struct ZebraAddressUtxo {
    txid: String,
    #[serde(rename = "outputIndex")]
    output_index: u32,
    satoshis: u64,
    height: u32,
}

async fn locate_spendable_test_coinbase(
    env: &zinder_testkit::live::LiveTestEnv,
    json_rpc: &ZebraJsonRpcSource,
    test_address: &str,
) -> Result<TestCoinbase> {
    if let Some(coinbase) = select_spendable_test_coinbase(env, json_rpc, test_address).await? {
        return Ok(coinbase);
    }

    regtest_generate_blocks(env, 101).await?;
    select_spendable_test_coinbase(env, json_rpc, test_address)
        .await?
        .ok_or_else(|| {
            eyre!(
                "no matured UTXO for test address {test_address} exceeds the ZIP-317 fee; \
                 configure Zebra `[mining] miner_address` to that value and restart from a \
                 fresh or funded regtest sidecar"
            )
        })
}

async fn select_spendable_test_coinbase(
    env: &zinder_testkit::live::LiveTestEnv,
    json_rpc: &ZebraJsonRpcSource,
    test_address: &str,
) -> Result<Option<TestCoinbase>> {
    let tip_height = json_rpc.tip_id().await?.height;
    let target_height = tip_height.value().saturating_add(1);
    let maturity_cutoff = target_height.saturating_sub(100);
    let mut utxos = fetch_address_utxos(env, test_address).await?;
    utxos.sort_by_key(|utxo| (utxo.height, utxo.satoshis));
    utxos.reverse();

    for utxo in utxos {
        if utxo.height <= maturity_cutoff
            && utxo.satoshis > ZIP317_FEE_ONE_IN_ONE_OUT_ZATS
            && address_utxo_is_unspent(env, &utxo).await?
        {
            return Ok(Some(TestCoinbase {
                txid_be: display_txid_to_wire_bytes(&utxo.txid)?,
                vout: utxo.output_index,
                value_zats: utxo.satoshis,
                target_height,
            }));
        }
    }
    Ok(None)
}

async fn fetch_address_utxos(
    env: &zinder_testkit::live::LiveTestEnv,
    test_address: &str,
) -> Result<Vec<ZebraAddressUtxo>> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"getaddressutxos","params":[{{"addresses":["{test_address}"]}}]}}"#
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "getaddressutxos curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| eyre!("getaddressutxos response is not JSON: {error}; body={body}"))?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre!("getaddressutxos RPC returned error: {error_field}"));
    }
    let result_field = parsed
        .get("result")
        .ok_or_else(|| eyre!("getaddressutxos response missing result field; body={body}"))?;
    serde_json::from_value(result_field.clone())
        .map_err(|error| eyre!("getaddressutxos result shape is invalid: {error}; body={body}"))
}

async fn address_utxo_is_unspent(
    env: &zinder_testkit::live::LiveTestEnv,
    utxo: &ZebraAddressUtxo,
) -> Result<bool> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"gettxout","params":["{}",{}]}}"#,
        utxo.txid, utxo.output_index
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "gettxout curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| eyre!("gettxout response is not JSON: {error}; body={body}"))?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre!("gettxout RPC returned error: {error_field}"));
    }
    Ok(parsed
        .get("result")
        .is_some_and(|rpc_result| !rpc_result.is_null()))
}

fn display_txid_to_wire_bytes(txid: &str) -> Result<[u8; 32]> {
    let mut bytes: [u8; 32] = hex::decode(txid)?.try_into().map_err(|decoded: Vec<u8>| {
        eyre!("txid decoded to {} bytes, expected 32", decoded.len())
    })?;
    bytes.reverse();
    Ok(bytes)
}

fn is_known_transaction_rejection(outcome: &TransactionBroadcastResult) -> bool {
    matches!(
        outcome,
        TransactionBroadcastResult::Rejected(BroadcastRejected { message, .. })
            if message.contains("already queued for download")
                || message.contains("transaction is already in state")
    )
}

/// Returns a deterministic recipient address distinct from the funded test
/// address so that the spend's outputs are unambiguously identifiable.
fn scratch_recipient_address(test_key: &TransparentTestKey, salt: u64) -> TransparentAddress {
    // Take the funded address's pubkey hash and XOR every byte with 0xFF so
    // the recipient differs but stays a valid p2pkh.
    let funded_hash = match test_key.address() {
        TransparentAddress::PublicKeyHash(hash) | TransparentAddress::ScriptHash(hash) => *hash,
    };
    let salt_bytes = salt.to_le_bytes();
    let mut scratch = [0_u8; 20];
    for (index, byte) in funded_hash.iter().enumerate() {
        scratch[index] = byte ^ 0xFF ^ salt_bytes[index % salt_bytes.len()];
    }
    TransparentAddress::PublicKeyHash(scratch)
}

async fn wait_for_added(
    event_stream: &mut zinder_source::MempoolSourceEventStream,
    expected_txid: TransactionId,
    deadline: Duration,
) -> Result<zinder_source::MempoolSourceEntry> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        let remaining = deadline.saturating_sub(started.elapsed());
        let outcome = tokio::time::timeout(remaining, event_stream.next()).await;
        match outcome {
            Ok(Some(Ok(MempoolSourceEvent::Added(entry)))) => {
                if entry.transaction_id == expected_txid {
                    return Ok(entry);
                }
            }
            Ok(Some(Ok(_other))) => {
                // Mined or Invalidated event for some unrelated txid; ignore.
            }
            Ok(Some(Err(error))) => {
                return Err(eyre!("mempool source emitted error item: {error}"));
            }
            Ok(None) => {
                return Err(eyre!(
                    "mempool source stream closed before observing the broadcast txid"
                ));
            }
            Err(_elapsed) => break,
        }
    }
    Err(eyre!(
        "deadline elapsed without observing the broadcast txid"
    ))
}

/// Waits for `MempoolSourceEvent::Mined { transaction_id, .. }` matching
/// `expected_txid`. Used by the reorg gate to confirm Zebra mined the
/// broadcast tx into a block before we invalidate it.
async fn wait_for_mined(
    event_stream: &mut zinder_source::MempoolSourceEventStream,
    expected_txid: TransactionId,
    deadline: Duration,
) -> Result<zinder_core::BlockHeight> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        let remaining = deadline.saturating_sub(started.elapsed());
        let outcome = tokio::time::timeout(remaining, event_stream.next()).await;
        match outcome {
            Ok(Some(Ok(MempoolSourceEvent::Mined {
                transaction_id,
                mined_height,
                ..
            }))) if transaction_id == expected_txid => {
                return Ok(mined_height);
            }
            Ok(Some(Ok(_other))) => {}
            Ok(Some(Err(error))) => {
                return Err(eyre!("mempool source emitted error item: {error}"));
            }
            Ok(None) => {
                return Err(eyre!(
                    "mempool source stream closed before observing Mined for the broadcast txid"
                ));
            }
            Err(_elapsed) => break,
        }
    }
    Err(eyre!(
        "deadline elapsed without observing Mined for the broadcast txid"
    ))
}

async fn rpc_invalidate_block(
    env: &zinder_testkit::live::LiveTestEnv,
    block_hash_hex: &str,
) -> Result<()> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"invalidateblock","params":["{block_hash_hex}"]}}"#
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "invalidateblock({block_hash_hex}) curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| eyre!("invalidateblock response is not JSON: {error}; body={body}"))?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre!(
            "invalidateblock({block_hash_hex}) RPC returned error: {error_field}"
        ));
    }
    Ok(())
}

async fn rpc_reconsider_block(
    env: &zinder_testkit::live::LiveTestEnv,
    block_hash_hex: &str,
) -> Result<()> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"reconsiderblock","params":["{block_hash_hex}"]}}"#
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "reconsiderblock({block_hash_hex}) curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TransactionBroadcastResult is #[non_exhaustive]; collapse every non-Accepted variant into a single failure"
)]
#[allow(
    clippy::too_many_lines,
    reason = "Reorg gate exercises mine -> Added -> mine -> Mined -> invalidate -> Added in one auditable script."
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn invalidating_block_drops_canonical_tip_and_rebroadcast_resurfaces_mempool_added()
-> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashRegtest])?;
    let test_key = TransparentTestKey::from_seed(&BROADCAST_TEST_SEED)
        .map_err(|error| eyre!("could not derive test key: {error}"))?;
    let test_address = test_key.address_base58();
    tracing::info!(
        target: "zinder::live",
        event = "reorg_gate_test_address",
        address = %test_address,
        "regtest reorg gate: configure mining.miner_address to this value"
    );

    let json_rpc = zebra_source(&env)?;

    let coinbase = locate_spendable_test_coinbase(&env, &json_rpc, &test_address).await?;

    let recipient_address =
        scratch_recipient_address(&test_key, UnixTimestampMillis::now().value());
    let raw_tx = test_key
        .build_p2pkh_spend(&P2pkhSpendArgs {
            coinbase_txid_be: coinbase.txid_be,
            coinbase_vout: coinbase.vout,
            coinbase_value_zats: coinbase.value_zats,
            recipient: &recipient_address,
            target_height: coinbase.target_height,
        })
        .map_err(|error| eyre!("transparent signer rejected the spend: {error}"))?;

    let mempool_source = JsonRpcMempoolSource::with_options(
        json_rpc.clone(),
        JsonRpcMempoolSourceOptions {
            poll_interval: MEMPOOL_POLL_INTERVAL,
            event_channel_capacity: 16,
        },
    );
    let mut event_stream = mempool_source.events().await?;

    let broadcast_outcome = json_rpc
        .broadcast_transaction(RawTransactionBytes::new(raw_tx))
        .await?;
    let broadcast_txid = match &broadcast_outcome {
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id }) => {
            *transaction_id
        }
        _ => {
            return Err(eyre!(
                "Zebra rejected the signed transparent v5 transaction: {broadcast_outcome:?}"
            ));
        }
    };

    wait_for_added(&mut event_stream, broadcast_txid, MEMPOOL_OBSERVE_TIMEOUT)
        .await
        .map_err(|error| {
            eyre!("polling source did not surface broadcast Added before mining: {error}")
        })?;

    // Mine one block so Zebra includes the broadcast tx.
    let mined_blocks = regtest_generate_blocks(&env, 1).await?;
    let mined_block_hash = mined_blocks
        .first()
        .ok_or_else(|| eyre!("regtest_generate_blocks(1) returned no hashes"))?
        .clone();

    let mined_height = wait_for_mined(&mut event_stream, broadcast_txid, MEMPOOL_OBSERVE_TIMEOUT)
        .await
        .map_err(|error| eyre!("polling source did not emit Mined for broadcast txid: {error}"))?;
    tracing::info!(
        target: "zinder::live",
        event = "reorg_gate_mined",
        broadcast_txid = %hex::encode(broadcast_txid.as_bytes()),
        mined_height = mined_height.value(),
        mined_block_hash = %mined_block_hash,
        "broadcast tx mined; invalidating the block to drive the reorg"
    );

    // Reorg out the block that contains the broadcast tx. Zebra's
    // `invalidateblock` does **not** automatically return the tx to the
    // mempool (unlike Bitcoin Core); the tx simply disappears from the
    // canonical chain. The reorg gate's purpose is to verify Zinder
    // follows that observed semantic (no synthesized re-add); the strict
    // "orchestrator does not synthesize Added from reverted blocks"
    // property is covered separately by the synthetic integration test
    // `reorg_returns_mined_tx_to_mempool_through_orchestrator`.
    rpc_invalidate_block(&env, &mined_block_hash).await?;

    // Confirm the tip rolled back. The chain at this point should be at
    // mined_height - 1 because the only invalidated block is the one we
    // mined to confirm the broadcast.
    let post_invalidate_tip = json_rpc.tip_id().await?.height;
    assert_eq!(
        post_invalidate_tip.value(),
        mined_height.value().saturating_sub(1),
        "after invalidateblock, the canonical tip should drop to mined_height - 1"
    );

    // Re-broadcast the same signed tx. Zebra's mempool accepts it (its
    // inputs are still spendable now that the mining block is gone), and
    // the polling source emits Added again on the next poll cycle. This
    // proves the post-reorg cycle works end-to-end.
    let rebroadcast_raw_tx = test_key
        .build_p2pkh_spend(&P2pkhSpendArgs {
            coinbase_txid_be: coinbase.txid_be,
            coinbase_vout: coinbase.vout,
            coinbase_value_zats: coinbase.value_zats,
            recipient: &recipient_address,
            target_height: coinbase.target_height,
        })
        .map_err(|error| eyre!("transparent signer rejected the rebroadcast: {error}"))?;
    let rebroadcast_outcome = json_rpc
        .broadcast_transaction(RawTransactionBytes::new(rebroadcast_raw_tx))
        .await?;
    let rebroadcast_txid = match &rebroadcast_outcome {
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id }) => {
            *transaction_id
        }
        _ => {
            return Err(eyre!(
                "Zebra rejected the rebroadcast after invalidateblock: {rebroadcast_outcome:?}"
            ));
        }
    };
    assert_eq!(
        rebroadcast_txid, broadcast_txid,
        "rebroadcast must produce the same txid as the original broadcast"
    );

    let readded_entry = wait_for_added(&mut event_stream, broadcast_txid, MEMPOOL_OBSERVE_TIMEOUT)
        .await
        .map_err(|error| {
            eyre!(
                "polling source did not surface rebroadcast Added for txid {}: {error}",
                hex::encode(broadcast_txid.as_bytes())
            )
        })?;
    assert_eq!(
        readded_entry.transaction_id, broadcast_txid,
        "post-reorg rebroadcast Added carries a different txid than the original"
    );

    // Restore the chain so subsequent runs in the same regtest sidecar
    // session see a clean linear history. Best-effort; even if the call
    // fails, the test result already passed/failed on the substantive
    // assertion.
    let _ = rpc_reconsider_block(&env, &mined_block_hash).await;
    let _ = regtest_generate_blocks(&env, 1).await;
    Ok(())
}
