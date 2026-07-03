//! Worked example: observe a transparent address across the mempool boundary.
//!
//! Pairs with [`observe_transparent_address`] to demonstrate the
//! mempool-to-confirmed handoff:
//!
//! 1. Snapshot the live mempool outputs for the watched address through
//!    [`EndpointBackedIndex::transparent_mempool_outputs_by_address`].
//! 2. Subscribe to the mempool event log via
//!    [`EndpointBackedIndex::mempool_events`] to receive
//!    `Added`/`Mined`/`Invalidated`/`Suppressed` transitions.
//! 3. Track each unconfirmed transaction id observed for the address until
//!    one of those transitions resolves it; emit one line per state change.
//!
//! Deduplication rule at the boundary: when an entry first appears via the
//! snapshot, the example records `(transaction_id, address)`. When the same
//! transaction id arrives via `MempoolEvent::Mined`, the example treats the
//! confirmation as the canonical state change and discards the unconfirmed
//! record. This mirrors the contract a wallet would implement when handing
//! the zero-conf to one-conf transition off to its persistent store.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "Worked example demonstrates console output; its target is operator-facing terminal use."
)]
//!
//! Run it after starting Zinder:
//!
//! ```bash
//! cargo run -p zinder-client --example observe_mempool_to_confirmed -- \
//!     http://127.0.0.1:9101 zcash-regtest tmDpFafuBHKGUYmuwLsrxWJrwcnSyzEEtYx
//! ```

use std::collections::HashSet;
use std::env;
use std::process::ExitCode;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use zinder_client::{
    EndpointBackedIndex, EventStreamStart, IndexerError, MempoolEvent, Network, RemoteChainIndex,
    RemoteOpenOptions, RetryPolicy, TransactionId, TransparentAddressScriptHash,
    TransparentMempoolOutputsRequest,
};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MEMPOOL_SNAPSHOT_LIMIT: u32 = 256;

#[tokio::main]
async fn main() -> ExitCode {
    let args = env::args().collect::<Vec<_>>();
    let [_, endpoint, network_name, address] =
        if let [program, endpoint, network_name, address] = args.as_slice() {
            [
                program.clone(),
                endpoint.clone(),
                network_name.clone(),
                address.clone(),
            ]
        } else {
            eprintln!(
                "usage: observe_mempool_to_confirmed <endpoint> <network> <t-address>\n\
                 example: observe_mempool_to_confirmed http://127.0.0.1:9101 zcash-regtest tm..."
            );
            return ExitCode::from(2);
        };

    let network = match network_name.as_str() {
        "zcash-mainnet" => Network::ZcashMainnet,
        "zcash-testnet" => Network::ZcashTestnet,
        "zcash-regtest" => Network::ZcashRegtest,
        other => {
            eprintln!(
                "unknown network {other:?}; expected one of zcash-mainnet, zcash-testnet, zcash-regtest"
            );
            return ExitCode::from(2);
        }
    };

    if let Err(error) = run(endpoint, network, address).await {
        eprintln!("observe_mempool_to_confirmed: fatal: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(endpoint: String, network: Network, address: String) -> Result<(), IndexerError> {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match observe_once(&endpoint, network, &address).await {
            Ok(()) => return Ok(()),
            Err(error) => match error.retry_policy() {
                RetryPolicy::RetryWithBackoff => {
                    eprintln!(
                        "retryable failure ({error}); backing off for {} ms",
                        backoff.as_millis()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                RetryPolicy::OperatorActionRequired | RetryPolicy::ClientError => {
                    return Err(error);
                }
                _ => return Err(error),
            },
        }
    }
}

async fn observe_once(endpoint: &str, network: Network, address: &str) -> Result<(), IndexerError> {
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint: endpoint.to_owned(),
        network,
    })?;

    let script_hash = transparent_address_script_hash(address)?;
    let watched = snapshot_mempool(&chain_index, script_hash, address).await?;
    subscribe_mempool_events(chain_index, watched, script_hash).await
}

async fn snapshot_mempool(
    chain_index: &RemoteChainIndex,
    script_hash: TransparentAddressScriptHash,
    address: &str,
) -> Result<HashSet<TransactionId>, IndexerError> {
    let outputs = chain_index
        .transparent_mempool_outputs_by_address(TransparentMempoolOutputsRequest {
            address_script_hash: script_hash,
            max_entries: MEMPOOL_SNAPSHOT_LIMIT,
        })
        .await?;
    let mut watched = HashSet::new();
    for output in &outputs {
        watched.insert(output.outpoint.transaction_id);
        println!(
            "mempool snapshot: address={} outpoint={:?} value_zat={}",
            address, output.outpoint, output.value_zat
        );
    }
    println!("snapshot complete: tracked={}", watched.len());
    Ok(watched)
}

async fn subscribe_mempool_events(
    chain_index: RemoteChainIndex,
    initial_watched: HashSet<TransactionId>,
    script_hash: TransparentAddressScriptHash,
) -> Result<(), IndexerError> {
    let mut events = chain_index
        .mempool_events(EventStreamStart::EarliestRetained)
        .await?;
    let mut watched = initial_watched;
    println!(
        "subscribed to mempool events; tracking {} transactions",
        watched.len()
    );
    while let Some(envelope) = events.next().await {
        let envelope = envelope?;
        match envelope.event {
            MempoolEvent::Added { entry } => {
                if entry_touches_address(&chain_index, &entry.transaction_id, script_hash).await?
                    && watched.insert(entry.transaction_id)
                {
                    println!(
                        "mempool added: tx={:?} first_seen_unix_millis={}",
                        entry.transaction_id,
                        entry.first_seen_unix_millis.value()
                    );
                }
            }
            MempoolEvent::Mined {
                transaction_id,
                mined_height,
                block_hash,
            } => {
                if watched.remove(&transaction_id) {
                    println!(
                        "mempool mined: tx={:?} height={} block_hash={:?}",
                        transaction_id,
                        mined_height.value(),
                        block_hash
                    );
                }
            }
            MempoolEvent::Invalidated {
                transaction_id,
                reason,
            } => {
                if watched.remove(&transaction_id) {
                    println!("mempool invalidated: tx={transaction_id:?} reason={reason:?}");
                }
            }
            MempoolEvent::Suppressed { transaction_id } => {
                if watched.remove(&transaction_id) {
                    println!("mempool suppressed: tx={transaction_id:?}");
                }
            }
            _ => {
                println!(
                    "mempool event: sequence={} (unhandled variant)",
                    envelope.event_sequence
                );
            }
        }
    }
    Ok(())
}

async fn entry_touches_address(
    chain_index: &RemoteChainIndex,
    transaction_id: &TransactionId,
    script_hash: TransparentAddressScriptHash,
) -> Result<bool, IndexerError> {
    let outputs = chain_index
        .transparent_mempool_outputs_by_address(TransparentMempoolOutputsRequest {
            address_script_hash: script_hash,
            max_entries: MEMPOOL_SNAPSHOT_LIMIT,
        })
        .await?;
    Ok(outputs
        .iter()
        .any(|output| &output.outpoint.transaction_id == transaction_id))
}

fn transparent_address_script_hash(
    address: &str,
) -> Result<TransparentAddressScriptHash, IndexerError> {
    use zebra_chain::transparent::Address as ZebraTransparentAddress;
    let zebra_address =
        address
            .parse::<ZebraTransparentAddress>()
            .map_err(|_| IndexerError::InvalidRequest {
                reason: format!("transparent address {address:?} could not be parsed"),
            })?;
    let script_pub_key = zebra_address.script().as_raw_bytes().to_vec();
    if script_pub_key.is_empty() {
        return Err(IndexerError::InvalidRequest {
            reason: format!("transparent address {address:?} has no receivable script"),
        });
    }
    let mut hasher = Sha256::new();
    hasher.update(&script_pub_key);
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(TransparentAddressScriptHash::from_bytes(digest))
}
