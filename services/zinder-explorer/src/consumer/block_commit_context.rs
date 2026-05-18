//! Per-block parsed view shared across derive consumers for one commit.
//!
//! [`BlockCommitContext`] hydrates a [`WalletQuery.FullBlock`] response into
//! a parsed `zebra-chain` block paired with a lazily-resolved prevout map.
//! The same value is shared across every chain-events consumer driven from
//! the same [`crate::consumer::BlockSource`], so for a four-consumer fan-out
//! the block parses once and prevouts resolve once even though four
//! independent `apply_block` calls observe the same height.
//!
//! Hosting the parsed block (rather than the raw bytes) inside the cache
//! matters: re-parsing a 2 MB mainnet block four times per commit would
//! dominate the per-block CPU budget; resolving prevouts twice would double
//! the prevout RPC traffic.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::OnceCell;
use tonic::Request;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::transparent;
use zinder_core::wire::encode_internal_transaction_id;
use zinder_core::{BlockHeight, TransactionId, TransparentOutPoint};
use zinder_proto::v1::wallet::{
    self, TransparentPrevoutsRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

/// Maximum prevouts fanned out in one [`TransparentPrevoutsRequest`].
///
/// Mirrors the cap the wallet-side handler enforces; batching above this
/// returns `INVALID_ARGUMENT`.
const MAX_PREVOUTS_PER_BATCH: usize = 256;

/// Errors surfaced while hydrating a [`BlockCommitContext`] or resolving
/// its prevouts.
///
/// Every variant carries the upstream-status text rather than a typed
/// `tonic::Status` so consumers can re-emit the error without dragging a
/// transport dependency into their own error enums.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockCommitContextError {
    /// `WalletQuery.FullBlock` returned a non-OK status other than `NotFound`.
    #[error("WalletQuery.FullBlock failed for height {height}: {status}")]
    WalletFullBlock {
        /// Height the consumer was trying to materialize.
        height: u32,
        /// Stringified upstream status.
        status: String,
    },
    /// `WalletQuery.FullBlock` returned a response whose `block` field was unset.
    #[error("WalletQuery.FullBlock response for height {height} carried no block payload")]
    WalletFullBlockMissingBlock {
        /// Height the consumer was trying to materialize.
        height: u32,
    },
    /// `raw_block_bytes` did not parse as a Zcash block.
    #[error("FullBlock raw_block_bytes for height {height} did not parse: {reason}")]
    BlockParseFailed {
        /// Height whose payload failed to decode.
        height: u32,
        /// Reason returned by `zebra-chain`'s deserializer.
        reason: String,
    },
    /// `WalletQuery.TransparentPrevouts` returned a non-OK status.
    #[error("WalletQuery.TransparentPrevouts failed: {0}")]
    WalletPrevouts(String),
    /// A `TransparentPrevoutsResponse` entry carried a malformed transaction id.
    #[error("TransparentPrevouts response carried a transaction id that was not 32 bytes")]
    PrevoutTransactionIdMalformed,
    /// A `TransparentPrevoutsResponse` entry carried a value outside the
    /// Zcash amount range.
    #[error("TransparentPrevouts response carried a malformed value: {0}")]
    PrevoutValueMalformed(String),
}

/// How [`BlockCommitContext::prevouts`] resolves missing values.
///
/// `Online` carries the live wallet client; `Offline` short-circuits to
/// `None` so consumers that branch on prevout availability never block on a
/// resolution attempt the binary has explicitly disabled. The wallet
/// client is boxed because the generated `WalletQueryClient` carries an
/// internal codec table that pushes the inline size past 200 bytes.
#[derive(Clone)]
pub enum PrevoutResolver {
    /// Resolve through `WalletQuery.TransparentPrevouts`. The boxed
    /// client is `Clone`-cheap (it wraps a `tonic::Channel`).
    Online(Box<WalletQueryClient<AuthenticatedChannel>>),
    /// Prevout resolution is not available; `prevouts()` returns `None`.
    Offline,
}

impl PrevoutResolver {
    /// Wraps a wallet client into the `Online` variant.
    #[must_use]
    pub fn online(client: WalletQueryClient<AuthenticatedChannel>) -> Self {
        Self::Online(Box::new(client))
    }
}

impl std::fmt::Debug for PrevoutResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online(_) => formatter.write_str("PrevoutResolver::Online"),
            Self::Offline => formatter.write_str("PrevoutResolver::Offline"),
        }
    }
}

/// Per-block parsed view threaded through one batch of consumer `apply_block`
/// calls.
///
/// Held inside `Arc<BlockCommitContext>` and shared across consumers
/// observing the same height through [`crate::consumer::BlockSource`].
/// Hydration of `prevouts()` is `OnceCell`-protected, so the resolution
/// happens at most once per shared context even when several consumers
/// request the map concurrently.
pub struct BlockCommitContext {
    /// Height the context describes.
    pub height: BlockHeight,
    /// Block hash bytes (32 bytes, internal byte order).
    pub block_hash: Vec<u8>,
    /// Previous-block hash bytes (32 bytes, internal byte order).
    pub previous_block_hash: Vec<u8>,
    /// Raw block bytes as the wallet plane delivered them. Length is the
    /// authoritative on-disk block size.
    pub raw_block_bytes: Vec<u8>,
    /// Block parsed once with `zebra-chain`.
    pub block: ZebraBlock,
    prevouts: OnceCell<Option<Arc<HashMap<TransparentOutPoint, transparent::Output>>>>,
    resolver: Mutex<PrevoutResolver>,
}

/// Parsed-block payload [`BlockCommitContext::new`] takes by value.
pub(crate) struct BlockCommitPayload {
    pub(crate) height: BlockHeight,
    pub(crate) block_hash: Vec<u8>,
    pub(crate) previous_block_hash: Vec<u8>,
    pub(crate) raw_block_bytes: Vec<u8>,
    pub(crate) block: ZebraBlock,
}

impl BlockCommitContext {
    /// Builds a context from an already-parsed block plus its raw bytes.
    pub(crate) fn new(payload: BlockCommitPayload, resolver: PrevoutResolver) -> Self {
        Self {
            height: payload.height,
            block_hash: payload.block_hash,
            previous_block_hash: payload.previous_block_hash,
            raw_block_bytes: payload.raw_block_bytes,
            block: payload.block,
            prevouts: OnceCell::new(),
            resolver: Mutex::new(resolver),
        }
    }

    /// Returns the prevout map for the block's non-coinbase inputs.
    ///
    /// `Ok(None)` means the binary configured an [`PrevoutResolver::Offline`]
    /// resolver, so the consumer should treat every prevout as unresolved.
    /// `Ok(Some(map))` resolves every transparent input the block contains;
    /// the map may be missing individual outpoints when the upstream cannot
    /// produce them.
    pub async fn prevouts(
        &self,
    ) -> Result<
        Option<Arc<HashMap<TransparentOutPoint, transparent::Output>>>,
        BlockCommitContextError,
    > {
        let cached = self
            .prevouts
            .get_or_try_init(|| async {
                let resolver = self.resolver.lock().clone();
                match resolver {
                    PrevoutResolver::Offline => Ok(None),
                    PrevoutResolver::Online(mut client) => {
                        let map = resolve_block_prevouts(client.as_mut(), &self.block).await?;
                        Ok(Some(Arc::new(map)))
                    }
                }
            })
            .await?;
        Ok(cached.clone())
    }
}

async fn resolve_block_prevouts(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    block: &ZebraBlock,
) -> Result<HashMap<TransparentOutPoint, transparent::Output>, BlockCommitContextError> {
    let mut outpoints: HashSet<TransparentOutPoint> = HashSet::new();
    for (position, transaction) in block.transactions.iter().enumerate() {
        if position == 0 {
            continue;
        }
        for input in transaction.inputs() {
            if let transparent::Input::PrevOut { outpoint, .. } = input {
                outpoints.insert(TransparentOutPoint::new(
                    TransactionId::from_bytes(outpoint.hash.0),
                    outpoint.index,
                ));
            }
        }
    }
    if outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    let mut resolved: HashMap<TransparentOutPoint, transparent::Output> = HashMap::new();
    let mut buffer: Vec<TransparentOutPoint> = Vec::with_capacity(MAX_PREVOUTS_PER_BATCH);
    for outpoint in outpoints {
        buffer.push(outpoint);
        if buffer.len() == MAX_PREVOUTS_PER_BATCH {
            let batch = std::mem::take(&mut buffer);
            request_prevouts_batch(wallet_client, batch, &mut resolved).await?;
        }
    }
    if !buffer.is_empty() {
        request_prevouts_batch(wallet_client, buffer, &mut resolved).await?;
    }
    Ok(resolved)
}

async fn request_prevouts_batch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    outpoints: Vec<TransparentOutPoint>,
    resolved: &mut HashMap<TransparentOutPoint, transparent::Output>,
) -> Result<(), BlockCommitContextError> {
    let request = TransparentPrevoutsRequest {
        outpoints: outpoints
            .iter()
            .map(|outpoint| wallet::OutPoint {
                transaction_id: encode_internal_transaction_id(outpoint.transaction_id).to_vec(),
                output_index: outpoint.output_index,
            })
            .collect(),
        at_epoch: None,
    };
    let response = wallet_client
        .transparent_prevouts(Request::new(request))
        .await
        .map_err(|status| BlockCommitContextError::WalletPrevouts(status.message().to_owned()))?
        .into_inner();
    for entry in response.entries {
        let Some(outpoint_wire) = entry.outpoint else {
            continue;
        };
        let Some(prevout) = entry.prevout else {
            continue;
        };
        let transaction_id_bytes: [u8; 32] = outpoint_wire
            .transaction_id
            .as_slice()
            .try_into()
            .map_err(|_| BlockCommitContextError::PrevoutTransactionIdMalformed)?;
        let outpoint = TransparentOutPoint::new(
            TransactionId::from_bytes(transaction_id_bytes),
            outpoint_wire.output_index,
        );
        let amount = i64::try_from(prevout.value_zat).unwrap_or(i64::MAX);
        let prevout_amount = zebra_chain::amount::Amount::try_from(amount)
            .map_err(|error| BlockCommitContextError::PrevoutValueMalformed(error.to_string()))?;
        let output = transparent::Output {
            value: prevout_amount,
            lock_script: transparent::Script::new(&prevout.script_pub_key),
        };
        resolved.insert(outpoint, output);
    }
    Ok(())
}
