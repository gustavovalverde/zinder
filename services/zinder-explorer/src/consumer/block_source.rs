//! Bounded TTL cache fronting `WalletQuery.FullBlock` for derive consumers.
//!
//! Every chain-events consumer that reads block content goes through one
//! shared [`BlockSource`]. The first consumer to ask for height `H` triggers
//! the `FullBlock` RPC, parses the bytes with `zebra-chain`, builds a
//! [`BlockCommitContext`], and caches the `Arc` under `H`. Subsequent
//! consumers asking for the same height during the cache window get the
//! same `Arc` without re-fetching or re-parsing.
//!
//! The cache is intentionally tiny (capacity 32, TTL 30 s): mainnet block
//! cadence is ~75 s, so the cache survives the intra-envelope fan-out
//! across N consumers without ever needing to hold more than a handful of
//! blocks at once. Cache eviction is "evict oldest insert when full,"
//! which mirrors the access pattern (newest-first; older entries are
//! unlikely to be re-requested once every consumer has advanced past
//! them).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tonic::Request;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto as _;
use zinder_core::BlockHeight;
use zinder_proto::v1::wallet::{self, FullBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::block_commit_context::{
    BlockCommitContext, BlockCommitContextError, BlockCommitPayload, PrevoutResolver,
};

/// Maximum number of cached parsed blocks.
///
/// Sized to comfortably hold the intra-envelope fan-out across the four
/// chain-events consumers Zinder ships today, plus headroom for the
/// occasional consumer that lags a couple of heights behind.
const CACHE_CAPACITY: usize = 32;

/// Maximum age before a cached parsed block is treated as stale.
///
/// Shorter than the mainnet block interval (~75 s) so the cache size stays
/// bounded by the fan-out window and not by absolute time.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Block fetcher shared across derive consumers.
///
/// Hold one `BlockSource` per process and clone it (via [`Clone`], cheap
/// because it wraps an `Arc`) into each consumer. The internal cache
/// dedupes parallel `block(H)` calls from N consumers into a single
/// upstream RPC and a single parse.
#[derive(Clone)]
pub struct BlockSource {
    inner: Arc<BlockSourceInner>,
}

struct BlockSourceInner {
    wallet_client: AsyncMutex<WalletQueryClient<AuthenticatedChannel>>,
    prevout_resolver: PrevoutResolver,
    cache: Mutex<VecDeque<CachedEntry>>,
}

/// Three-state cache outcome distinguishing miss from cached-absent.
enum CacheLookup {
    Miss,
    HitAbsent,
    HitPresent(Arc<BlockCommitContext>),
}

struct CachedEntry {
    height: BlockHeight,
    inserted_at: Instant,
    /// `None` for confirmed-absent (`NotFound` from upstream) so subsequent
    /// callers don't re-issue the RPC.
    context: Option<Arc<BlockCommitContext>>,
}

impl BlockSource {
    /// Builds a block source backed by `wallet_client` and the configured
    /// prevout-resolution stance.
    #[must_use]
    pub fn new(
        wallet_client: WalletQueryClient<AuthenticatedChannel>,
        prevout_resolver: PrevoutResolver,
    ) -> Self {
        Self {
            inner: Arc::new(BlockSourceInner {
                wallet_client: AsyncMutex::new(wallet_client),
                prevout_resolver,
                cache: Mutex::new(VecDeque::with_capacity(CACHE_CAPACITY)),
            }),
        }
    }

    /// Returns the parsed block context for `height`, hitting the cache
    /// when available.
    ///
    /// `Ok(None)` means the wallet plane has no `FullBlock` artifact for
    /// `height` (typical at the checkpoint height during bootstrap).
    /// `Ok(Some(_))` returns the shared `Arc` other consumers see.
    pub async fn block(
        &self,
        height: BlockHeight,
    ) -> Result<Option<Arc<BlockCommitContext>>, BlockCommitContextError> {
        match self.cache_lookup(height) {
            CacheLookup::HitPresent(context) => Ok(Some(context)),
            CacheLookup::HitAbsent => Ok(None),
            CacheLookup::Miss => self.fetch_and_cache(height).await,
        }
    }

    /// Outcome of a cache lookup. `Miss` triggers an upstream fetch;
    /// `HitPresent` returns a previously-cached parsed block; `HitAbsent`
    /// returns the cached "wallet has no `FullBlock` for this height"
    /// result so we don't re-issue the RPC.
    fn cache_lookup(&self, height: BlockHeight) -> CacheLookup {
        let now = Instant::now();
        let mut cache = self.inner.cache.lock();
        cache.retain(|entry| now.duration_since(entry.inserted_at) <= CACHE_TTL);
        cache
            .iter()
            .find(|entry| entry.height == height)
            .map_or(CacheLookup::Miss, |entry| {
                entry
                    .context
                    .clone()
                    .map_or(CacheLookup::HitAbsent, CacheLookup::HitPresent)
            })
    }

    async fn fetch_and_cache(
        &self,
        height: BlockHeight,
    ) -> Result<Option<Arc<BlockCommitContext>>, BlockCommitContextError> {
        let fetched = self.fetch_block(height).await?;
        let context = fetched.map(Arc::new);
        {
            let mut cache = self.inner.cache.lock();
            if cache.len() == CACHE_CAPACITY {
                cache.pop_front();
            }
            cache.push_back(CachedEntry {
                height,
                inserted_at: Instant::now(),
                context: context.clone(),
            });
        }
        Ok(context)
    }

    async fn fetch_block(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockCommitContext>, BlockCommitContextError> {
        let mut client = self.inner.wallet_client.lock().await;
        let response = client
            .full_block(Request::new(FullBlockRequest {
                block_height: height.value(),
                at_epoch: None,
            }))
            .await;
        drop(client);
        let inner = match response {
            Ok(envelope) => envelope.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(status) => {
                return Err(BlockCommitContextError::WalletFullBlock {
                    height: height.value(),
                    status: status.message().to_owned(),
                });
            }
        };
        let Some(wallet::FullBlock {
            block_hash,
            parent_block_hash,
            raw_block_bytes,
            ..
        }) = inner.block
        else {
            return Ok(None);
        };
        let parsed: ZebraBlock = raw_block_bytes
            .as_slice()
            .zcash_deserialize_into()
            .map_err(|error| BlockCommitContextError::BlockParseFailed {
                height: height.value(),
                reason: error.to_string(),
            })?;
        let payload = BlockCommitPayload {
            height,
            block_hash,
            previous_block_hash: parent_block_hash,
            raw_block_bytes,
            block: parsed,
        };
        Ok(Some(BlockCommitContext::new(
            payload,
            self.inner.prevout_resolver.clone(),
        )))
    }
}
