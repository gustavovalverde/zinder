//! Store key byte contract.

use std::mem::size_of;

use zinder_core::{
    BlockHash, BlockHeight, BlockId, ChainEpochId, FinalNoteCommitmentRoot, Network,
    ShieldedProtocol, SubtreeRootIndex, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint,
};

/// Ordered key bytes used inside `RocksDB` column families.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StoreKey(Vec<u8>);

const KEY_VERSION: u8 = 1;
const BLOCK_HEADER_KEY_KIND: u8 = 1;
const COMPACT_BLOCK_KEY_KIND: u8 = 2;
const TRANSACTION_FACTS_KEY_KIND: u8 = 3;
const TREE_STATE_KEY_KIND: u8 = 4;
const SUBTREE_ROOT_KEY_KIND: u8 = 5;
const ADDRESS_OUTPUT_INDEX_KEY_KIND: u8 = 6;
const TRANSPARENT_SPEND_FACT_KEY_KIND: u8 = 7;
const MEMPOOL_EVENT_KEY_KIND: u8 = 8;
const BLOCK_HASH_INDEX_KEY_KIND: u8 = 10;
const TRANSPARENT_OUTPUT_KEY_KIND: u8 = 11;
const TRANSPARENT_OUTPUT_BLOCK_INDEX_KEY_KIND: u8 = 13;
const BLOCK_BLOB_KEY_KIND: u8 = 14;
const BLOCK_TRANSACTION_INDEX_KEY_KIND: u8 = 15;
const TRANSACTION_LOCATION_KEY_KIND: u8 = 16;
const TRANSACTION_BLOB_KEY_KIND: u8 = 17;
const TRANSPARENT_SPEND_FACT_BLOCK_INDEX_KEY_KIND: u8 = 19;
const FINAL_NOTE_COMMITMENT_ROOTS_KEY_KIND: u8 = 20;
const TRANSACTION_INTRINSIC_VALUE_BALANCES_KEY_KIND: u8 = 21;
const BLOCK_VALUE_POOL_BALANCES_KEY_KIND: u8 = 22;
const DISPLACED_BLOCK_BY_ORDER_KEY_KIND: u8 = 23;
const DISPLACED_BLOCK_BY_HASH_KEY_KIND: u8 = 24;
const DISPLACED_ROOT_INDEX_KEY_KIND: u8 = 25;
// Key kinds 26..=32 are reserved for future artifact families; visibility keys start at 33.
const VISIBLE_BLOCK_EPOCH_KEY_KIND: u8 = 33;
const VISIBLE_COMPACT_BLOCK_EPOCH_KEY_KIND: u8 = 34;
const VISIBLE_TREE_STATE_EPOCH_KEY_KIND: u8 = 35;
const VISIBLE_TRANSACTION_EPOCH_KEY_KIND: u8 = 36;
const VISIBLE_SUBTREE_ROOT_EPOCH_KEY_KIND: u8 = 37;
const VISIBLE_FINAL_NOTE_COMMITMENT_ROOTS_EPOCH_KEY_KIND: u8 = 38;
const VISIBLE_BLOCK_VALUE_POOL_BALANCES_EPOCH_KEY_KIND: u8 = 39;

const STORE_KEY_HEADER_LEN: usize = 2;
const NETWORK_ID_LEN: usize = 4;
const BLOCK_HEIGHT_LEN: usize = 4;
const TRANSACTION_ID_LEN: usize = 32;
const CHAIN_EPOCH_ID_LEN: usize = 8;
const SHIELDED_PROTOCOL_LEN: usize = 1;
const SUBTREE_ROOT_INDEX_LEN: usize = 4;
const FINAL_NOTE_COMMITMENT_ROOT_LEN: usize = 32;
const DISPLACEMENT_EVENT_SEQUENCE_LEN: usize = 8;
const BLOCK_HASH_LEN: usize = 32;
const DISPLACED_ROOT_INDEX_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + FINAL_NOTE_COMMITMENT_ROOT_LEN + SHIELDED_PROTOCOL_LEN;
const DISPLACED_ROOT_INDEX_KEY_LEN: usize = DISPLACED_ROOT_INDEX_PREFIX_LEN
    + DISPLACEMENT_EVENT_SEQUENCE_LEN
    + BLOCK_HEIGHT_LEN
    + BLOCK_HASH_LEN;
const HEIGHT_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + BLOCK_HEIGHT_LEN;
const TRANSACTION_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + TRANSACTION_ID_LEN;
const SUBTREE_ROOT_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + SHIELDED_PROTOCOL_LEN + SUBTREE_ROOT_INDEX_LEN;

impl StoreKey {
    /// Wraps an iterator-supplied key slice in a [`StoreKey`].
    ///
    /// Used by scan visitors that need to reattach the raw key bytes for
    /// error reporting; never used to construct keys for writes.
    pub(crate) fn from_raw_bytes(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    pub(crate) fn visible_chain_epoch_pointer() -> Self {
        Self(vec![KEY_VERSION, 1])
    }

    pub(crate) fn chain_event_sequence_pointer() -> Self {
        Self(vec![KEY_VERSION, 8])
    }

    pub(crate) fn cursor_auth_key() -> Self {
        Self(vec![KEY_VERSION, 9])
    }

    pub(crate) fn oldest_retained_chain_event_sequence() -> Self {
        Self(vec![KEY_VERSION, 10])
    }

    pub(crate) fn store_metadata() -> Self {
        Self(vec![KEY_VERSION, 12])
    }

    pub(crate) fn mempool_event_sequence_pointer() -> Self {
        Self(vec![KEY_VERSION, 13])
    }

    pub(crate) fn oldest_retained_mempool_event_sequence() -> Self {
        Self(vec![KEY_VERSION, 14])
    }

    pub(crate) fn transparent_retention_swept_height() -> Self {
        Self(vec![KEY_VERSION, 15])
    }

    pub(crate) fn raw_blob_retention() -> Self {
        Self(vec![KEY_VERSION, 16])
    }

    pub(crate) fn transparent_retention_release_height() -> Self {
        Self(vec![KEY_VERSION, 17])
    }

    pub(crate) fn transparent_retention_deleted_through_height() -> Self {
        Self(vec![KEY_VERSION, 18])
    }

    pub(crate) fn displaced_block_archive_coverage() -> Self {
        Self(vec![KEY_VERSION, 19])
    }

    pub(crate) fn displaced_block_count() -> Self {
        Self(vec![KEY_VERSION, 20])
    }

    pub(crate) fn displaced_root_archive_coverage() -> Self {
        Self(vec![KEY_VERSION, 21])
    }

    pub(crate) fn chain_epoch(chain_epoch: ChainEpochId) -> Self {
        let mut key = vec![KEY_VERSION];
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn block_header(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(BLOCK_HEADER_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn block_blob(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(BLOCK_BLOB_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn compact_block(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(COMPACT_BLOCK_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn block_transaction_index(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
        tx_index_in_block: u32,
    ) -> Self {
        let mut key = Self::block_transaction_index_prefix(network, chain_epoch, height).0;
        key.extend_from_slice(&tx_index_in_block.to_be_bytes());
        Self(key)
    }

    pub(crate) fn block_transaction_index_prefix(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(BLOCK_TRANSACTION_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.extend_from_slice(&height.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transaction_location(
        network: Network,
        chain_epoch: ChainEpochId,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSACTION_LOCATION_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.extend_from_slice(&transaction_id.as_bytes());
        Self(key)
    }

    pub(crate) fn transaction_facts(
        network: Network,
        chain_epoch: ChainEpochId,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSACTION_FACTS_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.extend_from_slice(&transaction_id.as_bytes());
        Self(key)
    }

    pub(crate) fn transaction_blob(
        network: Network,
        chain_epoch: ChainEpochId,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSACTION_BLOB_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.extend_from_slice(&transaction_id.as_bytes());
        Self(key)
    }

    pub(crate) fn transaction_intrinsic_value_balances(
        network: Network,
        chain_epoch: ChainEpochId,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSACTION_INTRINSIC_VALUE_BALANCES_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.extend_from_slice(&transaction_id.as_bytes());
        Self(key)
    }

    pub(crate) fn tree_state(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(TREE_STATE_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn final_note_commitment_roots(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(FINAL_NOTE_COMMITMENT_ROOTS_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn block_value_pool_balances(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(BLOCK_VALUE_POOL_BALANCES_KEY_KIND);
        push_network_epoch_height(&mut key, network, chain_epoch, height);
        Self(key)
    }

    pub(crate) fn tree_state_network_prefix(network: Network) -> Self {
        let mut key = artifact_key_prefix(TREE_STATE_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        Self(key)
    }

    pub(crate) fn tree_state_key_parts(key_bytes: &[u8]) -> Option<(ChainEpochId, BlockHeight)> {
        let prefix_len = STORE_KEY_HEADER_LEN + NETWORK_ID_LEN;
        let expected_len = prefix_len + CHAIN_EPOCH_ID_LEN + BLOCK_HEIGHT_LEN;
        if key_bytes.len() != expected_len
            || key_bytes.first().copied() != Some(KEY_VERSION)
            || key_bytes.get(1).copied() != Some(TREE_STATE_KEY_KIND)
        {
            return None;
        }
        let epoch_start = prefix_len;
        let height_start = epoch_start + CHAIN_EPOCH_ID_LEN;
        let epoch_bytes =
            <[u8; CHAIN_EPOCH_ID_LEN]>::try_from(&key_bytes[epoch_start..height_start]).ok()?;
        let height_bytes = <[u8; BLOCK_HEIGHT_LEN]>::try_from(&key_bytes[height_start..]).ok()?;
        Some((
            ChainEpochId::new(u64::from_be_bytes(epoch_bytes)),
            BlockHeight::new(u32::from_be_bytes(height_bytes)),
        ))
    }

    pub(crate) fn subtree_root(
        network: Network,
        chain_epoch: ChainEpochId,
        protocol: ShieldedProtocol,
        subtree_index: SubtreeRootIndex,
    ) -> Self {
        let mut key = artifact_key_prefix(SUBTREE_ROOT_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        key.push(protocol.id());
        key.extend_from_slice(&subtree_index.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn address_output_index_network_prefix(network: Network) -> Self {
        let mut key = artifact_key_prefix(ADDRESS_OUTPUT_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        Self(key)
    }

    pub(crate) fn address_output_index_prefix(
        network: Network,
        address_script_hash: TransparentAddressScriptHash,
    ) -> Self {
        let mut key = Self::address_output_index_network_prefix(network).0;
        key.extend_from_slice(&address_script_hash.as_bytes());
        Self(key)
    }

    pub(crate) fn address_output_index(
        network: Network,
        address_script_hash: TransparentAddressScriptHash,
        height: BlockHeight,
        outpoint: TransparentOutPoint,
    ) -> Self {
        let mut key = Self::address_output_index_prefix(network, address_script_hash).0;
        key.extend_from_slice(&height.value().to_be_bytes());
        key.extend_from_slice(&outpoint.transaction_id.as_bytes());
        key.extend_from_slice(&outpoint.output_index.to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_output_network_prefix(network: Network) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_OUTPUT_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_output(network: Network, outpoint: TransparentOutPoint) -> Self {
        let mut key = Self::transparent_output_network_prefix(network).0;
        key.extend_from_slice(&outpoint.transaction_id.as_bytes());
        key.extend_from_slice(&outpoint.output_index.to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_output_block_index_prefix(
        network: Network,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_OUTPUT_BLOCK_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&height.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_output_block_index(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::transparent_output_block_index_prefix(network, height).0;
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_spend_fact(network: Network, outpoint: TransparentOutPoint) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_SPEND_FACT_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&outpoint.transaction_id.as_bytes());
        key.extend_from_slice(&outpoint.output_index.to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_spend_fact_block_index_prefix(
        network: Network,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_SPEND_FACT_BLOCK_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&height.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_spend_fact_block_index(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::transparent_spend_fact_block_index_prefix(network, height).0;
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn block_hash_index(network: Network, block_hash: BlockHash) -> Self {
        let mut key = artifact_key_prefix(BLOCK_HASH_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&block_hash.as_bytes());
        Self(key)
    }

    pub(crate) fn displaced_block_order_prefix(network: Network) -> Self {
        let mut key = artifact_key_prefix(DISPLACED_BLOCK_BY_ORDER_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        Self(key)
    }

    pub(crate) fn displaced_block_by_order(
        network: Network,
        event_sequence: u64,
        height: BlockHeight,
    ) -> Self {
        let mut key = Self::displaced_block_order_prefix(network).0;
        key.extend_from_slice(&event_sequence.to_be_bytes());
        key.extend_from_slice(&height.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn displaced_block_event_prefix(network: Network, event_sequence: u64) -> Self {
        let mut key = Self::displaced_block_order_prefix(network).0;
        key.extend_from_slice(&event_sequence.to_be_bytes());
        Self(key)
    }

    pub(crate) fn displaced_block_by_hash(network: Network, block_hash: BlockHash) -> Self {
        let mut key = artifact_key_prefix(DISPLACED_BLOCK_BY_HASH_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&block_hash.as_bytes());
        Self(key)
    }

    pub(crate) fn displaced_root_index_prefix(
        network: Network,
        root: FinalNoteCommitmentRoot,
        protocol: ShieldedProtocol,
    ) -> Self {
        let mut key = artifact_key_prefix(DISPLACED_ROOT_INDEX_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&root.as_bytes());
        key.push(protocol.id());
        Self(key)
    }

    pub(crate) fn displaced_root_index(
        network: Network,
        root: FinalNoteCommitmentRoot,
        protocol: ShieldedProtocol,
        position: (u64, BlockId),
    ) -> Self {
        let (event_sequence, block_id) = position;
        let mut key = Self::displaced_root_index_prefix(network, root, protocol).0;
        key.extend_from_slice(&event_sequence.to_be_bytes());
        key.extend_from_slice(&block_id.height.value().to_be_bytes());
        key.extend_from_slice(&block_id.hash.as_bytes());
        Self(key)
    }

    pub(crate) fn displaced_root_index_position(key_bytes: &[u8]) -> Option<(u64, BlockId)> {
        if key_bytes.len() != DISPLACED_ROOT_INDEX_KEY_LEN
            || key_bytes.first().copied() != Some(KEY_VERSION)
            || key_bytes.get(1).copied() != Some(DISPLACED_ROOT_INDEX_KEY_KIND)
        {
            return None;
        }
        let event_start = DISPLACED_ROOT_INDEX_PREFIX_LEN;
        let height_start = event_start + DISPLACEMENT_EVENT_SEQUENCE_LEN;
        let hash_start = height_start + BLOCK_HEIGHT_LEN;
        let event_bytes = key_bytes[event_start..height_start].try_into().ok()?;
        let height_bytes = key_bytes[height_start..hash_start].try_into().ok()?;
        let block_hash_bytes = key_bytes[hash_start..].try_into().ok()?;
        Some((
            u64::from_be_bytes(event_bytes),
            BlockId::new(
                BlockHeight::new(u32::from_be_bytes(height_bytes)),
                BlockHash::from_bytes(block_hash_bytes),
            ),
        ))
    }

    pub(crate) fn chain_event(event_sequence: u64) -> Self {
        let mut key = vec![KEY_VERSION];
        key.extend_from_slice(&event_sequence.to_be_bytes());
        Self(key)
    }

    pub(crate) fn mempool_event(event_sequence: u64) -> Self {
        let mut key = vec![KEY_VERSION, MEMPOOL_EVENT_KEY_KIND];
        key.extend_from_slice(&event_sequence.to_be_bytes());
        Self(key)
    }

    /// Extracts the event sequence from a raw `MempoolEvent` column-family
    /// key. Returns `None` when the key has the wrong length, version
    /// prefix, or kind byte.
    pub(crate) fn mempool_event_sequence_from_key(key_bytes: &[u8]) -> Option<u64> {
        if key_bytes.len() != STORE_KEY_HEADER_LEN + size_of::<u64>()
            || key_bytes[0] != KEY_VERSION
            || key_bytes[1] != MEMPOOL_EVENT_KEY_KIND
        {
            return None;
        }
        let sequence_bytes: [u8; size_of::<u64>()] =
            key_bytes[STORE_KEY_HEADER_LEN..].try_into().ok()?;
        Some(u64::from_be_bytes(sequence_bytes))
    }

    pub(crate) fn visible_block_epoch_prefix(network: Network, height: BlockHeight) -> Self {
        visible_height_epoch_prefix(VISIBLE_BLOCK_EPOCH_KEY_KIND, network, height)
    }

    pub(crate) fn visible_block_epoch(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        visible_height_epoch_key(VISIBLE_BLOCK_EPOCH_KEY_KIND, network, height, chain_epoch)
    }

    pub(crate) fn visible_compact_block_epoch_prefix(
        network: Network,
        height: BlockHeight,
    ) -> Self {
        visible_height_epoch_prefix(VISIBLE_COMPACT_BLOCK_EPOCH_KEY_KIND, network, height)
    }

    pub(crate) fn visible_compact_block_epoch(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        visible_height_epoch_key(
            VISIBLE_COMPACT_BLOCK_EPOCH_KEY_KIND,
            network,
            height,
            chain_epoch,
        )
    }

    pub(crate) fn visible_final_note_commitment_roots_epoch_prefix(
        network: Network,
        height: BlockHeight,
    ) -> Self {
        visible_height_epoch_prefix(
            VISIBLE_FINAL_NOTE_COMMITMENT_ROOTS_EPOCH_KEY_KIND,
            network,
            height,
        )
    }

    pub(crate) fn visible_final_note_commitment_roots_epoch(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        visible_height_epoch_key(
            VISIBLE_FINAL_NOTE_COMMITMENT_ROOTS_EPOCH_KEY_KIND,
            network,
            height,
            chain_epoch,
        )
    }

    pub(crate) fn visible_block_value_pool_balances_epoch_prefix(
        network: Network,
        height: BlockHeight,
    ) -> Self {
        visible_height_epoch_prefix(
            VISIBLE_BLOCK_VALUE_POOL_BALANCES_EPOCH_KEY_KIND,
            network,
            height,
        )
    }

    pub(crate) fn visible_block_value_pool_balances_epoch(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        visible_height_epoch_key(
            VISIBLE_BLOCK_VALUE_POOL_BALANCES_EPOCH_KEY_KIND,
            network,
            height,
            chain_epoch,
        )
    }

    #[cfg(test)]
    pub(crate) fn visible_tree_state_epoch_prefix(network: Network, height: BlockHeight) -> Self {
        visible_height_epoch_prefix(VISIBLE_TREE_STATE_EPOCH_KEY_KIND, network, height)
    }

    pub(crate) fn visible_tree_state_epoch(
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> Self {
        visible_height_epoch_key(
            VISIBLE_TREE_STATE_EPOCH_KEY_KIND,
            network,
            height,
            chain_epoch,
        )
    }

    pub(crate) fn visible_transaction_epoch_prefix(
        network: Network,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = vec![KEY_VERSION, VISIBLE_TRANSACTION_EPOCH_KEY_KIND];
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&transaction_id.as_bytes());
        Self(key)
    }

    pub(crate) fn visible_transaction_epoch(
        network: Network,
        transaction_id: TransactionId,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::visible_transaction_epoch_prefix(network, transaction_id).0;
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn visible_subtree_root_epoch_prefix(
        network: Network,
        protocol: ShieldedProtocol,
        subtree_index: SubtreeRootIndex,
    ) -> Self {
        let mut key = vec![KEY_VERSION, VISIBLE_SUBTREE_ROOT_EPOCH_KEY_KIND];
        key.extend_from_slice(&network.id().to_be_bytes());
        key.push(protocol.id());
        key.extend_from_slice(&subtree_index.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn visible_subtree_root_epoch(
        network: Network,
        protocol: ShieldedProtocol,
        subtree_index: SubtreeRootIndex,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::visible_subtree_root_epoch_prefix(network, protocol, subtree_index).0;
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    /// Returns the ordered key bytes.
    #[must_use]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the key and returns the ordered key bytes.
    #[must_use]
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub(crate) fn reorg_window_prefix_len(key_bytes: &[u8]) -> Option<usize> {
        if key_bytes.len() < STORE_KEY_HEADER_LEN || key_bytes[0] != KEY_VERSION {
            return None;
        }

        let prefix_len = match key_bytes[1] {
            VISIBLE_BLOCK_EPOCH_KEY_KIND
            | VISIBLE_COMPACT_BLOCK_EPOCH_KEY_KIND
            | VISIBLE_TREE_STATE_EPOCH_KEY_KIND
            | VISIBLE_FINAL_NOTE_COMMITMENT_ROOTS_EPOCH_KEY_KIND
            | VISIBLE_BLOCK_VALUE_POOL_BALANCES_EPOCH_KEY_KIND => HEIGHT_VISIBILITY_PREFIX_LEN,
            VISIBLE_TRANSACTION_EPOCH_KEY_KIND => TRANSACTION_VISIBILITY_PREFIX_LEN,
            VISIBLE_SUBTREE_ROOT_EPOCH_KEY_KIND => SUBTREE_ROOT_VISIBILITY_PREFIX_LEN,
            _ => return None,
        };

        (key_bytes.len() >= prefix_len).then_some(prefix_len)
    }

    /// Extracts the outpoint from a raw `transparent_output` or
    /// `transparent_spend_fact` column-family key. Returns `None` when the
    /// key has the wrong length, version prefix, or kind byte.
    pub(crate) fn transparent_outpoint_from_key(key_bytes: &[u8]) -> Option<TransparentOutPoint> {
        const OUTPUT_INDEX_LEN: usize = 4;
        let expected_len =
            STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + TRANSACTION_ID_LEN + OUTPUT_INDEX_LEN;
        if key_bytes.len() != expected_len
            || key_bytes[0] != KEY_VERSION
            || !matches!(
                key_bytes[1],
                TRANSPARENT_OUTPUT_KEY_KIND | TRANSPARENT_SPEND_FACT_KEY_KIND
            )
        {
            return None;
        }
        let transaction_id_start = STORE_KEY_HEADER_LEN + NETWORK_ID_LEN;
        let output_index_start = transaction_id_start + TRANSACTION_ID_LEN;
        let transaction_id_bytes = <[u8; TRANSACTION_ID_LEN]>::try_from(
            &key_bytes[transaction_id_start..output_index_start],
        )
        .ok()?;
        let output_index_bytes =
            <[u8; OUTPUT_INDEX_LEN]>::try_from(&key_bytes[output_index_start..]).ok()?;

        Some(TransparentOutPoint::new(
            TransactionId::from_bytes(transaction_id_bytes),
            u32::from_be_bytes(output_index_bytes),
        ))
    }

    pub(crate) fn transparent_artifact_chain_epoch_id(key_bytes: &[u8]) -> Option<ChainEpochId> {
        if key_bytes.len() < STORE_KEY_HEADER_LEN || key_bytes[0] != KEY_VERSION {
            return None;
        }
        if !matches!(
            key_bytes[1],
            TRANSPARENT_SPEND_FACT_BLOCK_INDEX_KEY_KIND | TRANSPARENT_OUTPUT_BLOCK_INDEX_KEY_KIND
        ) {
            return None;
        }
        let epoch_start = key_bytes.len().checked_sub(CHAIN_EPOCH_ID_LEN)?;
        let chain_epoch_bytes =
            <[u8; CHAIN_EPOCH_ID_LEN]>::try_from(&key_bytes[epoch_start..]).ok()?;

        Some(ChainEpochId::new(u64::from_be_bytes(chain_epoch_bytes)))
    }
}

fn artifact_key_prefix(artifact_kind: u8) -> Vec<u8> {
    vec![KEY_VERSION, artifact_kind]
}

fn push_network_epoch_height(
    key: &mut Vec<u8>,
    network: Network,
    chain_epoch: ChainEpochId,
    height: BlockHeight,
) {
    key.extend_from_slice(&network.id().to_be_bytes());
    key.extend_from_slice(&chain_epoch.value().to_be_bytes());
    key.extend_from_slice(&height.value().to_be_bytes());
}

fn visible_height_epoch_prefix(
    artifact_kind: u8,
    network: Network,
    height: BlockHeight,
) -> StoreKey {
    let mut key = vec![KEY_VERSION, artifact_kind];
    key.extend_from_slice(&network.id().to_be_bytes());
    key.extend_from_slice(&height.value().to_be_bytes());
    StoreKey(key)
}

fn visible_height_epoch_key(
    artifact_kind: u8,
    network: Network,
    height: BlockHeight,
    chain_epoch: ChainEpochId,
) -> StoreKey {
    let mut key = visible_height_epoch_prefix(artifact_kind, network, height).0;
    key.extend_from_slice(&chain_epoch.value().to_be_bytes());
    StoreKey(key)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use zinder_core::{FinalNoteCommitmentRoot, Network, ShieldedProtocol};

    use super::{BlockHeight, ChainEpochId, StoreKey, SubtreeRootIndex, TransactionId};

    #[test]
    fn artifact_and_visibility_key_kinds_are_disjoint() {
        let network = Network::ZcashRegtest;
        let chain_epoch = ChainEpochId::new(7);
        let height = BlockHeight::new(42);
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        let subtree_index = SubtreeRootIndex::new(3);
        let block_hash = zinder_core::BlockHash::from_bytes([0x22; 32]);
        let root = FinalNoteCommitmentRoot::from_bytes([0x33; 32]);
        let artifact_prefixes = [
            StoreKey::block_header(network, chain_epoch, height),
            StoreKey::compact_block(network, chain_epoch, height),
            StoreKey::transaction_facts(network, chain_epoch, transaction_id),
            StoreKey::transaction_intrinsic_value_balances(network, chain_epoch, transaction_id),
            StoreKey::tree_state(network, chain_epoch, height),
            StoreKey::final_note_commitment_roots(network, chain_epoch, height),
            StoreKey::subtree_root(
                network,
                chain_epoch,
                ShieldedProtocol::Sapling,
                subtree_index,
            ),
            StoreKey::address_output_index(
                network,
                zinder_core::TransparentAddressScriptHash::from_bytes([0x44; 32]),
                height,
                zinder_core::TransparentOutPoint::new(transaction_id, 0),
            ),
            StoreKey::transparent_spend_fact(
                network,
                zinder_core::TransparentOutPoint::new(transaction_id, 0),
            ),
            StoreKey::transparent_spend_fact_block_index(network, height, chain_epoch),
            StoreKey::transparent_output(
                network,
                zinder_core::TransparentOutPoint::new(transaction_id, 0),
            ),
            StoreKey::transparent_output_block_index(network, height, chain_epoch),
            StoreKey::mempool_event(99),
            StoreKey::displaced_block_by_order(network, 99, height),
            StoreKey::displaced_block_by_hash(network, block_hash),
            StoreKey::displaced_root_index(
                network,
                root,
                ShieldedProtocol::Sapling,
                (99, zinder_core::BlockId::new(height, block_hash)),
            ),
        ]
        .map(|key| key.as_bytes()[..2].to_vec());
        let visibility_prefixes = [
            StoreKey::visible_block_epoch_prefix(network, height),
            StoreKey::visible_compact_block_epoch_prefix(network, height),
            StoreKey::visible_final_note_commitment_roots_epoch_prefix(network, height),
            StoreKey::visible_transaction_epoch_prefix(network, transaction_id),
            StoreKey::visible_tree_state_epoch_prefix(network, height),
            StoreKey::visible_subtree_root_epoch_prefix(
                network,
                ShieldedProtocol::Sapling,
                subtree_index,
            ),
        ]
        .map(|key| key.as_bytes()[..2].to_vec());
        let raw_artifact_prefix_count = artifact_prefixes.len();
        let artifact_prefixes = artifact_prefixes.into_iter().collect::<HashSet<_>>();
        assert_eq!(
            artifact_prefixes.len(),
            raw_artifact_prefix_count,
            "artifact key kinds collide within the artifact namespace"
        );

        for visibility_prefix in visibility_prefixes {
            assert!(!artifact_prefixes.contains(&visibility_prefix));
        }
    }

    #[test]
    fn storage_control_singletons_are_disjoint_from_chain_epoch_keys() {
        let storage_control_keys = [
            StoreKey::visible_chain_epoch_pointer(),
            StoreKey::chain_event_sequence_pointer(),
            StoreKey::cursor_auth_key(),
            StoreKey::oldest_retained_chain_event_sequence(),
            StoreKey::store_metadata(),
            StoreKey::mempool_event_sequence_pointer(),
            StoreKey::oldest_retained_mempool_event_sequence(),
            StoreKey::transparent_retention_swept_height(),
            StoreKey::raw_blob_retention(),
            StoreKey::transparent_retention_release_height(),
            StoreKey::transparent_retention_deleted_through_height(),
            StoreKey::displaced_block_archive_coverage(),
            StoreKey::displaced_block_count(),
            StoreKey::displaced_root_archive_coverage(),
        ]
        .map(StoreKey::into_bytes)
        .into_iter()
        .collect::<HashSet<_>>();

        assert_eq!(storage_control_keys.len(), 14);
        for chain_epoch in [
            ChainEpochId::new(0),
            ChainEpochId::new(1),
            ChainEpochId::new(12),
        ] {
            assert!(
                !storage_control_keys.contains(&StoreKey::chain_epoch(chain_epoch).into_bytes())
            );
            assert!(
                !storage_control_keys
                    .contains(&StoreKey::chain_event(chain_epoch.value()).into_bytes())
            );
            assert!(
                !storage_control_keys
                    .contains(&StoreKey::mempool_event(chain_epoch.value()).into_bytes())
            );
        }
    }

    #[test]
    fn displaced_root_index_key_round_trips_occurrence_position() {
        let height = BlockHeight::new(42);
        let block_hash = zinder_core::BlockHash::from_bytes([0x44; 32]);
        let key = StoreKey::displaced_root_index(
            Network::ZcashRegtest,
            FinalNoteCommitmentRoot::from_bytes([0x55; 32]),
            ShieldedProtocol::Orchard,
            (73, zinder_core::BlockId::new(height, block_hash)),
        );

        assert_eq!(
            StoreKey::displaced_root_index_position(key.as_bytes()),
            Some((73, zinder_core::BlockId::new(height, block_hash)))
        );
    }
}
