//! Store key byte contract.

use zinder_core::{
    BlockHeight, ChainEpochId, Network, ShieldedProtocol, SubtreeRootIndex, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint,
};

/// Ordered key bytes used inside `RocksDB` column families.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct StoreKey(Vec<u8>);

const KEY_VERSION: u8 = 1;
const FINALIZED_BLOCK_KEY_KIND: u8 = 1;
const COMPACT_BLOCK_KEY_KIND: u8 = 2;
const TRANSACTION_KEY_KIND: u8 = 3;
const TREE_STATE_KEY_KIND: u8 = 4;
const SUBTREE_ROOT_KEY_KIND: u8 = 5;
const TRANSPARENT_ADDRESS_UTXO_KEY_KIND: u8 = 6;
const TRANSPARENT_UTXO_SPEND_KEY_KIND: u8 = 7;
// Key kinds 8..=32 are reserved for future artifact families; visibility keys start at 33.
const VISIBLE_BLOCK_EPOCH_KEY_KIND: u8 = 33;
const VISIBLE_COMPACT_BLOCK_EPOCH_KEY_KIND: u8 = 34;
const VISIBLE_TREE_STATE_EPOCH_KEY_KIND: u8 = 35;
const VISIBLE_TRANSACTION_EPOCH_KEY_KIND: u8 = 36;
const VISIBLE_SUBTREE_ROOT_EPOCH_KEY_KIND: u8 = 37;

const STORE_KEY_HEADER_LEN: usize = 2;
const NETWORK_ID_LEN: usize = 4;
const BLOCK_HEIGHT_LEN: usize = 4;
const TRANSACTION_ID_LEN: usize = 32;
const CHAIN_EPOCH_ID_LEN: usize = 8;
const SHIELDED_PROTOCOL_LEN: usize = 1;
const SUBTREE_ROOT_INDEX_LEN: usize = 4;
const HEIGHT_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + BLOCK_HEIGHT_LEN;
const TRANSACTION_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + TRANSACTION_ID_LEN;
const SUBTREE_ROOT_VISIBILITY_PREFIX_LEN: usize =
    STORE_KEY_HEADER_LEN + NETWORK_ID_LEN + SHIELDED_PROTOCOL_LEN + SUBTREE_ROOT_INDEX_LEN;

impl StoreKey {
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

    pub(crate) fn chain_epoch(chain_epoch: ChainEpochId) -> Self {
        let mut key = vec![KEY_VERSION];
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn finalized_block(
        network: Network,
        chain_epoch: ChainEpochId,
        height: BlockHeight,
    ) -> Self {
        let mut key = artifact_key_prefix(FINALIZED_BLOCK_KEY_KIND);
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

    pub(crate) fn transaction(
        network: Network,
        chain_epoch: ChainEpochId,
        transaction_id: TransactionId,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSACTION_KEY_KIND);
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

    pub(crate) fn transparent_address_utxo_prefix(
        network: Network,
        address_script_hash: TransparentAddressScriptHash,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_ADDRESS_UTXO_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&address_script_hash.as_bytes());
        Self(key)
    }

    pub(crate) fn transparent_address_utxo(
        network: Network,
        address_script_hash: TransparentAddressScriptHash,
        height: BlockHeight,
        outpoint: TransparentOutPoint,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::transparent_address_utxo_prefix(network, address_script_hash).0;
        key.extend_from_slice(&height.value().to_be_bytes());
        key.extend_from_slice(&outpoint.transaction_id.as_bytes());
        key.extend_from_slice(&outpoint.output_index.to_be_bytes());
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_utxo_spend_prefix(
        network: Network,
        outpoint: TransparentOutPoint,
    ) -> Self {
        let mut key = artifact_key_prefix(TRANSPARENT_UTXO_SPEND_KEY_KIND);
        key.extend_from_slice(&network.id().to_be_bytes());
        key.extend_from_slice(&outpoint.transaction_id.as_bytes());
        key.extend_from_slice(&outpoint.output_index.to_be_bytes());
        Self(key)
    }

    pub(crate) fn transparent_utxo_spend(
        network: Network,
        outpoint: TransparentOutPoint,
        chain_epoch: ChainEpochId,
    ) -> Self {
        let mut key = Self::transparent_utxo_spend_prefix(network, outpoint).0;
        key.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Self(key)
    }

    pub(crate) fn chain_event(event_sequence: u64) -> Self {
        let mut key = vec![KEY_VERSION];
        key.extend_from_slice(&event_sequence.to_be_bytes());
        Self(key)
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
            | VISIBLE_TREE_STATE_EPOCH_KEY_KIND => HEIGHT_VISIBILITY_PREFIX_LEN,
            VISIBLE_TRANSACTION_EPOCH_KEY_KIND => TRANSACTION_VISIBILITY_PREFIX_LEN,
            VISIBLE_SUBTREE_ROOT_EPOCH_KEY_KIND => SUBTREE_ROOT_VISIBILITY_PREFIX_LEN,
            _ => return None,
        };

        (key_bytes.len() >= prefix_len).then_some(prefix_len)
    }

    pub(crate) fn transparent_artifact_chain_epoch_id(key_bytes: &[u8]) -> Option<ChainEpochId> {
        if key_bytes.len() < STORE_KEY_HEADER_LEN || key_bytes[0] != KEY_VERSION {
            return None;
        }
        if !matches!(
            key_bytes[1],
            TRANSPARENT_ADDRESS_UTXO_KEY_KIND | TRANSPARENT_UTXO_SPEND_KEY_KIND
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

    use zinder_core::{Network, ShieldedProtocol};

    use super::{BlockHeight, ChainEpochId, StoreKey, SubtreeRootIndex, TransactionId};

    #[test]
    fn artifact_and_visibility_key_kinds_are_disjoint() {
        let network = Network::ZcashRegtest;
        let chain_epoch = ChainEpochId::new(7);
        let height = BlockHeight::new(42);
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        let subtree_index = SubtreeRootIndex::new(3);
        let artifact_prefixes = [
            StoreKey::finalized_block(network, chain_epoch, height),
            StoreKey::compact_block(network, chain_epoch, height),
            StoreKey::transaction(network, chain_epoch, transaction_id),
            StoreKey::tree_state(network, chain_epoch, height),
            StoreKey::subtree_root(
                network,
                chain_epoch,
                ShieldedProtocol::Sapling,
                subtree_index,
            ),
            StoreKey::transparent_address_utxo(
                network,
                zinder_core::TransparentAddressScriptHash::from_bytes([0x44; 32]),
                height,
                zinder_core::TransparentOutPoint::new(transaction_id, 0),
                chain_epoch,
            ),
            StoreKey::transparent_utxo_spend(
                network,
                zinder_core::TransparentOutPoint::new(transaction_id, 0),
                chain_epoch,
            ),
        ]
        .map(|key| key.as_bytes()[..2].to_vec());
        let visibility_prefixes = [
            StoreKey::visible_block_epoch_prefix(network, height),
            StoreKey::visible_compact_block_epoch_prefix(network, height),
            StoreKey::visible_transaction_epoch_prefix(network, transaction_id),
            StoreKey::visible_tree_state_epoch_prefix(network, height),
            StoreKey::visible_subtree_root_epoch_prefix(
                network,
                ShieldedProtocol::Sapling,
                subtree_index,
            ),
        ]
        .map(|key| key.as_bytes()[..2].to_vec());
        let artifact_prefixes = artifact_prefixes.into_iter().collect::<HashSet<_>>();

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
            StoreKey::store_metadata(),
        ]
        .map(StoreKey::into_bytes)
        .into_iter()
        .collect::<HashSet<_>>();

        assert_eq!(storage_control_keys.len(), 4);
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
        }
    }
}
