#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live regtest broadcast cycle.
//!
//! Closes M3's last gap: a transparent v5 transaction signed inside Zinder,
//! broadcast through Zebra's `sendrawtransaction`, and observed back through
//! the mempool source as `MempoolSourceEvent::Added`. The signer lives in
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
use tokio_stream::StreamExt as _;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::{
    BroadcastAccepted, Network, RawTransactionBytes, TransactionBroadcastResult, TransactionId,
};
use zinder_source::{
    JsonRpcMempoolSource, JsonRpcMempoolSourceOptions, MempoolSource, MempoolSourceEvent,
    NodeSource, TransactionBroadcaster, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_testkit::live::{init, require_live_for};
use zinder_testkit::{P2pkhSpendArgs, TransparentAddress, TransparentTestKey};

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

    // Snapshot the tip before mining so we know which block to inspect for
    // the test address's coinbase. This lets the test work against a regtest
    // sidecar that already has a non-genesis chain state.
    let tip_before = json_rpc.tip_id().await?.height;

    // Mine 101 blocks so the next block's coinbase has 100 confirmations and
    // is spendable in a tx targeting `tip_before + 102`.
    regtest_generate_blocks(&env, 101).await?;

    let funded_height = zinder_core::BlockHeight::new(tip_before.value() + 1);
    let target_height = tip_before.value() + 102;
    let coinbase = locate_test_coinbase(&json_rpc, &test_key, &test_address, funded_height).await?;

    let recipient_address = scratch_recipient_address(&test_key);
    let raw_tx = test_key
        .build_p2pkh_spend(&P2pkhSpendArgs {
            coinbase_txid_be: coinbase.txid_be,
            coinbase_vout: coinbase.vout,
            coinbase_value_zats: coinbase.value_zats,
            recipient: &recipient_address,
            target_height,
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
    tracing::info!(
        target: "zinder::live",
        event = "broadcast_cycle_sendrawtransaction_accepted",
        broadcast_txid = %hex::encode(broadcast_txid.as_bytes()),
        "Zebra accepted the signed transparent v5 spend"
    );

    let added_entry = wait_for_added(&mut event_stream, broadcast_txid, MEMPOOL_OBSERVE_TIMEOUT)
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
    Ok(())
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
}

async fn locate_test_coinbase(
    json_rpc: &ZebraJsonRpcSource,
    test_key: &TransparentTestKey,
    test_address: &str,
    starting_height: zinder_core::BlockHeight,
) -> Result<TestCoinbase> {
    let block = json_rpc
        .fetch_block_by_height(starting_height)
        .await
        .map_err(|error| {
            eyre!(
                "fetch_block_by_height({}) failed: {error}",
                starting_height.value()
            )
        })?;
    let parsed_block: ZebraBlock = block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| eyre!("zebra-chain block parse failed: {error}"))?;
    let coinbase_tx = parsed_block
        .transactions
        .first()
        .ok_or_else(|| eyre!("block has no coinbase transaction"))?;
    let expected_script_bytes = test_key.address_script_bytes();
    let txid_be: [u8; 32] = coinbase_tx.hash().0;

    for (vout_index, output) in coinbase_tx.outputs().iter().enumerate() {
        if output.lock_script.as_raw_bytes() == expected_script_bytes.as_slice() {
            let value_zats = u64::try_from(i64::from(output.value))
                .map_err(|error| eyre!("coinbase output value did not fit u64: {error}"))?;
            let vout = u32::try_from(vout_index)
                .map_err(|error| eyre!("coinbase output index did not fit u32: {error}"))?;
            return Ok(TestCoinbase {
                txid_be,
                vout,
                value_zats,
            });
        }
    }

    Err(eyre!(
        "block {} has no coinbase output paying to the test address {test_address}; \
         configure Zebra `[mining] miner_address` to that value and restart the sidecar so \
         coinbase outputs accumulate at the test address",
        starting_height.value()
    ))
}

/// Returns a deterministic recipient address distinct from the funded test
/// address so that the spend's outputs are unambiguously identifiable.
fn scratch_recipient_address(test_key: &TransparentTestKey) -> TransparentAddress {
    // Take the funded address's pubkey hash and XOR every byte with 0xFF so
    // the recipient differs but stays a valid p2pkh.
    let funded_hash = match test_key.address() {
        TransparentAddress::PublicKeyHash(hash) | TransparentAddress::ScriptHash(hash) => *hash,
    };
    let mut scratch = [0_u8; 20];
    for (index, byte) in funded_hash.iter().enumerate() {
        scratch[index] = byte ^ 0xFF;
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

    let tip_before = json_rpc.tip_id().await?.height;

    regtest_generate_blocks(&env, 101).await?;

    let funded_height = zinder_core::BlockHeight::new(tip_before.value() + 1);
    let target_height = tip_before.value() + 102;
    let coinbase = locate_test_coinbase(&json_rpc, &test_key, &test_address, funded_height).await?;

    let recipient_address = scratch_recipient_address(&test_key);
    let raw_tx = test_key
        .build_p2pkh_spend(&P2pkhSpendArgs {
            coinbase_txid_be: coinbase.txid_be,
            coinbase_vout: coinbase.vout,
            coinbase_value_zats: coinbase.value_zats,
            recipient: &recipient_address,
            target_height,
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
            target_height,
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
    Ok(())
}
