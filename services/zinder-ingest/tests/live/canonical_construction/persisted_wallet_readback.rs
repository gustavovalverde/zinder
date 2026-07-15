use std::path::Path;

use eyre::{Result, eyre};
use prost::Message;
use rust_rocksdb::{DB, LiveFile, Options, ReadOptions};
use zinder_core::{
    BlockHeaderArtifact, BlockHeight, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, SerializedBytesDigest, decode_canonical_block_replay,
    wire::{decode_height_key_ascending, encode_height_key_ascending},
};
use zinder_proto::compat::lightwalletd::CompactBlock;
use zinder_store::CanonicalBlockLoadEvidence;

const READBACK_READAHEAD_BYTES: usize = 2 * 1_024 * 1_024;
const BLOCK_HEADER_COLUMN_FAMILY: &str = "block_header";
const BLOCK_HASH_INDEX_COLUMN_FAMILY: &str = "block_hash_index";
const BLOCK_REPLAY_COLUMN_FAMILY: &str = "block_replay";
const COMPACT_BLOCK_COLUMN_FAMILY: &str = "compact_block";
const TRANSACTION_LOCATION_COLUMN_FAMILY: &str = "transaction_location";
const TRANSACTION_BLOB_COLUMN_FAMILY: &str = "transaction_blob";
const BLOCK_BLOB_COLUMN_FAMILY: &str = "block_blob";

#[derive(Clone, Copy, Debug)]
pub(super) struct PersistedFamilyEvidence {
    pub(super) row_count: u64,
    pub(super) logical_bytes: u64,
    pub(super) sst_file_bytes: u64,
    pub(super) sst_file_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PersistedCanonicalEvidence {
    pub(super) block_header: PersistedFamilyEvidence,
    pub(super) block_hash_index: PersistedFamilyEvidence,
    pub(super) block_replay: PersistedFamilyEvidence,
    pub(super) compact_block: PersistedFamilyEvidence,
    pub(super) transaction_location: PersistedFamilyEvidence,
    pub(super) transaction_blob: PersistedFamilyEvidence,
    pub(super) block_blob: PersistedFamilyEvidence,
}

impl PersistedCanonicalEvidence {
    fn families(self) -> [PersistedFamilyEvidence; 7] {
        [
            self.block_header,
            self.block_hash_index,
            self.block_replay,
            self.compact_block,
            self.transaction_location,
            self.transaction_blob,
            self.block_blob,
        ]
    }

    fn logical_bytes(self) -> u64 {
        self.families()
            .into_iter()
            .map(|family| family.logical_bytes)
            .sum()
    }

    fn sst_file_bytes(self) -> u64 {
        self.families()
            .into_iter()
            .map(|family| family.sst_file_bytes)
            .sum()
    }

    fn sst_file_count(self) -> u64 {
        self.families()
            .into_iter()
            .map(|family| family.sst_file_count)
            .sum()
    }
}

pub(super) fn validate_persisted_wallet_families(
    store_path: &Path,
    expected: CanonicalBlockLoadEvidence,
) -> Result<PersistedCanonicalEvidence> {
    let column_families = DB::list_cf(&Options::default(), store_path)?;
    let database =
        DB::open_cf_for_read_only(&Options::default(), store_path, &column_families, false)?;
    let live_files = database.live_files()?;
    let persisted = PersistedCanonicalEvidence {
        block_header: inspect_family(&database, &live_files, BLOCK_HEADER_COLUMN_FAMILY)?,
        block_hash_index: inspect_family(&database, &live_files, BLOCK_HASH_INDEX_COLUMN_FAMILY)?,
        block_replay: inspect_family(&database, &live_files, BLOCK_REPLAY_COLUMN_FAMILY)?,
        compact_block: inspect_family(&database, &live_files, COMPACT_BLOCK_COLUMN_FAMILY)?,
        transaction_location: inspect_family(
            &database,
            &live_files,
            TRANSACTION_LOCATION_COLUMN_FAMILY,
        )?,
        transaction_blob: inspect_family(&database, &live_files, TRANSACTION_BLOB_COLUMN_FAMILY)?,
        block_blob: inspect_family(&database, &live_files, BLOCK_BLOB_COLUMN_FAMILY)?,
    };

    assert_eq!(
        persisted.block_header.row_count,
        expected.block_header_count
    );
    assert_eq!(
        persisted.block_hash_index.row_count,
        expected.block_hash_index_count
    );
    assert_eq!(
        persisted.block_replay.row_count,
        expected.block_replay_count
    );
    assert_eq!(
        persisted.compact_block.row_count,
        expected.compact_block_count
    );
    assert_eq!(
        persisted.transaction_location.row_count,
        expected.transaction_location_count
    );
    assert_eq!(
        persisted.transaction_blob.row_count,
        expected.transaction_blob_count
    );
    assert_eq!(persisted.block_blob.row_count, expected.block_blob_count);
    assert_eq!(persisted.logical_bytes(), expected.logical_bytes);
    assert_eq!(persisted.sst_file_bytes(), expected.sst_file_bytes);
    assert_eq!(persisted.sst_file_count(), expected.sst_file_count);

    validate_replay_sequence(&database, expected)?;
    for height in sample_heights(expected) {
        validate_sampled_block_families(&database, height, expected)?;
    }
    Ok(persisted)
}

fn inspect_family(
    database: &DB,
    live_files: &[LiveFile],
    family_name: &'static str,
) -> Result<PersistedFamilyEvidence> {
    let column_family = database
        .cf_handle(family_name)
        .ok_or_else(|| eyre!("fresh canonical store is missing {family_name}"))?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    read_options.set_readahead_size(READBACK_READAHEAD_BYTES);
    let mut iterator = database.raw_iterator_cf_opt(&column_family, read_options);
    iterator.seek_to_first();
    let mut row_count = 0_u64;
    let mut logical_bytes = 0_u64;
    while iterator.valid() {
        let (key, row_value) = iterator
            .item()
            .ok_or_else(|| eyre!("{family_name} iterator lost its current row"))?;
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| eyre!("{family_name} row count exceeds u64::MAX"))?;
        let row_bytes = u64::try_from(key.len().saturating_add(row_value.len()))
            .map_err(|_| eyre!("{family_name} row bytes exceed u64::MAX"))?;
        logical_bytes = logical_bytes
            .checked_add(row_bytes)
            .ok_or_else(|| eyre!("{family_name} logical bytes exceed u64::MAX"))?;
        iterator.next();
    }
    iterator.status()?;

    let mut sst_file_bytes = 0_u64;
    let mut sst_file_count = 0_u64;
    for file in live_files
        .iter()
        .filter(|file| file.column_family_name == family_name)
    {
        sst_file_count = sst_file_count
            .checked_add(1)
            .ok_or_else(|| eyre!("{family_name} SST count exceeds u64::MAX"))?;
        let file_bytes = u64::try_from(file.size)
            .map_err(|_| eyre!("{family_name} SST bytes exceed u64::MAX"))?;
        sst_file_bytes = sst_file_bytes
            .checked_add(file_bytes)
            .ok_or_else(|| eyre!("{family_name} SST bytes exceed u64::MAX"))?;
    }
    Ok(PersistedFamilyEvidence {
        row_count,
        logical_bytes,
        sst_file_bytes,
        sst_file_count,
    })
}

fn validate_replay_sequence(database: &DB, expected: CanonicalBlockLoadEvidence) -> Result<()> {
    let column_family = database
        .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
        .ok_or_else(|| eyre!("fresh canonical store is missing block_replay"))?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    read_options.set_readahead_size(READBACK_READAHEAD_BYTES);
    let mut iterator = database.raw_iterator_cf_opt(&column_family, read_options);
    iterator.seek_to_first();
    let mut expected_height = expected.first_height;
    let mut expected_parent_hash = expected.first_parent_hash;
    let mut observed_transaction_count = 0_u64;
    let mut sequence =
        CanonicalBlockFactsSequenceDigestBuilder::new(CanonicalBlockFactsSequenceDigestVersion::V1);
    let mut observed_tip_hash = None;
    while iterator.valid() {
        let (key, replay_bytes) = iterator
            .item()
            .ok_or_else(|| eyre!("block_replay iterator lost its current row"))?;
        let height = decode_height_key_ascending(key)?;
        assert_eq!(height, expected_height);
        let replay = decode_canonical_block_replay(replay_bytes)?;
        let facts = replay.facts();
        assert_eq!(facts.block_header.height, height);
        assert_eq!(facts.block_header.parent_hash, expected_parent_hash);
        sequence.try_append(replay.reference_digest())?;
        observed_transaction_count = observed_transaction_count
            .checked_add(u64::try_from(facts.transactions.len())?)
            .ok_or_else(|| eyre!("persisted transaction count exceeds u64::MAX"))?;
        observed_tip_hash = Some(facts.block_header.block_hash);
        expected_parent_hash = facts.block_header.block_hash;
        expected_height = BlockHeight::new(
            expected_height
                .value()
                .checked_add(1)
                .ok_or_else(|| eyre!("persisted block height exceeds u32::MAX"))?,
        );
        iterator.next();
    }
    iterator.status()?;
    assert_eq!(observed_transaction_count, expected.transaction_count);
    assert_eq!(observed_tip_hash, Some(expected.tip_hash));
    assert_eq!(sequence.finish(), expected.sequence_digest);
    Ok(())
}

fn sample_heights(expected: CanonicalBlockLoadEvidence) -> Vec<BlockHeight> {
    let midpoint = BlockHeight::new(
        expected
            .first_height
            .value()
            .saturating_add(u32::try_from(expected.block_count / 2).unwrap_or(u32::MAX)),
    );
    let mut heights = vec![expected.first_height, midpoint, expected.tip_height];
    heights.sort_unstable();
    heights.dedup();
    heights
}

fn validate_sampled_block_families(
    database: &DB,
    height: BlockHeight,
    expected: CanonicalBlockLoadEvidence,
) -> Result<()> {
    let height_key = encode_height_key_ascending(height);
    let replay_bytes = read_uncached(database, BLOCK_REPLAY_COLUMN_FAMILY, &height_key)?;
    let replay = decode_canonical_block_replay(&replay_bytes)?;
    let facts = replay.facts();
    let header = &facts.block_header;
    assert_eq!(header.height, height);
    if height == expected.first_height {
        assert_eq!(header.block_hash, expected.first_hash);
        assert_eq!(header.parent_hash, expected.first_parent_hash);
    }
    if height == expected.tip_height {
        assert_eq!(header.block_hash, expected.tip_hash);
    }

    let persisted_header = read_uncached(database, BLOCK_HEADER_COLUMN_FAMILY, &height_key)?;
    assert_eq!(persisted_header, encode_block_header_v1(header));
    let persisted_height = read_uncached(
        database,
        BLOCK_HASH_INDEX_COLUMN_FAMILY,
        &header.block_hash.as_bytes(),
    )?;
    assert_eq!(persisted_height, height_key);

    let compact_bytes = read_uncached(database, COMPACT_BLOCK_COLUMN_FAMILY, &height_key)?;
    let compact = CompactBlock::decode(compact_bytes.as_slice())?;
    assert_eq!(compact.height, u64::from(height.value()));
    assert_eq!(compact.hash.as_slice(), header.block_hash.as_bytes());
    assert_eq!(compact.prev_hash.as_slice(), header.parent_hash.as_bytes());
    let compact_metadata = compact.chain_metadata.ok_or_else(|| {
        eyre!(
            "sampled compact block {} has no chain metadata",
            height.value()
        )
    })?;
    if height == expected.tip_height {
        assert_eq!(
            compact_metadata.sapling_commitment_tree_size,
            expected.tip_metadata.sapling_commitment_tree_size
        );
        assert_eq!(
            compact_metadata.orchard_commitment_tree_size,
            expected.tip_metadata.orchard_commitment_tree_size
        );
        assert_eq!(
            compact_metadata.ironwood_commitment_tree_size,
            expected.tip_metadata.ironwood_commitment_tree_size
        );
    }

    let last_transaction_index = facts.transactions.len().saturating_sub(1);
    for transaction_index in [0, last_transaction_index] {
        let transaction = facts
            .transactions
            .get(transaction_index)
            .ok_or_else(|| eyre!("sampled block {} has no transactions", height.value()))?;
        let transaction_index = u32::try_from(transaction_index)?;
        let transaction_id = transaction.public_facts.transaction_id;
        let persisted_location = read_uncached(
            database,
            TRANSACTION_LOCATION_COLUMN_FAMILY,
            &transaction_id.as_bytes(),
        )?;
        assert_eq!(
            persisted_location,
            encode_transaction_location_v1(
                height,
                &header.block_hash.as_bytes(),
                transaction_index,
            )
        );
        let transaction_key = encode_transaction_position_v1(height, transaction_index);
        let transaction_bytes =
            read_uncached(database, TRANSACTION_BLOB_COLUMN_FAMILY, &transaction_key)?;
        assert_eq!(
            SerializedBytesDigest::from_serialized_bytes(&transaction_bytes),
            transaction.serialized_bytes_digest
        );
    }
    Ok(())
}

fn read_uncached(database: &DB, family_name: &'static str, key: &[u8]) -> Result<Vec<u8>> {
    let column_family = database
        .cf_handle(family_name)
        .ok_or_else(|| eyre!("fresh canonical store is missing {family_name}"))?;
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(false);
    database
        .get_cf_opt(&column_family, key, &read_options)?
        .ok_or_else(|| eyre!("{family_name} is missing sampled key {}", hex::encode(key)))
}

fn encode_block_header_v1(header: &BlockHeaderArtifact) -> [u8; 184] {
    let mut encoded = [0_u8; 184];
    encoded[..32].copy_from_slice(&header.block_hash.as_bytes());
    encoded[32..64].copy_from_slice(&header.parent_hash.as_bytes());
    encoded[64..96].copy_from_slice(&header.merkle_root_hash);
    encoded[96..128].copy_from_slice(&header.commitment_bytes);
    encoded[128..136].copy_from_slice(&header.block_time.to_le_bytes());
    encoded[136..140].copy_from_slice(&header.bits.to_le_bytes());
    encoded[140..172].copy_from_slice(&header.nonce);
    encoded[172..176].copy_from_slice(&header.version.to_le_bytes());
    encoded[176..184].copy_from_slice(&header.block_size_bytes.to_le_bytes());
    encoded
}

fn encode_transaction_location_v1(
    height: BlockHeight,
    block_hash: &[u8; 32],
    transaction_index: u32,
) -> [u8; 40] {
    let mut encoded = [0_u8; 40];
    encoded[..4].copy_from_slice(&encode_height_key_ascending(height));
    encoded[4..36].copy_from_slice(block_hash);
    encoded[36..].copy_from_slice(&transaction_index.to_be_bytes());
    encoded
}

fn encode_transaction_position_v1(height: BlockHeight, transaction_index: u32) -> [u8; 8] {
    let mut encoded = [0_u8; 8];
    encoded[..4].copy_from_slice(&encode_height_key_ascending(height));
    encoded[4..].copy_from_slice(&transaction_index.to_be_bytes());
    encoded
}
