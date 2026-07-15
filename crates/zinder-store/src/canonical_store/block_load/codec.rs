use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, TransactionLocation,
    wire::encode_height_key_ascending,
};

pub(super) const BLOCK_HEADER_VALUE_LEN: usize = 184;
pub(super) const BLOCK_HASH_INDEX_RECORD_LEN: usize = 36;
pub(super) const TRANSACTION_LOCATION_RECORD_LEN: usize = 72;

pub(super) fn encode_block_header(header: &BlockHeaderArtifact) -> [u8; BLOCK_HEADER_VALUE_LEN] {
    let mut encoded_header = [0_u8; BLOCK_HEADER_VALUE_LEN];
    encoded_header[..32].copy_from_slice(&header.block_hash.as_bytes());
    encoded_header[32..64].copy_from_slice(&header.parent_hash.as_bytes());
    encoded_header[64..96].copy_from_slice(&header.merkle_root_hash);
    encoded_header[96..128].copy_from_slice(&header.commitment_bytes);
    encoded_header[128..136].copy_from_slice(&header.block_time.to_le_bytes());
    encoded_header[136..140].copy_from_slice(&header.bits.to_le_bytes());
    encoded_header[140..172].copy_from_slice(&header.nonce);
    encoded_header[172..176].copy_from_slice(&header.version.to_le_bytes());
    encoded_header[176..184].copy_from_slice(&header.block_size_bytes.to_le_bytes());
    encoded_header
}

pub(super) fn encode_transaction_position(height: BlockHeight, transaction_index: u32) -> [u8; 8] {
    let mut key = [0_u8; 8];
    key[..4].copy_from_slice(&encode_height_key_ascending(height));
    key[4..].copy_from_slice(&transaction_index.to_be_bytes());
    key
}

pub(super) fn encode_block_position(height: BlockHeight) -> [u8; 4] {
    encode_height_key_ascending(height)
}

pub(super) fn encode_block_hash_location(block_hash: BlockHash, height: BlockHeight) -> [u8; 36] {
    let mut row = [0_u8; BLOCK_HASH_INDEX_RECORD_LEN];
    row[..32].copy_from_slice(&block_hash.as_bytes());
    row[32..].copy_from_slice(&encode_block_position(height));
    row
}

pub(super) fn encode_transaction_location(location: TransactionLocation) -> [u8; 72] {
    let mut row = [0_u8; TRANSACTION_LOCATION_RECORD_LEN];
    row[..32].copy_from_slice(&location.transaction_id.as_bytes());
    row[32..36].copy_from_slice(&encode_block_position(location.block_height));
    row[36..68].copy_from_slice(&location.block_hash.as_bytes());
    row[68..].copy_from_slice(&location.tx_index_in_block.to_be_bytes());
    row
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHash, BlockHeaderArtifact, BlockHeight};

    use super::*;

    #[test]
    fn header_v1_encoding_has_known_field_offsets() {
        let encoded = encode_block_header(&BlockHeaderArtifact::new(
            BlockHeight::new(7),
            BlockHash::from_bytes([1; 32]),
            BlockHash::from_bytes([2; 32]),
            [3; 32],
            [4; 32],
            0x0102_0304_0506_0708,
            0x1112_1314,
            [5; 32],
            0x2122_2324,
            0x3132_3334_3536_3738,
        ));

        assert_eq!(&encoded[..32], &[1; 32]);
        assert_eq!(&encoded[32..64], &[2; 32]);
        assert_eq!(&encoded[64..96], &[3; 32]);
        assert_eq!(&encoded[96..128], &[4; 32]);
        assert_eq!(&encoded[128..136], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&encoded[136..140], &[0x14, 0x13, 0x12, 0x11]);
        assert_eq!(&encoded[140..172], &[5; 32]);
        assert_eq!(&encoded[172..176], &[0x24, 0x23, 0x22, 0x21]);
        assert_eq!(
            &encoded[176..184],
            &[0x38, 0x37, 0x36, 0x35, 0x34, 0x33, 0x32, 0x31]
        );
    }

    #[test]
    fn transaction_position_v1_encoding_sorts_by_height_then_index() {
        assert_eq!(
            encode_transaction_position(BlockHeight::new(0x0102_0304), 0x1112_1314),
            [1, 2, 3, 4, 0x11, 0x12, 0x13, 0x14]
        );
    }

    #[test]
    fn direct_index_and_height_rows_have_known_v1_bytes() {
        let height = BlockHeight::new(0x0102_0304);
        let block_hash = BlockHash::from_bytes([7; 32]);
        let location = zinder_core::TransactionLocation::new(
            zinder_core::TransactionId::from_bytes([8; 32]),
            height,
            block_hash,
            0x1112_1314,
        );

        assert_eq!(encode_block_position(height), [1, 2, 3, 4]);
        let block_hash_location = encode_block_hash_location(block_hash, height);
        assert_eq!(&block_hash_location[..32], &[7; 32]);
        assert_eq!(&block_hash_location[32..], &[1, 2, 3, 4]);
        let encoded_location = encode_transaction_location(location);
        assert_eq!(&encoded_location[..32], &[8; 32]);
        assert_eq!(&encoded_location[32..36], &[1, 2, 3, 4]);
        assert_eq!(&encoded_location[36..68], &[7; 32]);
        assert_eq!(&encoded_location[68..], &[0x11, 0x12, 0x13, 0x14]);
    }
}
