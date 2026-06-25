//! Stream cursor byte contract.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use zinder_core::{BlockHash, BlockHeight, Network, TransactionId, TransparentOutPoint};

/// Fixed byte length of a fixed-body [`StreamCursorTokenV1`].
///
/// Mempool, transparent-history, and address-output cursors are exactly this
/// long. Chain-event cursors append a variable-length locator after the fixed
/// body, so their length grows with the number of locator entries.
pub const STREAM_CURSOR_TOKEN_V1_LEN: usize = 82;

const STREAM_CURSOR_SCHEMA_VERSION: u8 = 1;
const STREAM_FAMILY_MASK: u8 = 0x0f;
const STREAM_RESERVED_FLAGS_MASK: u8 = 0xf0;
const CURSOR_BODY_LEN: usize = 50;
const AUTH_TAG_LEN: usize = 32;

/// Maximum number of `(height, hash)` entries a chain-event cursor locator carries.
///
/// The cap bounds both the recoverable reorg depth and the cursor size:
/// entries are exponentially back-spaced from the tip, so 32 entries reach
/// roughly 2^31 blocks of fork depth.
pub(crate) const CHAIN_EVENT_LOCATOR_MAX: usize = 32;

/// Byte length of one locator entry: a big-endian `u32` height followed by a
/// 32-byte block hash.
const CHAIN_EVENT_LOCATOR_ENTRY_LEN: usize = 36;

type HmacSha256 = Hmac<Sha256>;

/// Chain-event stream family encoded in the low nibble of a cursor flags byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChainEventStreamFamily {
    /// Tip stream: every committed chain event, including reorg events.
    Tip,
    /// Safe stream: only commits whose range is past the reorg window;
    /// never reorg events.
    Safe,
}

impl ChainEventStreamFamily {
    pub(crate) const fn flags(self) -> u8 {
        match self {
            Self::Tip => 0x0,
            Self::Safe => 0x1,
        }
    }

    const fn from_flags(flags: u8) -> Option<Self> {
        if flags & STREAM_RESERVED_FLAGS_MASK != 0 {
            return None;
        }

        match flags & STREAM_FAMILY_MASK {
            0x0 => Some(Self::Tip),
            0x1 => Some(Self::Safe),
            _ => None,
        }
    }
}

/// Mempool-event stream family encoded in the low nibble of a cursor flags
/// byte.
///
/// Mempool cursors share the [`StreamCursorTokenV1`] envelope but carry a
/// different body shape (sequence + last transaction id rather than
/// sequence + last height/hash). The flags byte at offset 49 distinguishes
/// the two; chain-event decoders reject mempool cursors with
/// [`StreamCursorError::StreamFamilyMismatch`] and vice versa.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MempoolEventStreamFamily {
    /// Single mempool event family (flag `0x2`). Reserved nibble values
    /// `0x3..0xF` are available for future mempool-stream variants (e.g. a
    /// transparent-only family).
    Mempool,
}

impl MempoolEventStreamFamily {
    pub(crate) const fn flags(self) -> u8 {
        match self {
            Self::Mempool => 0x2,
        }
    }

    const fn from_flags(flags: u8) -> Option<Self> {
        if flags & STREAM_RESERVED_FLAGS_MASK != 0 {
            return None;
        }
        match flags & STREAM_FAMILY_MASK {
            0x2 => Some(Self::Mempool),
            _ => None,
        }
    }
}

/// Cursor payload decoded from a mempool-event stream cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MempoolEventCursorPayload {
    /// Stream family encoded in the cursor flags byte.
    pub family: MempoolEventStreamFamily,
    /// Mempool event sequence carried by the cursor.
    pub event_sequence: u64,
    /// Identifier of the mempool transaction last delivered before this
    /// cursor was issued.
    pub last_transaction_id: TransactionId,
}

/// Transparent-history stream family encoded in a cursor flags byte.
///
/// The single variant `TransparentHistory` (flag `0x3`) bookmarks a
/// position inside a transparent-address tx-history scan. The high nibble
/// of the flags byte encodes the iteration direction so resume continues
/// in the same order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransparentHistoryStreamFamily {
    /// Transparent-address tx-history bookmark family (flag `0x3`).
    TransparentHistory,
}

const STREAM_DIRECTION_FLAG_BIT: u8 = 0x10;

impl TransparentHistoryStreamFamily {
    pub(crate) const fn flags(self, descending: bool) -> u8 {
        let direction_bit = if descending {
            STREAM_DIRECTION_FLAG_BIT
        } else {
            0
        };
        match self {
            Self::TransparentHistory => 0x3 | direction_bit,
        }
    }

    const fn from_flags(flags: u8) -> Option<(Self, bool)> {
        let reserved_mask = STREAM_RESERVED_FLAGS_MASK & !STREAM_DIRECTION_FLAG_BIT;
        if flags & reserved_mask != 0 {
            return None;
        }
        let descending = flags & STREAM_DIRECTION_FLAG_BIT != 0;
        match flags & STREAM_FAMILY_MASK {
            0x3 => Some((Self::TransparentHistory, descending)),
            _ => None,
        }
    }
}

/// Anchor used to construct a transparent-history cursor token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentHistoryCursorAnchor {
    /// Network the cursor is bound to.
    pub network: Network,
    /// Stream family encoded in the cursor flags byte.
    pub family: TransparentHistoryStreamFamily,
    /// Block height of the last yielded entry.
    pub last_block_height: BlockHeight,
    /// Position of the last yielded entry inside its block.
    pub last_tx_index_in_block: u32,
    /// Iteration direction at issue time.
    pub descending: bool,
}

/// Cursor payload decoded from a transparent-history stream cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentHistoryCursorPayload {
    /// Stream family encoded in the cursor flags byte.
    pub family: TransparentHistoryStreamFamily,
    /// Descending iteration direction encoded in the cursor flags byte.
    pub descending: bool,
    /// Block height of the last yielded transaction.
    pub last_block_height: BlockHeight,
    /// Position of the last yielded transaction inside its block.
    pub last_tx_index_in_block: u32,
}

/// Transparent-output stream family encoded in the low nibble of a cursor
/// flags byte.
///
/// The single variant `AddressOutput` (flag `0x4`) bookmarks a position
/// inside a `(network, address_script_hash)` prefix scan. The cursor body
/// records the last yielded `(block_height, outpoint)` so resuming
/// continues strictly after that output regardless of how the upstream
/// iterator dedupes outpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressOutputStreamFamily {
    /// output bookmark family (flag `0x4`).
    AddressOutput,
}

impl AddressOutputStreamFamily {
    pub(crate) const fn flags(self) -> u8 {
        match self {
            Self::AddressOutput => 0x4,
        }
    }

    const fn from_flags(flags: u8) -> Option<Self> {
        if flags & STREAM_RESERVED_FLAGS_MASK != 0 {
            return None;
        }
        match flags & STREAM_FAMILY_MASK {
            0x4 => Some(Self::AddressOutput),
            _ => None,
        }
    }
}

/// Cursor payload decoded from a transparent-output stream cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressOutputCursorPayload {
    /// Stream family encoded in the cursor flags byte.
    pub family: AddressOutputStreamFamily,
    /// Block height of the last yielded output.
    pub last_block_height: BlockHeight,
    /// Outpoint of the last yielded output.
    pub last_outpoint: TransparentOutPoint,
}

/// Snapshot-page stream family encoded in the low nibble of a cursor flags
/// byte.
///
/// The single variant `SnapshotPage` (flag `0x5`) bookmarks a position inside
/// one `MempoolSnapshot` paging walk. The body carries the snapshot sequence
/// the page belongs to and the last yielded transaction id, so resuming
/// continues strictly after that transaction within the same snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotPageStreamFamily {
    /// Mempool snapshot paging family (flag `0x5`).
    SnapshotPage,
}

impl SnapshotPageStreamFamily {
    pub(crate) const fn flags(self) -> u8 {
        match self {
            Self::SnapshotPage => 0x5,
        }
    }

    const fn from_flags(flags: u8) -> Option<Self> {
        if flags & STREAM_RESERVED_FLAGS_MASK != 0 {
            return None;
        }
        match flags & STREAM_FAMILY_MASK {
            0x5 => Some(Self::SnapshotPage),
            _ => None,
        }
    }
}

/// Cursor payload decoded from a mempool-snapshot paging cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotPageCursorPayload {
    /// Stream family encoded in the cursor flags byte.
    pub family: SnapshotPageStreamFamily,
    /// Snapshot sequence the page belongs to.
    pub snapshot_sequence: u64,
    /// Identifier of the snapshot transaction last delivered before this
    /// cursor was issued.
    pub after_transaction_id: TransactionId,
}

/// One `(height, hash)` pair in a chain-event cursor locator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainEventCursorAnchor {
    pub(crate) height: BlockHeight,
    pub(crate) hash: BlockHash,
}

/// Back-spaced `(height, hash)` pairs that let the server resolve a fork point
/// against the block index even after the event log pruned the divergence.
///
/// Entries are ordered tip-first: the first entry is the cursor's own tip and
/// later entries reach exponentially further back. Always carries at least the
/// tip entry and at most [`CHAIN_EVENT_LOCATOR_MAX`] entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChainEventLocator {
    entries: Vec<ChainEventCursorAnchor>,
}

impl ChainEventLocator {
    /// Builds a locator from tip-first `(height, hash)` entries.
    ///
    /// The first entry is the tip; callers supply the back-spaced ancestors in
    /// descending-height order after it. Rejects an empty list or one that
    /// exceeds [`CHAIN_EVENT_LOCATOR_MAX`].
    pub(crate) fn new(entries: Vec<ChainEventCursorAnchor>) -> Result<Self, StreamCursorError> {
        if entries.is_empty() || entries.len() > CHAIN_EVENT_LOCATOR_MAX {
            return Err(StreamCursorError::InvalidLocatorLength {
                entry_count: entries.len(),
            });
        }
        Ok(Self { entries })
    }

    /// Returns the locator's tip entry, the most recent position it bookmarks.
    pub(crate) fn tip(&self) -> ChainEventCursorAnchor {
        // The constructor rejects an empty entry list, so the first entry is
        // always present.
        self.entries
            .first()
            .copied()
            .unwrap_or(ChainEventCursorAnchor {
                height: BlockHeight::new(0),
                hash: BlockHash::from_bytes([0; 32]),
            })
    }

    /// Returns the locator entries, ordered tip-first.
    pub(crate) fn entries(&self) -> &[ChainEventCursorAnchor] {
        &self.entries
    }
}

/// Cursor payload decoded from a chain-event stream cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChainEventCursorPayload {
    pub(crate) family: ChainEventStreamFamily,
    pub(crate) event_sequence: u64,
    pub(crate) locator: ChainEventLocator,
}

/// Fixed-layout cursor token for resumable streams.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamCursorTokenV1(Vec<u8>);

impl StreamCursorTokenV1 {
    pub(crate) fn chain_event(
        network: Network,
        family: ChainEventStreamFamily,
        event_sequence: u64,
        locator: &ChainEventLocator,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let tip = locator.tip();
        let additional_entries = &locator.entries()[1..];
        let mut cursor_bytes = Vec::with_capacity(
            CURSOR_BODY_LEN
                + 1
                + additional_entries.len() * CHAIN_EVENT_LOCATOR_ENTRY_LEN
                + AUTH_TAG_LEN,
        );
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&event_sequence.to_be_bytes());
        cursor_bytes.extend_from_slice(&tip.height.value().to_be_bytes());
        cursor_bytes.extend_from_slice(&tip.hash.as_bytes());
        cursor_bytes.push(family.flags());
        // The tip entry is encoded inline above; the count byte covers only the
        // back-spaced ancestors that follow it.
        let additional_count = u8::try_from(additional_entries.len()).map_err(|_| {
            StreamCursorError::InvalidLocatorLength {
                entry_count: locator.entries().len(),
            }
        })?;
        cursor_bytes.push(additional_count);
        for entry in additional_entries {
            cursor_bytes.extend_from_slice(&entry.height.value().to_be_bytes());
            cursor_bytes.extend_from_slice(&entry.hash.as_bytes());
        }

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    pub(crate) fn decode_chain_event(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<ChainEventCursorPayload, StreamCursorError> {
        // Fixed body (50 bytes) + locator count (1 byte) + auth tag (32 bytes)
        // is the shortest a chain-event cursor can be (zero ancestor entries).
        let min_len = CURSOR_BODY_LEN + 1 + AUTH_TAG_LEN;
        if self.0.len() < min_len {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let family = ChainEventStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;
        if !matches!(
            family,
            ChainEventStreamFamily::Tip | ChainEventStreamFamily::Safe
        ) {
            return Err(StreamCursorError::StreamFamilyMismatch { flags });
        }

        let additional_count = usize::from(self.0[CURSOR_BODY_LEN]);
        if additional_count >= CHAIN_EVENT_LOCATOR_MAX {
            return Err(StreamCursorError::InvalidLocatorLength {
                entry_count: additional_count.saturating_add(1),
            });
        }
        let body_len = CURSOR_BODY_LEN + 1 + additional_count * CHAIN_EVENT_LOCATOR_ENTRY_LEN;
        let expected_len = body_len + AUTH_TAG_LEN;
        if self.0.len() != expected_len {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        verify_auth_tag(cursor_auth_key, &self.0[..body_len], &self.0[body_len..])?;

        let mut entries = Vec::with_capacity(additional_count.saturating_add(1));
        entries.push(ChainEventCursorAnchor {
            height: BlockHeight::new(read_u32_be(&self.0, 13)?),
            hash: read_block_hash(&self.0, 17)?,
        });
        let mut offset = CURSOR_BODY_LEN + 1;
        for _ in 0..additional_count {
            entries.push(ChainEventCursorAnchor {
                height: BlockHeight::new(read_u32_be(&self.0, offset)?),
                hash: read_block_hash(&self.0, offset + 4)?,
            });
            offset += CHAIN_EVENT_LOCATOR_ENTRY_LEN;
        }

        Ok(ChainEventCursorPayload {
            family,
            event_sequence: read_u64_be(&self.0, 5)?,
            locator: ChainEventLocator::new(entries)?,
        })
    }

    /// Builds a mempool-event cursor token.
    pub fn mempool_event(
        network: Network,
        family: MempoolEventStreamFamily,
        event_sequence: u64,
        last_transaction_id: TransactionId,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let mut cursor_bytes = Vec::with_capacity(STREAM_CURSOR_TOKEN_V1_LEN);
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&event_sequence.to_be_bytes());
        cursor_bytes.extend_from_slice(&last_transaction_id.as_bytes());
        // Reserved padding to keep the body 50 bytes long (1 + 4 + 8 + 32 +
        // 4 + 1) so chain and mempool cursors share the on-the-wire length.
        cursor_bytes.extend_from_slice(&[0u8; 4]);
        cursor_bytes.push(family.flags());

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    /// Decodes a mempool-event cursor token.
    pub fn decode_mempool_event(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<MempoolEventCursorPayload, StreamCursorError> {
        if self.0.len() != STREAM_CURSOR_TOKEN_V1_LEN {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let family = MempoolEventStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;

        verify_auth_tag(
            cursor_auth_key,
            &self.0[..CURSOR_BODY_LEN],
            &self.0[CURSOR_BODY_LEN..],
        )?;

        let transaction_id_bytes = <[u8; 32]>::try_from(&self.0[13..45]).map_err(|_| {
            StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            }
        })?;

        Ok(MempoolEventCursorPayload {
            family,
            event_sequence: read_u64_be(&self.0, 5)?,
            last_transaction_id: TransactionId::from_bytes(transaction_id_bytes),
        })
    }

    /// Builds a transparent-history cursor token bookmarking
    /// `(last_block_height, last_tx_index_in_block)` plus the iteration
    /// direction.
    pub fn transparent_history(
        anchor: TransparentHistoryCursorAnchor,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let mut cursor_bytes = Vec::with_capacity(STREAM_CURSOR_TOKEN_V1_LEN);
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&anchor.network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&anchor.last_block_height.value().to_be_bytes());
        cursor_bytes.extend_from_slice(&anchor.last_tx_index_in_block.to_be_bytes());
        // Reserved padding to keep the body 50 bytes long
        // (1 + 4 + 4 + 4 + 36 + 1).
        cursor_bytes.extend_from_slice(&[0u8; 36]);
        cursor_bytes.push(anchor.family.flags(anchor.descending));

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    /// Decodes a transparent-history cursor token.
    pub fn decode_transparent_history(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<TransparentHistoryCursorPayload, StreamCursorError> {
        if self.0.len() != STREAM_CURSOR_TOKEN_V1_LEN {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let (family, descending) = TransparentHistoryStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;

        verify_auth_tag(
            cursor_auth_key,
            &self.0[..CURSOR_BODY_LEN],
            &self.0[CURSOR_BODY_LEN..],
        )?;

        Ok(TransparentHistoryCursorPayload {
            family,
            descending,
            last_block_height: BlockHeight::new(read_u32_be(&self.0, 5)?),
            last_tx_index_in_block: read_u32_be(&self.0, 9)?,
        })
    }

    /// Builds a transparent-output cursor token bookmarking
    /// `(last_block_height, last_outpoint)`.
    pub fn address_output(
        network: Network,
        family: AddressOutputStreamFamily,
        last_block_height: BlockHeight,
        last_outpoint: TransparentOutPoint,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let mut cursor_bytes = Vec::with_capacity(STREAM_CURSOR_TOKEN_V1_LEN);
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&last_block_height.value().to_be_bytes());
        cursor_bytes.extend_from_slice(&last_outpoint.transaction_id.as_bytes());
        cursor_bytes.extend_from_slice(&last_outpoint.output_index.to_be_bytes());
        // Reserved padding to keep the body 50 bytes long
        // (1 + 4 + 4 + 32 + 4 + 4 + 1).
        cursor_bytes.extend_from_slice(&[0u8; 4]);
        cursor_bytes.push(family.flags());

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    /// Decodes a transparent-output cursor token.
    pub fn decode_address_output(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<AddressOutputCursorPayload, StreamCursorError> {
        if self.0.len() != STREAM_CURSOR_TOKEN_V1_LEN {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let family = AddressOutputStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;

        verify_auth_tag(
            cursor_auth_key,
            &self.0[..CURSOR_BODY_LEN],
            &self.0[CURSOR_BODY_LEN..],
        )?;

        let transaction_id_bytes =
            <[u8; 32]>::try_from(&self.0[9..41]).map_err(|_| StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            })?;

        Ok(AddressOutputCursorPayload {
            family,
            last_block_height: BlockHeight::new(read_u32_be(&self.0, 5)?),
            last_outpoint: TransparentOutPoint {
                transaction_id: TransactionId::from_bytes(transaction_id_bytes),
                output_index: read_u32_be(&self.0, 41)?,
            },
        })
    }

    /// Builds a mempool-snapshot paging cursor token bookmarking
    /// `(snapshot_sequence, after_transaction_id)`.
    pub fn snapshot_page(
        network: Network,
        family: SnapshotPageStreamFamily,
        snapshot_sequence: u64,
        after_transaction_id: TransactionId,
        cursor_auth_key: [u8; 32],
    ) -> Result<Self, StreamCursorError> {
        let mut cursor_bytes = Vec::with_capacity(STREAM_CURSOR_TOKEN_V1_LEN);
        cursor_bytes.push(STREAM_CURSOR_SCHEMA_VERSION);
        cursor_bytes.extend_from_slice(&network.id().to_be_bytes());
        cursor_bytes.extend_from_slice(&snapshot_sequence.to_be_bytes());
        cursor_bytes.extend_from_slice(&after_transaction_id.as_bytes());
        // Reserved padding to keep the body 50 bytes long (1 + 4 + 8 + 32 +
        // 4 + 1) so this cursor shares the on-the-wire length of the family.
        cursor_bytes.extend_from_slice(&[0u8; 4]);
        cursor_bytes.push(family.flags());

        let auth_tag = compute_auth_tag(cursor_auth_key, &cursor_bytes)?;
        cursor_bytes.extend_from_slice(&auth_tag);

        Ok(Self(cursor_bytes))
    }

    /// Decodes a mempool-snapshot paging cursor token.
    pub fn decode_snapshot_page(
        &self,
        expected_network: Network,
        cursor_auth_key: [u8; 32],
    ) -> Result<SnapshotPageCursorPayload, StreamCursorError> {
        if self.0.len() != STREAM_CURSOR_TOKEN_V1_LEN {
            return Err(StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            });
        }

        if self.0[0] != STREAM_CURSOR_SCHEMA_VERSION {
            return Err(StreamCursorError::UnsupportedSchemaVersion { version: self.0[0] });
        }

        let network_id = read_u32_be(&self.0, 1)?;
        let network =
            Network::from_id(network_id).ok_or(StreamCursorError::UnknownNetwork { network_id })?;
        if network != expected_network {
            return Err(StreamCursorError::NetworkMismatch {
                expected: expected_network,
                actual: network,
            });
        }

        let flags = self.0[49];
        let family = SnapshotPageStreamFamily::from_flags(flags)
            .ok_or(StreamCursorError::StreamFamilyMismatch { flags })?;

        verify_auth_tag(
            cursor_auth_key,
            &self.0[..CURSOR_BODY_LEN],
            &self.0[CURSOR_BODY_LEN..],
        )?;

        let transaction_id_bytes = <[u8; 32]>::try_from(&self.0[13..45]).map_err(|_| {
            StreamCursorError::InvalidLength {
                byte_count: self.0.len(),
            }
        })?;

        Ok(SnapshotPageCursorPayload {
            family,
            snapshot_sequence: read_u64_be(&self.0, 5)?,
            after_transaction_id: TransactionId::from_bytes(transaction_id_bytes),
        })
    }

    /// Creates a cursor token from encoded bytes supplied by a client.
    #[must_use]
    pub fn from_bytes(cursor_bytes: impl Into<Vec<u8>>) -> Self {
        Self(cursor_bytes.into())
    }

    /// Returns the encoded cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Encodes this cursor token as unpadded base64url text.
    #[must_use]
    pub fn to_base64url(&self) -> String {
        BASE64_URL_SAFE_NO_PAD.encode(&self.0)
    }

    /// Decodes a cursor token from unpadded base64url text.
    pub fn from_base64url(cursor_text: &str) -> Result<Self, StreamCursorError> {
        BASE64_URL_SAFE_NO_PAD
            .decode(cursor_text)
            .map(Self)
            .map_err(|_| StreamCursorError::InvalidBase64)
    }
}

fn compute_auth_tag(
    cursor_auth_key: [u8; 32],
    cursor_body: &[u8],
) -> Result<[u8; AUTH_TAG_LEN], StreamCursorError> {
    let mut mac = cursor_mac(cursor_auth_key)?;
    mac.update(cursor_body);

    let digest = mac.finalize().into_bytes();
    let mut auth_tag = [0; AUTH_TAG_LEN];
    auth_tag.copy_from_slice(&digest);
    Ok(auth_tag)
}

fn verify_auth_tag(
    cursor_auth_key: [u8; 32],
    cursor_body: &[u8],
    auth_tag: &[u8],
) -> Result<(), StreamCursorError> {
    let mut mac = cursor_mac(cursor_auth_key)?;
    mac.update(cursor_body);
    mac.verify_slice(auth_tag)
        .map_err(|_| StreamCursorError::InvalidAuthTag)
}

fn cursor_mac(cursor_auth_key: [u8; 32]) -> Result<HmacSha256, StreamCursorError> {
    <HmacSha256 as Mac>::new_from_slice(&cursor_auth_key)
        .map_err(|_| StreamCursorError::InvalidAuthKey)
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32, StreamCursorError> {
    let end = offset
        .checked_add(4)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes = bytes
        .get(offset..end)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes =
        <[u8; 4]>::try_from(number_bytes).map_err(|_| StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    Ok(u32::from_be_bytes(number_bytes))
}

fn read_u64_be(bytes: &[u8], offset: usize) -> Result<u64, StreamCursorError> {
    let end = offset
        .checked_add(8)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes = bytes
        .get(offset..end)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let number_bytes =
        <[u8; 8]>::try_from(number_bytes).map_err(|_| StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    Ok(u64::from_be_bytes(number_bytes))
}

fn read_block_hash(bytes: &[u8], offset: usize) -> Result<BlockHash, StreamCursorError> {
    let end = offset
        .checked_add(32)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let hash_bytes = bytes
        .get(offset..end)
        .ok_or(StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    let hash_bytes =
        <[u8; 32]>::try_from(hash_bytes).map_err(|_| StreamCursorError::InvalidLength {
            byte_count: bytes.len(),
        })?;
    Ok(BlockHash::from_bytes(hash_bytes))
}

/// Error returned while decoding a stream cursor token.
#[derive(Debug, Error)]
pub enum StreamCursorError {
    /// Cursor byte length does not match its declared shape.
    #[error("stream cursor has invalid length: {byte_count} bytes")]
    InvalidLength {
        /// Cursor byte length.
        byte_count: usize,
    },

    /// Chain-event cursor locator carries an out-of-range entry count.
    #[error("stream cursor locator has invalid entry count: {entry_count}")]
    InvalidLocatorLength {
        /// Number of locator entries that failed the bound check.
        entry_count: usize,
    },

    /// Cursor schema version is not supported.
    #[error("stream cursor schema version {version} is unsupported")]
    UnsupportedSchemaVersion {
        /// Unsupported schema version.
        version: u8,
    },

    /// Cursor network id is unknown.
    #[error("stream cursor network id {network_id} is unknown")]
    UnknownNetwork {
        /// Unknown network id.
        network_id: u32,
    },

    /// Cursor belongs to a different network.
    #[error("stream cursor network mismatch: expected {expected:?}, actual {actual:?}")]
    NetworkMismatch {
        /// Expected network.
        expected: Network,
        /// Actual cursor network.
        actual: Network,
    },

    /// Cursor belongs to a different stream family.
    #[error("stream cursor family mismatch: flags {flags}")]
    StreamFamilyMismatch {
        /// Cursor flags byte.
        flags: u8,
    },

    /// Cursor authentication tag does not match its body.
    #[error("stream cursor authentication tag is invalid")]
    InvalidAuthTag,

    /// Store cursor authentication key could not initialize the MAC.
    #[error("stream cursor authentication key is invalid")]
    InvalidAuthKey,

    /// Cursor text is not valid unpadded base64url.
    #[error("stream cursor is not valid base64url")]
    InvalidBase64,
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHash, BlockHeight, Network, TransactionId, TransparentOutPoint};

    use super::{
        AUTH_TAG_LEN, AddressOutputCursorPayload, AddressOutputStreamFamily,
        CHAIN_EVENT_LOCATOR_MAX, CURSOR_BODY_LEN, ChainEventCursorAnchor, ChainEventLocator,
        ChainEventStreamFamily, STREAM_CURSOR_TOKEN_V1_LEN, SnapshotPageCursorPayload,
        SnapshotPageStreamFamily, StreamCursorError, StreamCursorTokenV1,
    };

    const CURSOR_AUTH_KEY: [u8; 32] = [7; 32];

    #[test]
    fn chain_event_cursor_round_trips_through_base64url() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;
        let cursor_text = cursor.to_base64url();
        let decoded = StreamCursorTokenV1::from_base64url(&cursor_text)?;

        assert_eq!(decoded, cursor);
        assert!(!cursor_text.contains('='));

        Ok(())
    }

    #[test]
    fn single_entry_locator_keeps_v1_byte_offsets() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;
        let cursor_bytes = cursor.as_bytes();

        // A one-entry locator carries no ancestors, so the cursor is the fixed
        // body, the zero count byte, and the auth tag.
        assert_eq!(cursor_bytes.len(), STREAM_CURSOR_TOKEN_V1_LEN + 1);
        assert_eq!(cursor_bytes[0], 1);
        assert_eq!(
            &cursor_bytes[1..5],
            &Network::ZcashRegtest.id().to_be_bytes()
        );
        assert_eq!(&cursor_bytes[5..13], &42_u64.to_be_bytes());
        assert_eq!(&cursor_bytes[13..17], &7_u32.to_be_bytes());
        assert_eq!(&cursor_bytes[17..49], &[9; 32]);
        assert_eq!(cursor_bytes[49], ChainEventStreamFamily::Tip.flags());
        assert_eq!(cursor_bytes[50], 0);
        assert_eq!(cursor_bytes[51..].len(), 32);

        Ok(())
    }

    #[test]
    fn multi_entry_locator_round_trips() -> Result<(), StreamCursorError> {
        let locator = ChainEventLocator::new(vec![
            ChainEventCursorAnchor {
                height: BlockHeight::new(100),
                hash: BlockHash::from_bytes([1; 32]),
            },
            ChainEventCursorAnchor {
                height: BlockHeight::new(99),
                hash: BlockHash::from_bytes([2; 32]),
            },
            ChainEventCursorAnchor {
                height: BlockHeight::new(96),
                hash: BlockHash::from_bytes([3; 32]),
            },
        ])?;
        let cursor = StreamCursorTokenV1::chain_event(
            Network::ZcashRegtest,
            ChainEventStreamFamily::Tip,
            7,
            &locator,
            CURSOR_AUTH_KEY,
        )?;

        let decoded = cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY)?;
        assert_eq!(decoded.event_sequence, 7);
        assert_eq!(decoded.family, ChainEventStreamFamily::Tip);
        assert_eq!(decoded.locator, locator);

        Ok(())
    }

    #[test]
    fn full_depth_locator_round_trips() -> Result<(), StreamCursorError> {
        let entries = (0..CHAIN_EVENT_LOCATOR_MAX)
            .map(|index| {
                let offset = u32::try_from(index).unwrap_or(u32::MAX);
                ChainEventCursorAnchor {
                    height: BlockHeight::new(1_000_000u32.saturating_sub(offset)),
                    hash: BlockHash::from_bytes([u8::try_from(index % 256).unwrap_or(0); 32]),
                }
            })
            .collect::<Vec<_>>();
        let locator = ChainEventLocator::new(entries)?;
        let cursor = StreamCursorTokenV1::chain_event(
            Network::ZcashRegtest,
            ChainEventStreamFamily::Safe,
            123,
            &locator,
            CURSOR_AUTH_KEY,
        )?;

        let decoded = cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY)?;
        assert_eq!(decoded.locator, locator);
        assert_eq!(decoded.family, ChainEventStreamFamily::Safe);

        Ok(())
    }

    #[test]
    fn tampered_locator_entry_fails_auth() -> Result<(), StreamCursorError> {
        let locator = ChainEventLocator::new(vec![
            ChainEventCursorAnchor {
                height: BlockHeight::new(100),
                hash: BlockHash::from_bytes([1; 32]),
            },
            ChainEventCursorAnchor {
                height: BlockHeight::new(99),
                hash: BlockHash::from_bytes([2; 32]),
            },
        ])?;
        let cursor = StreamCursorTokenV1::chain_event(
            Network::ZcashRegtest,
            ChainEventStreamFamily::Tip,
            7,
            &locator,
            CURSOR_AUTH_KEY,
        )?;
        let mut cursor_bytes = cursor.as_bytes().to_vec();
        // Flip a byte inside the appended ancestor entry, past the fixed body.
        cursor_bytes[55] ^= 1;
        let tampered = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            tampered.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidAuthTag)
        ));

        Ok(())
    }

    #[test]
    fn locator_with_too_many_entries_is_rejected() {
        let entries = (0..=CHAIN_EVENT_LOCATOR_MAX)
            .map(|index| ChainEventCursorAnchor {
                height: BlockHeight::new(u32::try_from(index).unwrap_or(u32::MAX)),
                hash: BlockHash::from_bytes([0; 32]),
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            ChainEventLocator::new(entries),
            Err(StreamCursorError::InvalidLocatorLength { .. })
        ));
    }

    #[test]
    fn empty_locator_is_rejected() {
        assert!(matches!(
            ChainEventLocator::new(Vec::new()),
            Err(StreamCursorError::InvalidLocatorLength { entry_count: 0 })
        ));
    }

    #[test]
    fn malformed_base64url_cursor_is_rejected() {
        assert!(matches!(
            StreamCursorTokenV1::from_base64url("not valid cursor text!"),
            Err(StreamCursorError::InvalidBase64)
        ));
    }

    #[test]
    fn invalid_length_cursor_is_rejected() {
        let cursor = StreamCursorTokenV1::from_bytes(vec![0]);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidLength { byte_count: 1 })
        ));
    }

    #[test]
    fn cursor_one_byte_below_chain_event_minimum_is_rejected() {
        // The shortest chain-event cursor is the body, the locator-count byte,
        // and the auth tag; dropping the count byte must fail the length check.
        let too_short = CURSOR_BODY_LEN + AUTH_TAG_LEN;
        let cursor = StreamCursorTokenV1::from_bytes(vec![0u8; too_short]);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidLength { byte_count }) if byte_count == too_short
        ));
    }

    #[test]
    fn unsupported_schema_version_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[0] = 2;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::UnsupportedSchemaVersion { version: 2 })
        ));
        Ok(())
    }

    #[test]
    fn unknown_network_id_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[1..5].copy_from_slice(&9999_u32.to_be_bytes());
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::UnknownNetwork { network_id: 9999 })
        ));
        Ok(())
    }

    #[test]
    fn wrong_network_is_rejected() -> Result<(), StreamCursorError> {
        let cursor = test_cursor()?;

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashMainnet, CURSOR_AUTH_KEY),
            Err(StreamCursorError::NetworkMismatch {
                expected: Network::ZcashMainnet,
                actual: Network::ZcashRegtest
            })
        ));
        Ok(())
    }

    #[test]
    fn wrong_stream_family_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        cursor_bytes[49] = 2;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::StreamFamilyMismatch { flags: 2 })
        ));
        Ok(())
    }

    #[test]
    fn invalid_auth_tag_is_rejected() -> Result<(), StreamCursorError> {
        let mut cursor_bytes = test_cursor()?.as_bytes().to_vec();
        assert!(!cursor_bytes.is_empty());
        let last_index = cursor_bytes.len() - 1;
        let last_byte = &mut cursor_bytes[last_index];
        *last_byte ^= 1;
        let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            cursor.decode_chain_event(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidAuthTag)
        ));
        Ok(())
    }

    fn test_cursor() -> Result<StreamCursorTokenV1, StreamCursorError> {
        let locator = ChainEventLocator::new(vec![ChainEventCursorAnchor {
            height: BlockHeight::new(7),
            hash: BlockHash::from_bytes([9; 32]),
        }])?;
        StreamCursorTokenV1::chain_event(
            Network::ZcashRegtest,
            ChainEventStreamFamily::Tip,
            42,
            &locator,
            CURSOR_AUTH_KEY,
        )
    }

    #[test]
    fn address_output_cursor_round_trips() -> Result<(), StreamCursorError> {
        let last_outpoint = TransparentOutPoint {
            transaction_id: TransactionId::from_bytes([5; 32]),
            output_index: 11,
        };
        let cursor = StreamCursorTokenV1::address_output(
            Network::ZcashRegtest,
            AddressOutputStreamFamily::AddressOutput,
            BlockHeight::new(2024),
            last_outpoint,
            CURSOR_AUTH_KEY,
        )?;

        assert_eq!(cursor.as_bytes().len(), STREAM_CURSOR_TOKEN_V1_LEN);
        assert_eq!(
            cursor.as_bytes()[49],
            AddressOutputStreamFamily::AddressOutput.flags()
        );

        let decoded = cursor.decode_address_output(Network::ZcashRegtest, CURSOR_AUTH_KEY)?;

        assert_eq!(
            decoded,
            AddressOutputCursorPayload {
                family: AddressOutputStreamFamily::AddressOutput,
                last_block_height: BlockHeight::new(2024),
                last_outpoint,
            }
        );

        Ok(())
    }

    #[test]
    fn address_output_cursor_rejects_chain_event_cursor() -> Result<(), StreamCursorError> {
        // A chain-event cursor carries a variable-length locator body, so the
        // fixed-length address-output decoder rejects it on length before it
        // ever inspects the family nibble.
        let chain_event_cursor = test_cursor()?;
        assert!(matches!(
            chain_event_cursor.decode_address_output(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidLength { .. })
        ));
        Ok(())
    }

    #[test]
    fn address_output_cursor_rejects_wrong_network() -> Result<(), StreamCursorError> {
        let cursor = StreamCursorTokenV1::address_output(
            Network::ZcashRegtest,
            AddressOutputStreamFamily::AddressOutput,
            BlockHeight::new(1),
            TransparentOutPoint {
                transaction_id: TransactionId::from_bytes([0; 32]),
                output_index: 0,
            },
            CURSOR_AUTH_KEY,
        )?;
        assert!(matches!(
            cursor.decode_address_output(Network::ZcashMainnet, CURSOR_AUTH_KEY),
            Err(StreamCursorError::NetworkMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn snapshot_page_cursor_round_trips() -> Result<(), StreamCursorError> {
        let after_transaction_id = TransactionId::from_bytes([3; 32]);
        let cursor = StreamCursorTokenV1::snapshot_page(
            Network::ZcashRegtest,
            SnapshotPageStreamFamily::SnapshotPage,
            17,
            after_transaction_id,
            CURSOR_AUTH_KEY,
        )?;

        assert_eq!(cursor.as_bytes().len(), STREAM_CURSOR_TOKEN_V1_LEN);
        assert_eq!(
            cursor.as_bytes()[49],
            SnapshotPageStreamFamily::SnapshotPage.flags()
        );

        let decoded = cursor.decode_snapshot_page(Network::ZcashRegtest, CURSOR_AUTH_KEY)?;
        assert_eq!(
            decoded,
            SnapshotPageCursorPayload {
                family: SnapshotPageStreamFamily::SnapshotPage,
                snapshot_sequence: 17,
                after_transaction_id,
            }
        );

        Ok(())
    }

    #[test]
    fn tampered_snapshot_page_cursor_fails_auth() -> Result<(), StreamCursorError> {
        let cursor = StreamCursorTokenV1::snapshot_page(
            Network::ZcashRegtest,
            SnapshotPageStreamFamily::SnapshotPage,
            17,
            TransactionId::from_bytes([3; 32]),
            CURSOR_AUTH_KEY,
        )?;
        let mut cursor_bytes = cursor.as_bytes().to_vec();
        // Flip a byte inside the transaction-id field to alter the resume
        // position; the HMAC must reject it instead of serving the wrong page.
        cursor_bytes[20] ^= 1;
        let tampered = StreamCursorTokenV1::from_bytes(cursor_bytes);

        assert!(matches!(
            tampered.decode_snapshot_page(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::InvalidAuthTag)
        ));

        Ok(())
    }

    #[test]
    fn snapshot_page_cursor_rejects_wrong_family() -> Result<(), StreamCursorError> {
        let cursor = StreamCursorTokenV1::address_output(
            Network::ZcashRegtest,
            AddressOutputStreamFamily::AddressOutput,
            BlockHeight::new(1),
            TransparentOutPoint {
                transaction_id: TransactionId::from_bytes([0; 32]),
                output_index: 0,
            },
            CURSOR_AUTH_KEY,
        )?;
        assert!(matches!(
            cursor.decode_snapshot_page(Network::ZcashRegtest, CURSOR_AUTH_KEY),
            Err(StreamCursorError::StreamFamilyMismatch { .. })
        ));
        Ok(())
    }
}
