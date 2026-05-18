//! Bounded TTL cache fronting `WalletQuery.FullBlock` for derive consumers.
//!
//! Every chain-events consumer that reads block content goes through one
//! shared [`BlockSource`]. The first consumer to ask for height `H` triggers
//! the `FullBlock` RPC, parses the bytes with `zebra-chain`, builds a
//! [`BlockCommitContext`], and caches the `Arc` under `H`. Subsequent
//! consumers asking for the same height during the cache window get the
//! same `Arc` without re-fetching or re-parsing.
//!
//! Concurrent misses on the same height are deduplicated by a per-height
//! inflight map: only the first task issues the RPC, the others await its
//! result and share the same `Arc<BlockCommitContext>`. Without this,
//! four consumer tasks racing for a fresh height each issued their own
//! RPC, defeated the cache, and (because they each held an independent
//! `Arc`) re-ran prevout resolution N times.
//!
//! The cache is intentionally tiny (capacity 32, TTL 30 s): mainnet block
//! cadence is ~75 s, so the cache survives the intra-envelope fan-out
//! across N consumers without ever needing to hold more than a handful of
//! blocks at once. Cache eviction is "evict oldest insert when full,"
//! which mirrors the access pattern (newest-first; older entries are
//! unlikely to be re-requested once every consumer has advanced past
//! them).

use std::collections::{HashMap, VecDeque, hash_map::Entry};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::OnceCell;
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
/// because it wraps an `Arc`) into each consumer. Each `fetch_block` call
/// constructs a fresh [`WalletQueryClient`] over the shared
/// [`AuthenticatedChannel`]; HTTP/2 multiplexes the calls, so multiple
/// consumers issue `FullBlock` RPCs concurrently without per-process
/// serialization. The cache then deduplicates results so repeat lookups
/// for the same height during the fan-out window skip the RPC entirely.
#[derive(Clone)]
pub struct BlockSource {
    inner: Arc<BlockSourceInner>,
}

struct BlockSourceInner {
    wallet_channel: AuthenticatedChannel,
    prevout_resolver: PrevoutResolver,
    cache: Mutex<VecDeque<CachedEntry>>,
    inflight: Mutex<HashMap<BlockHeight, InflightCell>>,
}

/// Shared one-shot cell every concurrent miss for a given height attaches to.
///
/// The first task drives `fetch_block` through
/// [`OnceCell::get_or_try_init`]; the others await the same future. On
/// success, the cell holds the shared `Arc`; on failure, the cell stays
/// empty so a later caller can retry (current waiters receive a cloned
/// error). Either way, the inflight entry is dropped from the map once
/// init resolves; the `Arc` keeps the cell alive for any waiters still
/// holding their own clone.
type InflightCell = Arc<OnceCell<Option<Arc<BlockCommitContext>>>>;

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
    /// Builds a block source backed by `wallet_channel` and the configured
    /// prevout-resolution stance.
    #[must_use]
    pub fn new(wallet_channel: AuthenticatedChannel, prevout_resolver: PrevoutResolver) -> Self {
        Self {
            inner: Arc::new(BlockSourceInner {
                wallet_channel,
                prevout_resolver,
                cache: Mutex::new(VecDeque::with_capacity(CACHE_CAPACITY)),
                inflight: Mutex::new(HashMap::new()),
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
            CacheLookup::HitPresent(context) => return Ok(Some(context)),
            CacheLookup::HitAbsent => return Ok(None),
            CacheLookup::Miss => {}
        }
        let cell = self.acquire_inflight_cell(height);
        let outcome = cell
            .get_or_try_init(|| async {
                let fetched = self.fetch_and_cache(height).await;
                self.inner.inflight.lock().remove(&height);
                fetched
            })
            .await?;
        Ok(outcome.clone())
    }

    /// Returns a per-height inflight cell, creating one if no fetch is in
    /// flight yet. Concurrent callers for the same height share the cell
    /// and therefore share the single fetch its init future drives.
    fn acquire_inflight_cell(&self, height: BlockHeight) -> InflightCell {
        let mut inflight = self.inner.inflight.lock();
        match inflight.entry(height) {
            Entry::Occupied(slot) => Arc::clone(slot.get()),
            Entry::Vacant(slot) => {
                let cell: InflightCell = Arc::new(OnceCell::new());
                slot.insert(Arc::clone(&cell));
                cell
            }
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
        let mut client = WalletQueryClient::new(self.inner.wallet_channel.clone());
        let response = client
            .full_block(Request::new(FullBlockRequest {
                block_height: height.value(),
                at_epoch: None,
            }))
            .await;
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
