mod rocksdb;

pub(crate) use rocksdb::{
    PrefixScanControl, RocksChainStore, RocksChainStoreRead, RocksChainStoreReadView,
    StorageDelete, StoragePut,
};
pub use rocksdb::{
    build_block_based_table_factory, build_block_cache, build_primary_db_options,
    build_secondary_db_options,
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
    TransparentAddressTxIndex,
    BlockHashIndex,
    ReorgWindow,
    ChainEvent,
    MempoolEvent,
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
            Self::TransparentAddressTxIndex => "transparent_address_tx_index",
            Self::BlockHashIndex => "block_hash_index",
            Self::ReorgWindow => "reorg_window",
            Self::ChainEvent => "chain_event",
            Self::MempoolEvent => "mempool_event",
        }
    }

    pub(crate) const fn all() -> [Self; 14] {
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
            Self::TransparentAddressTxIndex,
            Self::BlockHashIndex,
            Self::ReorgWindow,
            Self::ChainEvent,
            Self::MempoolEvent,
        ]
    }
}
