//! Node-sourced block values.

use zebra_chain::{block::Block as ZebraBlock, serialization::ZcashDeserializeInto};
use zinder_core::{BlockHash, BlockHeight, Network};

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
    /// Parses source block metadata from raw serialized Zcash block bytes.
    pub fn from_raw_block_bytes(
        network: Network,
        height: BlockHeight,
        raw_block_bytes: &[u8],
    ) -> Result<Self, SourceError> {
        let parsed_block = parse_raw_block(raw_block_bytes)?;
        let parsed_height = parsed_block
            .coinbase_height()
            .ok_or(SourceError::RawBlockCoinbaseHeightMissing)?
            .0;
        if parsed_height != height.value() {
            return Err(SourceError::RawBlockHeightMismatch {
                parsed_height,
                source_height: height,
            });
        }

        let block_time_seconds = u32::try_from(parsed_block.header.time.timestamp())
            .map_err(|_| SourceError::RawBlockTimeOutOfRange)?;
        let hash = BlockHash::from_bytes(parsed_block.hash().0);
        let parent_hash = BlockHash::from_bytes(parsed_block.header.previous_block_hash.0);

        Ok(Self {
            network,
            height,
            hash,
            parent_hash,
            block_time_seconds,
        })
    }
}

fn parse_raw_block(raw_block_bytes: &[u8]) -> Result<ZebraBlock, SourceError> {
    raw_block_bytes
        .zcash_deserialize_into::<ZebraBlock>()
        .map_err(|source| SourceError::RawBlockParseFailed {
            reason: source.to_string(),
        })
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
    /// Optional tree-state payload bytes observed with this block.
    pub tree_state_payload_bytes: Option<Vec<u8>>,
}

impl SourceBlock {
    /// Creates a source block from raw serialized Zcash block bytes.
    pub fn from_raw_block_bytes(
        network: Network,
        height: BlockHeight,
        raw_block_bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, SourceError> {
        let raw_block_bytes = raw_block_bytes.into();
        let header = SourceBlockHeader::from_raw_block_bytes(network, height, &raw_block_bytes)?;
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
            tree_state_payload_bytes: None,
        }
    }

    /// Adds tree-state payload bytes observed with this block.
    #[must_use]
    pub fn with_tree_state_payload_bytes(
        mut self,
        tree_state_payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.tree_state_payload_bytes = Some(tree_state_payload_bytes.into());
        self
    }
}

/// Decodes an RPC display-order block hash into canonical little-endian bytes.
pub fn decode_display_block_hash(display_hash: &str) -> Result<BlockHash, SourceError> {
    decode_display_hash_32(
        display_hash,
        |source| SourceError::InvalidBlockHashHex { source },
        |byte_count| SourceError::InvalidBlockHashLength { byte_count },
    )
    .map(BlockHash::from_bytes)
}

/// Encodes a canonical little-endian block hash as RPC display-order hex.
#[must_use]
pub fn encode_display_block_hash(hash: BlockHash) -> String {
    let mut hash_bytes = hash.as_bytes();
    hash_bytes.reverse();
    hex::encode(hash_bytes)
}

pub(crate) fn decode_display_hash_32(
    display_hash: &str,
    invalid_hex: impl FnOnce(hex::FromHexError) -> SourceError,
    invalid_length: impl FnOnce(usize) -> SourceError,
) -> Result<[u8; 32], SourceError> {
    if display_hash.len() != 64 && display_hash.len().is_multiple_of(2) {
        return Err(invalid_length(display_hash.len() / 2));
    }

    let mut hash_bytes = [0; 32];
    hex::decode_to_slice(display_hash, &mut hash_bytes).map_err(invalid_hex)?;
    hash_bytes.reverse();
    Ok(hash_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    use eyre::eyre;
    use serde_json::Value;

    #[test]
    fn raw_block_parser_rejects_unparseable_block() {
        assert!(matches!(
            SourceBlockHeader::from_raw_block_bytes(
                Network::ZcashRegtest,
                BlockHeight::new(1),
                &[],
            ),
            Err(SourceError::RawBlockParseFailed { .. })
        ));
    }

    #[test]
    fn raw_block_parser_accepts_matching_coinbase_height() -> eyre::Result<()> {
        let raw_block_bytes = fixture_raw_block_bytes()?;

        let header = SourceBlockHeader::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(1),
            &raw_block_bytes,
        )?;

        assert_eq!(header.height, BlockHeight::new(1));

        Ok(())
    }

    #[test]
    fn raw_block_parser_rejects_coinbase_height_mismatch() -> eyre::Result<()> {
        let raw_block_bytes = fixture_raw_block_bytes()?;

        let error = match SourceBlockHeader::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(2),
            &raw_block_bytes,
        ) {
            Ok(header) => return Err(eyre!("expected height mismatch, got {header:?}")),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            SourceError::RawBlockHeightMismatch {
                parsed_height: 1,
                source_height,
            } if source_height == BlockHeight::new(2)
        ));

        Ok(())
    }

    #[test]
    fn display_hash_decoder_reverses_fixed_hash_without_allocation() -> Result<(), SourceError> {
        assert_eq!(
            decode_display_block_hash(
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
            decode_display_block_hash("0011"),
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
