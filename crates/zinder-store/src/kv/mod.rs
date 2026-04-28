mod rocksdb;

pub(crate) use rocksdb::{
    PrefixScanControl, RocksChainStore, RocksChainStoreRead, RocksChainStoreReadView,
    StorageDelete, StoragePut,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageTable {
    StorageControl,
    ChainEpoch,
    FinalizedBlock,
    CompactBlock,
    Transaction,
    TreeState,
    SubtreeRoot,
    TransparentAddressUtxo,
    TransparentUtxoSpend,
    ReorgWindow,
    ChainEvent,
}

impl StorageTable {
    pub(crate) const fn column_family_name(self) -> &'static str {
        match self {
            Self::StorageControl => "storage_control",
            Self::ChainEpoch => "chain_epoch",
            Self::FinalizedBlock => "finalized_block",
            Self::CompactBlock => "compact_block",
            Self::Transaction => "transaction",
            Self::TreeState => "tree_state",
            Self::SubtreeRoot => "subtree_root",
            Self::TransparentAddressUtxo => "transparent_address_utxo",
            Self::TransparentUtxoSpend => "transparent_utxo_spend",
            Self::ReorgWindow => "reorg_window",
            Self::ChainEvent => "chain_event",
        }
    }

    pub(crate) const fn all() -> [Self; 11] {
        [
            Self::StorageControl,
            Self::ChainEpoch,
            Self::FinalizedBlock,
            Self::CompactBlock,
            Self::Transaction,
            Self::TreeState,
            Self::SubtreeRoot,
            Self::TransparentAddressUtxo,
            Self::TransparentUtxoSpend,
            Self::ReorgWindow,
            Self::ChainEvent,
        ]
    }
}
