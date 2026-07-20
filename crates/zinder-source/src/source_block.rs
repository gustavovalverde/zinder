//! Node-sourced block values.

use std::io::Cursor;

use zebra_chain::{
    block::{Header as ZebraBlockHeader, merkle::Root as ZebraMerkleRoot},
    serialization::ZcashDeserialize,
};
use zinder_core::{BlockHash, BlockHeader, BlockHeight, BlockId, Network};

use crate::SourceError;

/// Source block metadata observed before artifact construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceBlockHeader {
    /// Network the node reported this block on.
    pub network: Network,
    /// Source block height.
    pub height: BlockHeight,
    /// Source block hash in canonical little-endian byte order.
    pub hash: BlockHash,
    /// Parent block hash in canonical little-endian byte order.
    pub parent_hash: BlockHash,
    /// Block timestamp in Unix seconds.
    pub block_time_seconds: u32,
}

impl SourceBlockHeader {
    /// Parses only the serialized block header and uses the height supplied by
    /// the source request.
    ///
    /// Bulk catchup uses this path to validate parent links before canonical
    /// preparation. Canonical preparation still parses the complete block and
    /// validates the coinbase height against this source height before writing
    /// any artifact.
    fn from_raw_block_header(
        network: Network,
        height: BlockHeight,
        raw_block_bytes: &[u8],
    ) -> Result<Self, SourceError> {
        let header = block_header_from_raw_block_bytes(height, raw_block_bytes)?;
        let block_time_seconds =
            u32::try_from(header.block_time).map_err(|_| SourceError::RawBlockTimeOutOfRange)?;

        Ok(Self {
            network,
            height,
            hash: header.block_id.hash,
            parent_hash: header.previous_block_hash,
            block_time_seconds,
        })
    }
}

/// Parses a typed [`BlockHeader`] from raw serialized Zcash block
/// bytes.
///
/// Returns the block-header read-model shape consumed by the wallet data
/// plane. The typed `BlockHeader` is the public boundary; this
/// function is the one place where Zebra's block-header structure is
/// translated into Zinder vocabulary.
///
/// Only the header prefix is parsed: the full transaction list following
/// the header is not deserialized. Hot-path callers (`transaction()`
/// status lookups) therefore avoid the per-call cost of decoding every
/// transaction in the containing block.
pub fn block_header_from_raw_block_bytes(
    height: BlockHeight,
    raw_block_bytes: &[u8],
) -> Result<BlockHeader, SourceError> {
    let mut cursor = Cursor::new(raw_block_bytes);
    let header = ZebraBlockHeader::zcash_deserialize(&mut cursor).map_err(|source| {
        SourceError::RawBlockParseFailed {
            reason: source.to_string(),
        }
    })?;
    let block_id = BlockId::new(height, BlockHash::from_bytes(header.hash().0));
    let previous_block_hash = BlockHash::from_bytes(header.previous_block_hash.0);
    let ZebraMerkleRoot(merkle_root_hash) = header.merkle_root;
    let commitment_bytes = *header.commitment_bytes;
    let block_time = header.time.timestamp();
    let bits = u32::from_be_bytes(header.difficulty_threshold.bytes_in_display_order());
    let nonce = *header.nonce;
    let version = header.version;

    Ok(BlockHeader::new(
        block_id,
        previous_block_hash,
        merkle_root_hash,
        commitment_bytes,
        block_time,
        bits,
        nonce,
        version,
    ))
}

/// Block data observed from an upstream node before canonical artifact construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceBlock {
    /// Network the node reported this block on.
    pub network: Network,
    /// Source block height.
    pub height: BlockHeight,
    /// Source block hash in canonical little-endian byte order.
    pub hash: BlockHash,
    /// Parent block hash in canonical little-endian byte order.
    pub parent_hash: BlockHash,
    /// Block timestamp in Unix seconds.
    pub block_time_seconds: u32,
    /// Raw serialized block bytes returned by the node.
    pub raw_block_bytes: Vec<u8>,
}

impl SourceBlock {
    /// Creates a source block after parsing only the serialized header prefix.
    ///
    /// Source adapters need the header identity to validate chain links before
    /// canonical preparation. The canonical preparation boundary performs the
    /// one full-block parse and validates the coinbase height before any write.
    pub fn from_raw_block_bytes(
        network: Network,
        height: BlockHeight,
        raw_block_bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, SourceError> {
        let raw_block_bytes = raw_block_bytes.into();
        let header = SourceBlockHeader::from_raw_block_header(network, height, &raw_block_bytes)?;
        Ok(Self::new(header, raw_block_bytes))
    }

    /// Creates a node-sourced block value.
    #[must_use]
    pub fn new(header: SourceBlockHeader, raw_block_bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            network: header.network,
            height: header.height,
            hash: header.hash,
            parent_hash: header.parent_hash,
            block_time_seconds: header.block_time_seconds,
            raw_block_bytes: raw_block_bytes.into(),
        }
    }
}

/// Decodes an RPC-byte-order block hash hex string into canonical
/// internal-byte-order bytes.
///
/// Delegates to [`zinder_core::wire::decode_rpc_block_hash_hex`] for the
/// hex parsing and byte reversal; this function exists to map the wire error
/// to a typed [`SourceError`] variant the metrics layer aggregates on.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
pub fn decode_rpc_block_hash(rpc_hash: &str) -> Result<BlockHash, SourceError> {
    zinder_core::wire::decode_rpc_block_hash_hex(rpc_hash).map_err(wire_error_to_block_hash_error)
}

/// Encodes a canonical internal-byte-order block hash as RPC-byte-order hex.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
#[must_use]
pub fn encode_rpc_block_hash(hash: BlockHash) -> String {
    zinder_core::wire::encode_rpc_block_hash_hex(hash)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "WireDecodeError is #[non_exhaustive]; the catch-all maps future variants to a typed SourceError without losing the underlying message"
)]
fn wire_error_to_block_hash_error(wire_error: zinder_core::wire::WireDecodeError) -> SourceError {
    match wire_error {
        zinder_core::wire::WireDecodeError::InvalidLength { actual, .. } => {
            SourceError::InvalidBlockHashLength {
                byte_count: actual / 2,
            }
        }
        zinder_core::wire::WireDecodeError::InvalidHex { reason } => {
            SourceError::InvalidBlockHashHex { reason }
        }
        other => SourceError::InvalidBlockHashHex {
            reason: other.to_string(),
        },
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "WireDecodeError is #[non_exhaustive]; the catch-all maps future variants to a typed SourceError without losing the underlying message"
)]
pub(crate) fn wire_error_to_transaction_id_error(
    wire_error: zinder_core::wire::WireDecodeError,
) -> SourceError {
    match wire_error {
        zinder_core::wire::WireDecodeError::InvalidLength { actual, .. } => {
            SourceError::InvalidTransactionIdLength {
                byte_count: actual / 2,
            }
        }
        zinder_core::wire::WireDecodeError::InvalidHex { reason } => {
            SourceError::InvalidTransactionIdHex { reason }
        }
        other => SourceError::InvalidTransactionIdHex {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use eyre::eyre;
    use serde_json::Value;

    #[test]
    fn source_block_rejects_unparseable_header() {
        assert!(matches!(
            SourceBlock::from_raw_block_bytes(
                Network::ZcashRegtest,
                BlockHeight::new(1),
                Vec::new(),
            ),
            Err(SourceError::RawBlockParseFailed { .. })
        ));
    }

    #[test]
    fn source_block_accepts_a_valid_header_prefix() -> eyre::Result<()> {
        let raw_block_bytes = fixture_raw_block_bytes()?;

        let block = SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(1),
            raw_block_bytes,
        )?;

        assert_eq!(block.height, BlockHeight::new(1));

        Ok(())
    }

    #[test]
    fn source_block_defers_coinbase_height_validation_to_canonical_preparation() -> eyre::Result<()>
    {
        let raw_block_bytes = fixture_raw_block_bytes()?;

        let source_block = SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(2),
            raw_block_bytes,
        )?;

        assert_eq!(source_block.height, BlockHeight::new(2));

        Ok(())
    }

    #[test]
    fn display_hash_decoder_reverses_fixed_hash_without_allocation() -> Result<(), SourceError> {
        assert_eq!(
            decode_rpc_block_hash(
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            )?
            .as_bytes(),
            [
                0x1f, 0x1e, 0x1d, 0x1c, 0x1b, 0x1a, 0x19, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12,
                0x11, 0x10, 0x0f, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09, 0x08, 0x07, 0x06, 0x05, 0x04,
                0x03, 0x02, 0x01, 0x00,
            ]
        );

        Ok(())
    }

    #[test]
    fn display_hash_decoder_reports_even_length_mismatch() {
        assert!(matches!(
            decode_rpc_block_hash("0011"),
            Err(SourceError::InvalidBlockHashLength { byte_count: 2 })
        ));
    }

    fn fixture_raw_block_bytes() -> eyre::Result<Vec<u8>> {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../services/zinder-ingest/tests/fixtures/z3-regtest-block-1.json"
        ))?;
        let raw_block_hex = fixture
            .get("raw_block_hex")
            .and_then(Value::as_str)
            .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;

        hex::decode(raw_block_hex)
            .map_err(|error| eyre!("failed to decode fixture raw block hex: {error}"))
    }
}
