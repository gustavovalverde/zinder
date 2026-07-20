//! Lightwalletd query implementation over admitted canonical and wallet stores.

use std::{
    collections::HashSet,
    num::{NonZeroU16, NonZeroU32},
    sync::Arc,
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, BlockSelector,
    CanonicalBlockFactsSequenceDigest, CanonicalBlockFactsSequenceDigestVersion, ChainEpoch,
    ChainEpochId, MinedTransaction, MinedTransactionChainContext, NetworkUpgradeActivations,
    RawTransactionBytes, ShieldedProtocol, TransactionId, TransparentAddressBalance,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TransparentUnspentOutput,
    TxStatus,
};
use zinder_source::{TransactionBroadcaster, TreeStateUpstream};
use zinder_store::{ArtifactFamily, StreamCursorTokenV1};
use zinder_wallet_projection::{
    WalletAddressTransactionKey, WalletAddressUnspentOutputKey, WalletCanonicalSourceIdentity,
    WalletProjectionSourcePosition,
};

use crate::{
    ArtifactKey, BlockIdAtEpoch, CompactBlock, CompactBlockRange, LightwalletdQueryApi, QueryError,
    RawTransaction, SubtreeRoots, TransactionStatus, TransparentAddressTxIds,
    TransparentAddressTxIdsInRangeRequest, TransparentAddressUnspentOutputs,
    TransparentAddressUnspentOutputsRequest, TreeState, VisibleTipBlock, WalletServingReadPair,
};

const WALLET_READ_PAGE_SIZE: NonZeroU16 = NonZeroU16::MAX;
const LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_MAGIC: [u8; 4] = *b"lwh1";
const LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_LEN: usize = 174;

/// Exact continuation state for one lightwalletd transparent-history page.
///
/// The cursor carries the immutable wallet source that issued it. A publisher
/// can replace the serving pair between client page requests, so resuming with
/// only the ordered row key could otherwise combine histories from two forks.
#[derive(Clone, Copy)]
struct LightwalletdTransparentHistoryCursor {
    source: WalletCanonicalSourceIdentity,
    after: WalletAddressTransactionKey,
}

impl LightwalletdTransparentHistoryCursor {
    fn issue(
        source: WalletCanonicalSourceIdentity,
        after: WalletAddressTransactionKey,
    ) -> StreamCursorTokenV1 {
        let source_position = source.source_position();
        let sequence_digest = source.source_sequence_digest();
        let settled_tip = source.settled_tip();
        let mut encoded = Vec::with_capacity(LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_LEN);
        encoded.extend_from_slice(&LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_MAGIC);
        encoded.extend_from_slice(&source_position.chain_epoch_id.value().to_be_bytes());
        append_block_id(&mut encoded, source_position.tip);
        encoded.extend_from_slice(&source_position.event_sequence.to_be_bytes());
        encoded.extend_from_slice(&sequence_digest.version().value().to_be_bytes());
        encoded.extend_from_slice(&sequence_digest.block_count().to_be_bytes());
        encoded.extend_from_slice(&sequence_digest.as_bytes());
        append_block_id(&mut encoded, settled_tip);
        encoded.extend_from_slice(after.as_bytes());
        StreamCursorTokenV1::from_bytes(encoded)
    }

    fn resume(
        cursor: &StreamCursorTokenV1,
        expected_source: WalletCanonicalSourceIdentity,
    ) -> Result<WalletAddressTransactionKey, QueryError> {
        let decoded = Self::decode(cursor.as_bytes())?;
        if decoded.source != expected_source {
            return Err(QueryError::ChainEpochPinUnavailable {
                chain_epoch_id: decoded.source.source_position().chain_epoch_id,
            });
        }
        Ok(decoded.after)
    }

    fn decode(encoded: &[u8]) -> Result<Self, QueryError> {
        if encoded.len() != LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_LEN
            || !encoded.starts_with(&LIGHTWALLETD_TRANSPARENT_HISTORY_CURSOR_MAGIC)
        {
            return Err(invalid_transparent_history_cursor());
        }

        let chain_epoch_id = ChainEpochId::new(u64::from_be_bytes(cursor_bytes(encoded, 4..12)?));
        let tip = decode_block_id(encoded, 12)?;
        let event_sequence = u64::from_be_bytes(cursor_bytes(encoded, 48..56)?);
        let digest_version = u16::from_be_bytes(cursor_bytes(encoded, 56..58)?);
        let digest_version = CanonicalBlockFactsSequenceDigestVersion::try_from(digest_version)
            .map_err(|_| invalid_transparent_history_cursor())?;
        let digest_block_count = u64::from_be_bytes(cursor_bytes(encoded, 58..66)?);
        let digest = CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
            digest_version,
            digest_block_count,
            cursor_bytes(encoded, 66..98)?,
        );
        let settled_tip = decode_block_id(encoded, 98)?;
        if chain_epoch_id.value() == 0
            || event_sequence == 0
            || digest.block_count() == 0
            || !settled_tip_is_within_source(tip, settled_tip)
        {
            return Err(invalid_transparent_history_cursor());
        }
        let after = WalletAddressTransactionKey::decode(&encoded[134..])
            .map_err(|_| invalid_transparent_history_cursor())?;
        Ok(Self {
            source: WalletCanonicalSourceIdentity::new(
                WalletProjectionSourcePosition::new(chain_epoch_id, tip, event_sequence),
                digest,
                settled_tip,
            ),
            after,
        })
    }
}

fn append_block_id(encoded: &mut Vec<u8>, block_id: BlockId) {
    encoded.extend_from_slice(&block_id.height.value().to_be_bytes());
    encoded.extend_from_slice(&block_id.hash.as_bytes());
}

fn decode_block_id(encoded: &[u8], start: usize) -> Result<BlockId, QueryError> {
    let height = BlockHeight::new(u32::from_be_bytes(cursor_bytes(encoded, start..start + 4)?));
    let hash = BlockHash::from_bytes(cursor_bytes(encoded, start + 4..start + 36)?);
    Ok(BlockId::new(height, hash))
}

fn settled_tip_is_within_source(source_tip: BlockId, settled_tip: BlockId) -> bool {
    settled_tip.height < source_tip.height
        || (settled_tip.height == source_tip.height && settled_tip.hash == source_tip.hash)
}

fn cursor_bytes<const N: usize>(
    encoded: &[u8],
    range: std::ops::Range<usize>,
) -> Result<[u8; N], QueryError> {
    encoded
        .get(range)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(invalid_transparent_history_cursor)
}

fn invalid_transparent_history_cursor() -> QueryError {
    QueryError::TransparentHistoryCursorInvalid {
        reason: "cursor is not a lightwalletd transparent-history continuation",
    }
}

fn validate_transparent_history_cursor_key(
    key: WalletAddressTransactionKey,
    request: &TransparentAddressTxIdsInRangeRequest,
) -> Result<(), QueryError> {
    if key.address_script_hash() != request.address_script_hash {
        return Err(QueryError::TransparentHistoryCursorInvalid {
            reason: "cursor address does not match request address",
        });
    }
    let height = key.block_height();
    if height < request.start_height || height > request.end_height {
        return Err(QueryError::TransparentHistoryCursorInvalid {
            reason: "cursor height is outside request range",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use zinder_core::{
        BlockHash, BlockHeight, BlockId, CanonicalBlockFactsSequenceDigest,
        CanonicalBlockFactsSequenceDigestVersion, ChainEpochId, TransparentAddressScriptHash,
    };
    use zinder_wallet_projection::{
        WalletAddressTransactionKey, WalletCanonicalSourceIdentity, WalletProjectionSourcePosition,
    };

    use super::{
        LightwalletdTransparentHistoryCursor, QueryError, TransparentAddressTxIdsInRangeRequest,
        validate_transparent_history_cursor_key,
    };

    #[test]
    fn transparent_history_continuation_rejects_pair_replacement_and_preserves_current_paging()
    -> Result<(), Box<dyn Error>> {
        let first_page_source = source_identity(1, 0x11);
        let replacement_pair_source = source_identity(1, 0x22);
        let address = TransparentAddressScriptHash::from_bytes([0xa5; 32]);
        let first_page_after = WalletAddressTransactionKey::new(address, BlockHeight::new(12), 0);
        let second_page_after = WalletAddressTransactionKey::new(address, BlockHeight::new(13), 0);
        let first_page_cursor =
            LightwalletdTransparentHistoryCursor::issue(first_page_source, first_page_after);

        // The pair remains current while the client reads its second page.
        assert_eq!(
            LightwalletdTransparentHistoryCursor::resume(&first_page_cursor, first_page_source)?,
            first_page_after
        );
        let second_page_cursor =
            LightwalletdTransparentHistoryCursor::issue(first_page_source, second_page_after);
        assert_eq!(
            LightwalletdTransparentHistoryCursor::resume(&second_page_cursor, first_page_source)?,
            second_page_after
        );

        // A pair replacement between pages must never resume its raw key on
        // the replacement fork.
        let Err(error) = LightwalletdTransparentHistoryCursor::resume(
            &first_page_cursor,
            replacement_pair_source,
        ) else {
            return Err("replacement pair unexpectedly accepted prior-page cursor".into());
        };
        assert!(matches!(
            error,
            QueryError::ChainEpochPinUnavailable {
                chain_epoch_id
            } if chain_epoch_id == ChainEpochId::new(1)
        ));

        Ok(())
    }

    #[test]
    fn malformed_transparent_history_continuation_remains_invalid_input()
    -> Result<(), Box<dyn Error>> {
        let source = source_identity(1, 0x11);
        let malformed = zinder_store::StreamCursorTokenV1::from_bytes(vec![0; 40]);
        let Err(error) = LightwalletdTransparentHistoryCursor::resume(&malformed, source) else {
            return Err("malformed transparent-history cursor unexpectedly resumed".into());
        };
        assert!(matches!(
            error,
            QueryError::TransparentHistoryCursorInvalid { .. }
        ));
        Ok(())
    }

    #[test]
    fn structurally_impossible_transparent_history_source_is_invalid_input()
    -> Result<(), Box<dyn Error>> {
        let source = source_identity(1, 0x11);
        let after = WalletAddressTransactionKey::new(
            TransparentAddressScriptHash::from_bytes([0xa5; 32]),
            BlockHeight::new(12),
            0,
        );
        let cursor = LightwalletdTransparentHistoryCursor::issue(source, after);
        let mut zero_event_sequence = cursor.as_bytes().to_vec();
        let Some(event_sequence) = zero_event_sequence.get_mut(48..56) else {
            return Err("transparent-history cursor did not contain an event sequence".into());
        };
        event_sequence.fill(0);
        let zero_event_sequence =
            zinder_store::StreamCursorTokenV1::from_bytes(zero_event_sequence);

        let Err(error) = LightwalletdTransparentHistoryCursor::resume(&zero_event_sequence, source)
        else {
            return Err("zero event sequence unexpectedly resumed".into());
        };
        assert!(matches!(
            error,
            QueryError::TransparentHistoryCursorInvalid { .. }
        ));

        let mut zero_digest_block_count = cursor.as_bytes().to_vec();
        let Some(digest_block_count) = zero_digest_block_count.get_mut(58..66) else {
            return Err("transparent-history cursor did not contain a digest block count".into());
        };
        digest_block_count.fill(0);
        let zero_digest_block_count =
            zinder_store::StreamCursorTokenV1::from_bytes(zero_digest_block_count);
        let Err(error) =
            LightwalletdTransparentHistoryCursor::resume(&zero_digest_block_count, source)
        else {
            return Err("zero digest block count unexpectedly resumed".into());
        };
        assert!(matches!(
            error,
            QueryError::TransparentHistoryCursorInvalid { .. }
        ));

        Ok(())
    }

    #[test]
    fn transparent_history_continuation_must_match_its_request_scope() -> Result<(), Box<dyn Error>>
    {
        let request_address = TransparentAddressScriptHash::from_bytes([0xa5; 32]);
        let request = TransparentAddressTxIdsInRangeRequest {
            address_script_hash: request_address,
            start_height: BlockHeight::new(10),
            end_height: BlockHeight::new(20),
            max_entries: std::num::NonZeroU32::MIN,
            from_cursor: None,
            descending: false,
        };
        let other_address = WalletAddressTransactionKey::new(
            TransparentAddressScriptHash::from_bytes([0xa6; 32]),
            BlockHeight::new(12),
            0,
        );
        let Err(error) = validate_transparent_history_cursor_key(other_address, &request) else {
            return Err("address-mismatched cursor unexpectedly validated".into());
        };
        assert!(matches!(
            error,
            QueryError::TransparentHistoryCursorInvalid {
                reason: "cursor address does not match request address"
            }
        ));

        let outside_range =
            WalletAddressTransactionKey::new(request_address, BlockHeight::new(21), 0);
        let Err(error) = validate_transparent_history_cursor_key(outside_range, &request) else {
            return Err("range-mismatched cursor unexpectedly validated".into());
        };
        assert!(matches!(
            error,
            QueryError::TransparentHistoryCursorInvalid {
                reason: "cursor height is outside request range"
            }
        ));

        Ok(())
    }

    fn source_identity(chain_epoch: u64, hash_byte: u8) -> WalletCanonicalSourceIdentity {
        let tip = BlockId::new(BlockHeight::new(13), BlockHash::from_bytes([hash_byte; 32]));
        WalletCanonicalSourceIdentity::new(
            WalletProjectionSourcePosition::new(ChainEpochId::new(chain_epoch), tip, 9),
            CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                CanonicalBlockFactsSequenceDigestVersion::V1,
                13,
                [hash_byte; 32],
            ),
            tip,
        )
    }
}

/// Exact-fence lightwalletd query over canonical and wallet-projection stores.
#[derive(Clone)]
pub struct LightwalletdServingQuery<Broadcaster> {
    serving_pair_slot: Arc<ArcSwap<WalletServingReadPair>>,
    broadcaster: Broadcaster,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    tree_state_upstream: Option<Arc<dyn TreeStateUpstream>>,
}

impl<Broadcaster> std::fmt::Debug for LightwalletdServingQuery<Broadcaster> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pair = self.capture_pair();
        formatter
            .debug_struct("LightwalletdServingQuery")
            .field("canonical_fence", &pair.canonical_fence())
            .field("wallet_fence", &pair.wallet_source())
            .field("tree_state_upstream", &self.tree_state_upstream.is_some())
            .finish_non_exhaustive()
    }
}

impl<Broadcaster> LightwalletdServingQuery<Broadcaster> {
    /// Builds a query over a swappable slot of already-admitted immutable pairs.
    ///
    /// Every [`LightwalletdQueryApi`] method captures one `Arc` from this slot before
    /// reading. A publisher can therefore atomically replace the pair without
    /// changing the canonical or wallet reader observed by an in-flight request.
    #[must_use]
    pub fn from_serving_pair_slot(
        serving_pair_slot: Arc<ArcSwap<WalletServingReadPair>>,
        broadcaster: Broadcaster,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        Self {
            serving_pair_slot,
            broadcaster,
            network_upgrade_activations,
            tree_state_upstream: None,
        }
    }

    /// Attaches the node-backed sparse tree-state fill path.
    #[must_use]
    pub fn with_tree_state_upstream(mut self, upstream: Arc<dyn TreeStateUpstream>) -> Self {
        self.tree_state_upstream = Some(upstream);
        self
    }

    fn capture_pair(&self) -> Arc<WalletServingReadPair> {
        self.serving_pair_slot.load_full()
    }

    fn chain_epoch(
        pair: &WalletServingReadPair,
        requested: Option<ChainEpochId>,
    ) -> Result<ChainEpoch, QueryError> {
        let chain_epoch = pair.canonical().chain_epoch()?;
        if requested.is_some_and(|requested| requested != chain_epoch.id) {
            return Err(QueryError::ChainEpochPinUnavailable {
                chain_epoch_id: requested.unwrap_or(chain_epoch.id),
            });
        }
        Ok(chain_epoch)
    }

    fn block_id_at(
        pair: &WalletServingReadPair,
        height: BlockHeight,
    ) -> Result<BlockId, QueryError> {
        pair.canonical()
            .block_header_at(height)?
            .map(|header| BlockId::new(height, header.block_hash))
            .ok_or(QueryError::BlockNotInBestChain)
    }

    fn resolve_block_id_by_selector(
        pair: &WalletServingReadPair,
        selector: BlockSelector,
        chain_epoch: ChainEpoch,
    ) -> Result<BlockIdAtEpoch, QueryError> {
        let block_id = match selector {
            BlockSelector::Height(height) if height <= chain_epoch.visible_tip_height => {
                Self::block_id_at(pair, height)?
            }
            BlockSelector::Hash(hash) if hash == chain_epoch.visible_tip_hash => {
                BlockId::new(chain_epoch.visible_tip_height, hash)
            }
            BlockSelector::Height(_) | BlockSelector::Hash(_) => {
                return Err(QueryError::BlockNotInBestChain);
            }
            _ => {
                return Err(QueryError::UnsupportedBlockSelector {
                    reason: "selector is not supported by the canonical reader",
                });
            }
        };
        Ok(BlockIdAtEpoch {
            chain_epoch,
            block_id,
        })
    }
}

#[async_trait]
impl<Broadcaster> LightwalletdQueryApi for LightwalletdServingQuery<Broadcaster>
where
    Broadcaster: TransactionBroadcaster + Clone,
{
    async fn visible_tip_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<VisibleTipBlock, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        Ok(VisibleTipBlock {
            height: chain_epoch.visible_tip_height,
            block_hash: chain_epoch.visible_tip_hash,
            chain_epoch,
        })
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockIdAtEpoch, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        Self::resolve_block_id_by_selector(&pair, selector, chain_epoch)
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlock, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let compact_block = pair
            .canonical()
            .compact_block_at(height)?
            .ok_or_else(|| artifact_unavailable(ArtifactFamily::CompactBlock, height))?;
        Ok(CompactBlock {
            chain_epoch,
            compact_block,
        })
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: zinder_core::BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockRange, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        if block_range.start > block_range.end {
            return Err(QueryError::InvalidBlockRange {
                start_height: block_range.start,
                end_height: block_range.end,
            });
        }
        let compact_blocks = pair.canonical().compact_blocks_in_range(block_range)?;
        Ok(CompactBlockRange {
            chain_epoch,
            block_range,
            compact_blocks,
        })
    }

    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransactionStatus, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        // READY admission requires the construction manifest's transaction-location
        // and transaction-blob row counts to equal the authenticated source count;
        // live append and replacement update both families in the canonical atomic
        // batch. Under that admitted coverage contract, absence is a real miss.
        let Some(location) = pair.canonical().transaction_location(transaction_id)? else {
            return Ok(TransactionStatus {
                chain_epoch,
                status: TxStatus::NotFound,
            });
        };
        let header = pair
            .canonical()
            .block_header_at(location.block_height)?
            .ok_or(QueryError::BlockNotInBestChain)?;
        let raw_transaction_bytes = pair
            .canonical()
            .transaction_blob(location)?
            .map(|blob| blob.raw_transaction_bytes);
        let chain_context = MinedTransactionChainContext::from_response_epoch(
            &chain_epoch,
            location.block_height,
            self.network_upgrade_activations
                .consensus_branch_id_at(location.block_height),
            header.block_time,
        );
        Ok(TransactionStatus {
            chain_epoch,
            status: TxStatus::Mined(MinedTransaction::new(
                location,
                chain_context,
                raw_transaction_bytes,
            )),
        })
    }

    async fn raw_transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<RawTransaction, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let location = pair
            .canonical()
            .transaction_location(transaction_id)?
            .ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::TransactionLocation,
                key: ArtifactKey::TransactionId(transaction_id),
            })?;
        let transaction = pair
            .canonical()
            .transaction_blob(location)?
            .ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::TransactionBlob,
                key: ArtifactKey::TransactionId(transaction_id),
            })?;
        Ok(RawTransaction {
            chain_epoch,
            transaction,
        })
    }

    async fn transparent_address_unspent_outputs(
        &self,
        request: TransparentAddressUnspentOutputsRequest,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressUnspentOutputs, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let mut outputs = Vec::new();
        let mut after: Option<WalletAddressUnspentOutputKey> = None;
        loop {
            let page = pair.wallet().address_unspent_outputs_page_from_height(
                request.address_script_hash,
                request.start_height,
                after,
                WALLET_READ_PAGE_SIZE,
            )?;
            outputs.extend(page.outputs.into_iter().map(|output| {
                TransparentUnspentOutput::new(
                    output.address_script_hash,
                    output.script_pub_key,
                    output.outpoint,
                    output.value_zat,
                    output.created_at.block.height,
                    output.created_at.block.hash,
                )
            }));
            let Some(next) = page.next_page_after else {
                break;
            };
            after = Some(next);
        }
        Ok(TransparentAddressUnspentOutputs {
            chain_epoch,
            outputs,
        })
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
    ) -> Result<TransparentAddressTxIds, QueryError> {
        let pair = self.capture_pair();
        if request.descending {
            return Err(QueryError::MaterializedViewUnavailable {
                capability: "descending canonical transparent history",
            });
        }
        let chain_epoch = Self::chain_epoch(&pair, None)?;
        let after = request
            .from_cursor
            .as_ref()
            .map(|cursor| {
                LightwalletdTransparentHistoryCursor::resume(cursor, pair.wallet_source())
            })
            .transpose()?;
        if let Some(after) = after {
            validate_transparent_history_cursor_key(after, &request)?;
        }
        let page_size =
            NonZeroU16::new(u16::try_from(request.max_entries.get()).unwrap_or(u16::MAX))
                .unwrap_or(NonZeroU16::MAX);
        let page = pair.wallet().address_transaction_history_range_page(
            request.address_script_hash,
            BlockHeightRange::inclusive(request.start_height, request.end_height),
            after,
            page_size,
        )?;
        let artifacts = page
            .transactions
            .into_iter()
            .map(|row| {
                TransparentAddressTxIndexArtifact::new(
                    request.address_script_hash,
                    row.key.block_height(),
                    row.key.tx_index_in_block(),
                    row.transaction_id,
                    row.block_hash,
                )
            })
            .collect();
        Ok(TransparentAddressTxIds {
            chain_epoch,
            artifacts,
            next_cursor: page
                .next_page_after
                .map(|key| LightwalletdTransparentHistoryCursor::issue(pair.wallet_source(), key)),
        })
    }

    async fn transparent_address_balance(
        &self,
        addresses: Vec<TransparentAddressScriptHash>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressBalance, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        if addresses.is_empty() {
            return Err(QueryError::TransparentBalanceAddressCountExceeded {
                requested: 0,
                maximum: crate::MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES,
            });
        }
        let addresses: HashSet<_> = addresses.into_iter().collect();
        let mut confirmed_zat = 0_u64;
        for address in &addresses {
            confirmed_zat = confirmed_zat.saturating_add(pair.wallet().address_balance(*address)?);
        }
        Ok(TransparentAddressBalance {
            confirmed_zat,
            unconfirmed_delta_zat: 0,
            address_count: u32::try_from(addresses.len()).unwrap_or(u32::MAX),
            chain_epoch,
        })
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let checkpoint = pair
            .canonical()
            .tree_state_checkpoint_at_or_before(height)?;
        if let Some(checkpoint) =
            checkpoint.filter(|checkpoint| checkpoint.block_id.height == height)
        {
            return tree_state_from_checkpoint(chain_epoch, &checkpoint);
        }
        let block_id = Self::block_id_at(&pair, height)?;
        let upstream = self
            .tree_state_upstream
            .as_ref()
            .ok_or_else(|| artifact_unavailable(ArtifactFamily::TreeState, height))?;
        let source = upstream.fetch_tree_state_for_block(block_id).await?;
        Ok(TreeState {
            chain_epoch,
            height,
            block_hash: block_id.hash,
            payload_bytes: source.payload_bytes,
        })
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let checkpoint = pair
            .canonical()
            .tree_state_checkpoint_at_or_before(chain_epoch.visible_tip_height)?
            .ok_or_else(|| {
                artifact_unavailable(ArtifactFamily::TreeState, chain_epoch.visible_tip_height)
            })?;
        tree_state_from_checkpoint(chain_epoch, &checkpoint)
    }

    async fn subtree_roots(
        &self,
        subtree_root_range: zinder_core::SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<SubtreeRoots, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let completed_subtree_count = chain_epoch
            .tip_metadata
            .completed_subtree_count(subtree_root_range.protocol);
        if subtree_root_range.start_index.value() >= completed_subtree_count {
            return Ok(SubtreeRoots {
                chain_epoch,
                protocol: subtree_root_range.protocol,
                start_index: subtree_root_range.start_index,
                subtree_roots: Vec::new(),
            });
        }
        let available_entries = completed_subtree_count
            .saturating_sub(subtree_root_range.start_index.value())
            .min(subtree_root_range.max_entries.get());
        let available_entries =
            NonZeroU32::new(available_entries).ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::SubtreeRoot,
                key: ArtifactKey::SubtreeRootIndex {
                    protocol: subtree_root_range.protocol,
                    index: subtree_root_range.start_index,
                },
            })?;
        let available_range = zinder_core::SubtreeRootRange::new(
            subtree_root_range.protocol,
            subtree_root_range.start_index,
            available_entries,
        );
        let subtree_roots = pair.canonical().subtree_roots(available_range)?;
        Ok(SubtreeRoots {
            chain_epoch,
            protocol: subtree_root_range.protocol,
            start_index: subtree_root_range.start_index,
            subtree_roots,
        })
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<zinder_core::TransactionBroadcastOutcome, QueryError> {
        let _pair = self.capture_pair();
        if raw_transaction.len() > zinder_core::MAX_RAW_TRANSACTION_BYTES {
            return Err(QueryError::BroadcastTransactionTooLarge {
                actual: raw_transaction.len(),
                maximum: zinder_core::MAX_RAW_TRANSACTION_BYTES,
            });
        }
        self.broadcaster
            .broadcast_transaction(raw_transaction)
            .await
            .map_err(QueryError::Node)
    }
}

fn artifact_unavailable(family: ArtifactFamily, height: BlockHeight) -> QueryError {
    QueryError::ArtifactUnavailable {
        family,
        key: ArtifactKey::BlockHeight(height),
    }
}

fn tree_state_from_checkpoint(
    chain_epoch: ChainEpoch,
    checkpoint: &zinder_core::CommitmentTreeCheckpoint,
) -> Result<TreeState, QueryError> {
    let mut payload = Map::new();
    payload.insert("time".to_owned(), json!(checkpoint.block_time_seconds));
    for (name, protocol) in [
        ("sapling", ShieldedProtocol::Sapling),
        ("orchard", ShieldedProtocol::Orchard),
        ("ironwood", ShieldedProtocol::Ironwood),
    ] {
        if let Some(frontier) = checkpoint.frontiers.get(protocol) {
            payload.insert(
                name.to_owned(),
                json!({
                    "commitments": {
                        "finalState": hex::encode(frontier.final_state_bytes())
                    }
                }),
            );
        }
    }
    let payload_bytes = serde_json::to_vec(&Value::Object(payload)).map_err(|source| {
        QueryError::ArtifactCorrupt {
            family: ArtifactFamily::TreeState,
            reason: source.to_string(),
        }
    })?;
    Ok(TreeState {
        chain_epoch,
        height: checkpoint.block_id.height,
        block_hash: checkpoint.block_id.hash,
        payload_bytes,
    })
}
