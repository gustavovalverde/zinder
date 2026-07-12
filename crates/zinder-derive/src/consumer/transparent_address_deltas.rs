//! `TransparentAddressDeltas` derive consumer.
//!
//! Materializes one row per transparent-address value event keyed by
//! `(address_script_hash, ascending_height, in_block_position, kind, event_index)`.
//! The height segment is ascending, so a forward range scan serves a
//! single-address series oldest-first with no post-fetch reversal, matching
//! the zcashd `getaddressdeltas` order.
//!
//! ## Shared attribution
//!
//! The events come from
//! [`address_value_events`](crate::consumer::address_value_event::address_value_events),
//! the same per-event attribution
//! [`TransparentAddressActivityConsumer`](crate::consumer::transparent_address_activity)
//! folds into one net row per transaction. The delta surface persists the
//! events; the activity surface aggregates them, so net equals the sum of the
//! deltas over the same range.
//!
//! ## Resolution semantics
//!
//! Received-output events are always exact. Spend events carry
//! `spent_value_zat` from the canonical spend fact, so they need no prevout
//! re-resolution. A spend whose prevout is unresolved (or hydration is off)
//! produces no event rather than a wrong number; the per-page resolution
//! status is surfaced by the activity sibling for the same range.
//!
//! ## Rewind correctness
//!
//! The primary key starts with the address script hash, so a height-prefixed
//! range-delete cannot target one height across address ranges. The consumer
//! maintains a per-height index keyed by ascending height whose value lists
//! the `(address, in_block_position, kind, event_index)` tuples written at
//! that height; on revert it deletes each primary key it wrote, then the
//! index entry.

use std::collections::HashSet;

use prost::Message as _;
use zinder_core::wire::{
    decode_address_script_hash, decode_height_key_ascending, decode_in_block_position,
    decode_rpc_transaction_id_hex, encode_address_script_hash, encode_height_key_ascending,
    encode_in_block_position, encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHeight, TransparentAddressScriptHash};
use zinder_proto::v1::explorer::TransparentAddressDeltasRecord;
use zinder_proto::wire::{TRANSPARENT_DELTA_KIND_RECEIVED_BYTE, TRANSPARENT_DELTA_KIND_SPENT_BYTE};

use crate::consumer::address_value_event::{address_value_events, transaction_ids_by_position};
use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName, DeriveConsumerSchema,
};
use crate::{DeriveStore, DeriveStoreError};

/// Primary column family holding per-address delta rows.
pub const TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY: &str = "transparent_address_deltas";

/// Per-height index column family.
///
/// Key: 4-byte ascending block height. Value: concatenated
/// `(address_script_hash_32 | in_block_position_4 | kind_1 | event_index_4)`
/// for every row the consumer wrote at that height.
pub const TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY: &str = "transparent_address_deltas_index";

/// Column families the consumer needs registered before its first write.
pub const TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILIES: &[&str] = &[
    TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY,
];

/// Stable consumer name persisted in the SDK cursor table.
pub const TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("transparent_address_deltas");

/// On-disk schema declaration for the transparent-address-deltas consumer.
pub const TRANSPARENT_ADDRESS_DELTAS_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME,
    1,
    TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILIES,
);

const ADDRESS_HASH_LEN: usize = 32;
const HEIGHT_LEN: usize = 4;
const POSITION_LEN: usize = 4;
const KIND_LEN: usize = 1;
const EVENT_INDEX_LEN: usize = 4;

/// Length of one primary storage key:
/// 32 address + 4 ascending-height + 4 position + 1 kind + 4 event-index.
pub const TRANSPARENT_ADDRESS_DELTAS_KEY_LEN: usize =
    ADDRESS_HASH_LEN + HEIGHT_LEN + POSITION_LEN + KIND_LEN + EVENT_INDEX_LEN;

const INDEX_ENTRY_LEN: usize = ADDRESS_HASH_LEN + POSITION_LEN + KIND_LEN + EVENT_INDEX_LEN;

/// Aggregate of every retained transparent value event for one script hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressLifetimeSummary {
    /// Canonical script hash identifying the transparent address.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Checked sum of positive delta values.
    pub received_zat: u64,
    /// Checked sum of negative delta magnitudes.
    pub sent_zat: u64,
    /// Number of distinct canonical transaction ids carrying those deltas.
    pub distinct_transaction_count: u64,
    /// Earliest retained block time for the address.
    pub first_block_time_unix_seconds: i64,
    /// Latest retained block time for the address.
    pub last_block_time_unix_seconds: i64,
    /// Signed `received_zat - sent_zat`, retained for balance validation.
    pub net_balance_zat: i128,
    /// Non-negative net balance when it fits the public unsigned balance width.
    pub validated_balance_zat: Option<u64>,
}

/// Audit of the per-height delta source available to a lifetime bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentAddressDeltasSourceCoverage {
    /// First indexed height at or below the requested height.
    pub first_height: Option<BlockHeight>,
    /// Last indexed height at or below the requested height.
    pub last_height: Option<BlockHeight>,
    /// Number of indexed block rows at or below the requested height.
    pub row_count: u64,
    /// Whether exactly one index row exists for every height from 1 through
    /// the requested height.
    pub contiguous_from_height_1: bool,
}

/// Lifetime summaries and the source coverage that qualifies them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressDeltasLifetimeBootstrap {
    /// Summaries ordered by canonical script-hash bytes.
    pub summaries: Vec<TransparentAddressLifetimeSummary>,
    /// Coverage of the height index through the requested height.
    pub source_coverage: TransparentAddressDeltasSourceCoverage,
}

#[derive(Default)]
struct LifetimeAccumulator {
    received_zat: u64,
    sent_zat: u64,
    transaction_ids: HashSet<zinder_core::TransactionId>,
    first_block_time_unix_seconds: Option<i64>,
    last_block_time_unix_seconds: Option<i64>,
}

/// Materializes confirmed per-address value events.
#[derive(Default)]
pub struct TransparentAddressDeltasConsumer;

impl TransparentAddressDeltasConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the primary storage key for one value event.
    #[must_use]
    pub fn key_for_event(
        address: TransparentAddressScriptHash,
        height: BlockHeight,
        in_block_position: u32,
        kind_byte: u8,
        event_index: u32,
    ) -> [u8; TRANSPARENT_ADDRESS_DELTAS_KEY_LEN] {
        let mut key = [0u8; TRANSPARENT_ADDRESS_DELTAS_KEY_LEN];
        let position_end = ADDRESS_HASH_LEN + HEIGHT_LEN + POSITION_LEN;
        let kind_end = position_end + KIND_LEN;
        key[0..ADDRESS_HASH_LEN].copy_from_slice(&encode_address_script_hash(address));
        key[ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + HEIGHT_LEN]
            .copy_from_slice(&encode_height_key_ascending(height));
        key[ADDRESS_HASH_LEN + HEIGHT_LEN..position_end]
            .copy_from_slice(&encode_in_block_position(in_block_position));
        key[position_end] = kind_byte;
        key[kind_end..].copy_from_slice(&encode_in_block_position(event_index));
        key
    }

    /// Returns the address prefix (32 bytes) shared by every row for one
    /// address.
    #[must_use]
    pub const fn key_prefix_for_address(
        address: TransparentAddressScriptHash,
    ) -> [u8; ADDRESS_HASH_LEN] {
        encode_address_script_hash(address)
    }

    /// Streams all persisted delta rows and summarizes events through the
    /// requested inclusive height.
    ///
    /// Every key and payload is decoded even when its height is above the
    /// requested bound, so corrupt retained state never hides behind a lower
    /// bootstrap height. The separate source audit proves whether the index
    /// contains one row per block from height 1 through the bound.
    pub fn lifetime_summaries_through(
        store: &DeriveStore,
        through_height: BlockHeight,
    ) -> Result<TransparentAddressDeltasLifetimeBootstrap, DeriveStoreError> {
        let mut summaries = Vec::new();
        let mut current_address = None;
        let mut accumulator = LifetimeAccumulator::default();
        store.visit_consumer_rows(TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY, |key, payload| {
            let (address_script_hash, height, kind) = decode_delta_key(key)?;
            let record = TransparentAddressDeltasRecord::decode(payload)
                .map_err(|error| format!("invalid delta record: {error}"))?;
            let transaction_id = decode_rpc_transaction_id_hex(&record.transaction_id)
                .map_err(|error| format!("invalid transaction id: {error}"))?;
            if encode_rpc_transaction_id_hex(transaction_id) != record.transaction_id {
                return Err("transaction id is not canonical lowercase RPC hex".to_owned());
            }

            if current_address != Some(address_script_hash)
                && let Some(previous_address) = current_address.replace(address_script_hash)
            {
                push_lifetime_summary(
                    &mut summaries,
                    previous_address,
                    &std::mem::take(&mut accumulator),
                )?;
            }
            if height > through_height {
                return Ok(());
            }

            let magnitude = record.value_zat.unsigned_abs();
            if kind == TRANSPARENT_DELTA_KIND_RECEIVED_BYTE {
                if record.value_zat < 0 {
                    return Err("received delta record has a negative value".to_owned());
                }
                accumulator.received_zat = accumulator
                    .received_zat
                    .checked_add(magnitude)
                    .ok_or_else(|| "received_zat sum overflowed u64".to_owned())?;
            } else {
                if record.value_zat > 0 {
                    return Err("spent delta record has a positive value".to_owned());
                }
                accumulator.sent_zat = accumulator
                    .sent_zat
                    .checked_add(magnitude)
                    .ok_or_else(|| "sent_zat sum overflowed u64".to_owned())?;
            }
            accumulator.transaction_ids.insert(transaction_id);
            accumulator.first_block_time_unix_seconds = Some(
                accumulator
                    .first_block_time_unix_seconds
                    .map_or(record.block_time_unix_seconds, |time| {
                        time.min(record.block_time_unix_seconds)
                    }),
            );
            accumulator.last_block_time_unix_seconds = Some(
                accumulator
                    .last_block_time_unix_seconds
                    .map_or(record.block_time_unix_seconds, |time| {
                        time.max(record.block_time_unix_seconds)
                    }),
            );
            Ok(())
        })?;
        if let Some(address_script_hash) = current_address {
            push_lifetime_summary(&mut summaries, address_script_hash, &accumulator).map_err(
                |reason| DeriveStoreError::ConsumerPayloadDecode {
                    name: TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
                    reason,
                },
            )?;
        }

        Ok(TransparentAddressDeltasLifetimeBootstrap {
            summaries,
            source_coverage: source_coverage_through(store, through_height)?,
        })
    }
}

fn push_lifetime_summary(
    summaries: &mut Vec<TransparentAddressLifetimeSummary>,
    address_script_hash: TransparentAddressScriptHash,
    accumulator: &LifetimeAccumulator,
) -> Result<(), String> {
    if accumulator.transaction_ids.is_empty() {
        return Ok(());
    }
    summaries.push(
        lifetime_summary(address_script_hash, accumulator).map_err(|error| error.to_string())?,
    );
    Ok(())
}

fn decode_delta_key(key: &[u8]) -> Result<(TransparentAddressScriptHash, BlockHeight, u8), String> {
    if key.len() != TRANSPARENT_ADDRESS_DELTAS_KEY_LEN {
        return Err(format!(
            "delta key has {} bytes, expected {TRANSPARENT_ADDRESS_DELTAS_KEY_LEN}",
            key.len()
        ));
    }
    let address_script_hash = decode_address_script_hash(&key[..ADDRESS_HASH_LEN])
        .map_err(|error| format!("invalid address script hash: {error}"))?;
    let height = decode_height_key_ascending(&key[ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + HEIGHT_LEN])
        .map_err(|error| format!("invalid height: {error}"))?;
    let position_end = ADDRESS_HASH_LEN + HEIGHT_LEN + POSITION_LEN;
    decode_in_block_position(&key[ADDRESS_HASH_LEN + HEIGHT_LEN..position_end])
        .map_err(|error| format!("invalid in-block position: {error}"))?;
    let kind = key[position_end];
    match kind {
        TRANSPARENT_DELTA_KIND_RECEIVED_BYTE | TRANSPARENT_DELTA_KIND_SPENT_BYTE => {}
        kind => return Err(format!("invalid delta kind byte {kind}")),
    }
    decode_in_block_position(&key[position_end + KIND_LEN..])
        .map_err(|error| format!("invalid event index: {error}"))?;
    Ok((address_script_hash, height, kind))
}

fn lifetime_summary(
    address_script_hash: TransparentAddressScriptHash,
    accumulator: &LifetimeAccumulator,
) -> Result<TransparentAddressLifetimeSummary, DeriveStoreError> {
    let distinct_transaction_count =
        u64::try_from(accumulator.transaction_ids.len()).map_err(|_| {
            DeriveStoreError::ConsumerPayloadDecode {
                name: TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
                reason: "distinct transaction count overflowed u64".to_owned(),
            }
        })?;
    let net_balance_zat = i128::from(accumulator.received_zat) - i128::from(accumulator.sent_zat);
    Ok(TransparentAddressLifetimeSummary {
        address_script_hash,
        received_zat: accumulator.received_zat,
        sent_zat: accumulator.sent_zat,
        distinct_transaction_count,
        first_block_time_unix_seconds: accumulator
            .first_block_time_unix_seconds
            .unwrap_or_default(),
        last_block_time_unix_seconds: accumulator.last_block_time_unix_seconds.unwrap_or_default(),
        net_balance_zat,
        validated_balance_zat: u64::try_from(net_balance_zat).ok(),
    })
}

fn source_coverage_through(
    store: &DeriveStore,
    through_height: BlockHeight,
) -> Result<TransparentAddressDeltasSourceCoverage, DeriveStoreError> {
    let mut first_height = None;
    let mut last_height = None;
    let mut row_count = 0_u64;
    let mut next_contiguous_height = 1_u32;
    let mut is_contiguous = true;
    store.visit_consumer_rows(
        TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY,
        |key, payload| {
            let height = decode_height_key_ascending(key)
                .map_err(|error| format!("invalid delta-index height: {error}"))?;
            if payload.len() % INDEX_ENTRY_LEN != 0 {
                return Err(format!(
                    "delta-index payload at height {} has {} bytes, not a multiple of {INDEX_ENTRY_LEN}",
                    height.value(),
                    payload.len()
                ));
            }
            for entry in payload.chunks_exact(INDEX_ENTRY_LEN) {
                decode_index_entry(entry)?;
            }
            if height > through_height {
                return Ok(());
            }
            first_height.get_or_insert(height);
            last_height = Some(height);
            row_count = row_count
                .checked_add(1)
                .ok_or_else(|| "delta-index row count overflowed u64".to_owned())?;
            if height.value() != next_contiguous_height {
                is_contiguous = false;
            }
            next_contiguous_height = height.value().saturating_add(1);
            Ok(())
        },
    )?;
    let expected_row_count = u64::from(through_height.value());
    let contiguous_from_height_1 = is_contiguous
        && row_count == expected_row_count
        && next_contiguous_height == through_height.value().saturating_add(1);
    Ok(TransparentAddressDeltasSourceCoverage {
        first_height,
        last_height,
        row_count,
        contiguous_from_height_1,
    })
}

fn decode_index_entry(entry: &[u8]) -> Result<(), String> {
    decode_address_script_hash(&entry[..ADDRESS_HASH_LEN])
        .map_err(|error| format!("invalid delta-index address script hash: {error}"))?;
    let position_end = ADDRESS_HASH_LEN + POSITION_LEN;
    decode_in_block_position(&entry[ADDRESS_HASH_LEN..position_end])
        .map_err(|error| format!("invalid delta-index in-block position: {error}"))?;
    match entry[position_end] {
        TRANSPARENT_DELTA_KIND_RECEIVED_BYTE | TRANSPARENT_DELTA_KIND_SPENT_BYTE => {}
        kind => return Err(format!("invalid delta-index kind byte {kind}")),
    }
    decode_in_block_position(&entry[position_end + KIND_LEN..])
        .map_err(|error| format!("invalid delta-index event index: {error}"))?;
    Ok(())
}

impl BlockKeyedConsumer for TransparentAddressDeltasConsumer {
    fn name(&self) -> DeriveConsumerName {
        TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let transparent_spends = block.transparent_spends()?;
        let value_events = address_value_events(block, transparent_spends.as_deref());
        let primary_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY)?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY)?;

        let transaction_ids = transaction_ids_by_position(block);
        let mut index_payload: Vec<u8> = Vec::with_capacity(value_events.len() * INDEX_ENTRY_LEN);
        for event in &value_events {
            let Some(value_zat) = event.signed_value_zat() else {
                return Err(Box::new(
                    TransparentAddressDeltasConsumerError::ValueWidth {
                        height: block.height.value(),
                        value_zat: event.value_zat,
                    },
                ));
            };
            let kind_byte = event.kind.storage_byte();
            let key = Self::key_for_event(
                event.address_script_hash,
                block.height,
                event.in_block_position,
                kind_byte,
                event.event_index,
            );
            let record = TransparentAddressDeltasRecord {
                transaction_id: transaction_ids
                    .get(event.in_block_position as usize)
                    .cloned()
                    .unwrap_or_default(),
                block_time_unix_seconds: block.block_time_unix_seconds,
                value_zat,
            };
            let mut payload = Vec::with_capacity(record.encoded_len());
            record.encode(&mut payload).map_err(|error| {
                TransparentAddressDeltasConsumerError::Encode(error.to_string())
            })?;
            ctx.batch.put_cf(&primary_cf, key, payload);
            index_payload.extend_from_slice(&encode_address_script_hash(event.address_script_hash));
            index_payload.extend_from_slice(&encode_in_block_position(event.in_block_position));
            index_payload.push(kind_byte);
            index_payload.extend_from_slice(&encode_in_block_position(event.event_index));
        }

        ctx.batch.put_cf(
            &index_cf,
            encode_height_key_ascending(block.height),
            index_payload,
        );
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let index_key = encode_height_key_ascending(height);
        let Some(index_payload) = ctx
            .store
            .get_consumer(TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY, &index_key)?
        else {
            return Ok(());
        };
        if index_payload.len() % INDEX_ENTRY_LEN != 0 {
            return Err(Box::new(
                TransparentAddressDeltasConsumerError::IndexLengthMismatch {
                    height: height.value(),
                    bytes: index_payload.len(),
                },
            ));
        }
        let primary_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY)?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY)?;
        for chunk in index_payload.chunks_exact(INDEX_ENTRY_LEN) {
            let address_bytes: [u8; ADDRESS_HASH_LEN] =
                chunk[0..ADDRESS_HASH_LEN].try_into().unwrap_or([0u8; 32]);
            let position_bytes: [u8; POSITION_LEN] = chunk
                [ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + POSITION_LEN]
                .try_into()
                .unwrap_or([0u8; POSITION_LEN]);
            let kind_byte = chunk[ADDRESS_HASH_LEN + POSITION_LEN];
            let event_index_bytes: [u8; EVENT_INDEX_LEN] = chunk
                [ADDRESS_HASH_LEN + POSITION_LEN + KIND_LEN..]
                .try_into()
                .unwrap_or([0u8; EVENT_INDEX_LEN]);
            let address = TransparentAddressScriptHash::from_bytes(address_bytes);
            let position = u32::from_be_bytes(position_bytes);
            let event_index = u32::from_be_bytes(event_index_bytes);
            ctx.batch.delete_cf(
                &primary_cf,
                Self::key_for_event(address, height, position, kind_byte, event_index),
            );
        }
        ctx.batch.delete_cf(&index_cf, index_key);
        Ok(())
    }
}

/// Consumer-specific failure modes [`TransparentAddressDeltasConsumer`] can
/// surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransparentAddressDeltasConsumerError {
    /// Storage encoding of the materialized record failed.
    #[error("TransparentAddressDeltasRecord prost encode failed: {0}")]
    Encode(String),
    /// A value magnitude did not fit the signed 64-bit wire width.
    #[error(
        "transparent delta value {value_zat} at height {height} exceeds the signed 64-bit range"
    )]
    ValueWidth {
        /// Height of the offending event.
        height: u32,
        /// The magnitude that overflowed.
        value_zat: u64,
    },
    /// Per-height index entry was not a clean multiple of the entry length.
    #[error(
        "transparent_address_deltas_index entry for height {height} has {bytes} bytes, not a multiple of {INDEX_ENTRY_LEN}"
    )]
    IndexLengthMismatch {
        /// Height whose persisted index was malformed.
        height: u32,
        /// Byte length actually persisted.
        bytes: usize,
    },
}

#[cfg(test)]
mod tests {
    use eyre::Result;
    use tempfile::tempdir;
    use zinder_core::TransactionId;

    use super::*;
    use crate::DeriveStoreOptions;

    const ADDRESS_A: TransparentAddressScriptHash =
        TransparentAddressScriptHash::from_bytes([1; ADDRESS_HASH_LEN]);
    const ADDRESS_B: TransparentAddressScriptHash =
        TransparentAddressScriptHash::from_bytes([2; ADDRESS_HASH_LEN]);

    fn open_store() -> Result<(tempfile::TempDir, DeriveStore)> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[TRANSPARENT_ADDRESS_DELTAS_SCHEMA],
                ..DeriveStoreOptions::default()
            },
        )?;
        Ok((tempdir, store))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the fixture exposes every encoded delta-key and payload field"
    )]
    fn put_delta(
        store: &DeriveStore,
        address: TransparentAddressScriptHash,
        height: u32,
        position: u32,
        kind: u8,
        event_index: u32,
        transaction_byte: u8,
        block_time_unix_seconds: i64,
        value_zat: i64,
    ) -> Result<()> {
        let record = TransparentAddressDeltasRecord {
            transaction_id: encode_rpc_transaction_id_hex(TransactionId::from_bytes(
                [transaction_byte; 32],
            )),
            block_time_unix_seconds,
            value_zat,
        };
        store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
            &TransparentAddressDeltasConsumer::key_for_event(
                address,
                BlockHeight::new(height),
                position,
                kind,
                event_index,
            ),
            &record.encode_to_vec(),
        )?;
        Ok(())
    }

    fn put_index_height(store: &DeriveStore, height: u32) -> Result<()> {
        store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(height)),
            &[],
        )?;
        Ok(())
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the test proves grouping, deduplication, extrema, signed balances, and source coverage together"
    )]
    fn lifetime_bootstrap_groups_distinct_transactions_and_audits_coverage() -> Result<()> {
        let (_tempdir, store) = open_store()?;
        put_delta(
            &store,
            ADDRESS_A,
            1,
            0,
            TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
            0,
            11,
            300,
            10,
        )?;
        put_delta(
            &store,
            ADDRESS_A,
            1,
            0,
            TRANSPARENT_DELTA_KIND_SPENT_BYTE,
            1,
            11,
            300,
            -4,
        )?;
        put_delta(
            &store,
            ADDRESS_A,
            2,
            1,
            TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
            0,
            12,
            100,
            8,
        )?;
        put_delta(
            &store,
            ADDRESS_B,
            2,
            2,
            TRANSPARENT_DELTA_KIND_SPENT_BYTE,
            0,
            13,
            200,
            -7,
        )?;
        put_delta(
            &store,
            ADDRESS_A,
            3,
            0,
            TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
            0,
            14,
            400,
            20,
        )?;
        put_index_height(&store, 1)?;
        put_index_height(&store, 2)?;
        put_index_height(&store, 3)?;

        let bootstrap = TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &store,
            BlockHeight::new(2),
        )?;
        assert_eq!(
            bootstrap.summaries,
            vec![
                TransparentAddressLifetimeSummary {
                    address_script_hash: ADDRESS_A,
                    received_zat: 18,
                    sent_zat: 4,
                    distinct_transaction_count: 2,
                    first_block_time_unix_seconds: 100,
                    last_block_time_unix_seconds: 300,
                    net_balance_zat: 14,
                    validated_balance_zat: Some(14),
                },
                TransparentAddressLifetimeSummary {
                    address_script_hash: ADDRESS_B,
                    received_zat: 0,
                    sent_zat: 7,
                    distinct_transaction_count: 1,
                    first_block_time_unix_seconds: 200,
                    last_block_time_unix_seconds: 200,
                    net_balance_zat: -7,
                    validated_balance_zat: None,
                },
            ]
        );
        assert_eq!(
            bootstrap.source_coverage,
            TransparentAddressDeltasSourceCoverage {
                first_height: Some(BlockHeight::new(1)),
                last_height: Some(BlockHeight::new(2)),
                row_count: 2,
                contiguous_from_height_1: true,
            }
        );
        Ok(())
    }

    #[test]
    fn lifetime_bootstrap_rejects_malformed_primary_and_index_rows() -> Result<()> {
        let (_tempdir, store) = open_store()?;
        store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
            b"short",
            &TransparentAddressDeltasRecord {
                transaction_id: encode_rpc_transaction_id_hex(TransactionId::from_bytes([9; 32])),
                block_time_unix_seconds: 1,
                value_zat: 1,
            }
            .encode_to_vec(),
        )?;
        let malformed_primary = TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &store,
            BlockHeight::new(1),
        );
        assert!(matches!(
            malformed_primary,
            Err(DeriveStoreError::ConsumerPayloadDecode { name, .. })
                if name == TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY
        ));

        let (_tempdir, store) = open_store()?;
        store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
            &TransparentAddressDeltasConsumer::key_for_event(
                ADDRESS_A,
                BlockHeight::new(1),
                0,
                TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
                0,
            ),
            &[0x80],
        )?;
        let malformed_payload = TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &store,
            BlockHeight::new(1),
        );
        assert!(matches!(
            malformed_payload,
            Err(DeriveStoreError::ConsumerPayloadDecode { name, .. })
                if name == TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY
        ));

        let (_tempdir, store) = open_store()?;
        store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(1)),
            &[0],
        )?;
        let malformed_index = TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &store,
            BlockHeight::new(1),
        );
        assert!(matches!(
            malformed_index,
            Err(DeriveStoreError::ConsumerPayloadDecode { name, .. })
                if name == TRANSPARENT_ADDRESS_DELTAS_INDEX_COLUMN_FAMILY
        ));
        Ok(())
    }

    #[test]
    fn source_coverage_reports_height_gaps() -> Result<()> {
        let (_tempdir, store) = open_store()?;
        put_index_height(&store, 1)?;
        put_index_height(&store, 3)?;

        let coverage = TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &store,
            BlockHeight::new(3),
        )?
        .source_coverage;
        assert_eq!(coverage.first_height, Some(BlockHeight::new(1)));
        assert_eq!(coverage.last_height, Some(BlockHeight::new(3)));
        assert_eq!(coverage.row_count, 2);
        assert!(!coverage.contiguous_from_height_1);
        Ok(())
    }
}
