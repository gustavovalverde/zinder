mod rocksdb;

pub use rocksdb::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, build_block_based_table_factory,
    open_bounded_rocksdb,
};
pub(crate) use rocksdb::{
    PrefixScanControl, RocksChainStore, RocksChainStoreRead, RocksChainStoreReadView,
    StorageDelete, StoragePut,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageTable {
    StorageControl,
    ChainEpoch,
    BlockHeader,
    BlockBlob,
    CompactBlock,
    BlockTransactionIndex,
    TransactionLocation,
    TransactionFacts,
    TransactionBlob,
    TreeState,
    SubtreeRoot,
    AddressOutputIndex,
    TransparentOutput,
    TransparentOutputBlockIndex,
    TransparentSpendFact,
    TransparentSpendFactBlockIndex,
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
            Self::BlockHeader => "block_header",
            Self::BlockBlob => "block_blob",
            Self::CompactBlock => "compact_block",
            Self::BlockTransactionIndex => "block_transaction_index",
            Self::TransactionLocation => "transaction_location",
            Self::TransactionFacts => "transaction_facts",
            Self::TransactionBlob => "transaction_blob",
            Self::TreeState => "tree_state",
            Self::SubtreeRoot => "subtree_root",
            Self::AddressOutputIndex => "address_output_index",
            Self::TransparentOutput => "transparent_output",
            Self::TransparentOutputBlockIndex => "transparent_output_block_index",
            Self::TransparentSpendFact => "transparent_spend_fact",
            Self::TransparentSpendFactBlockIndex => "transparent_spend_fact_block_index",
            Self::TransparentAddressTxIndex => "transparent_address_tx_index",
            Self::BlockHashIndex => "block_hash_index",
            Self::ReorgWindow => "reorg_window",
            Self::ChainEvent => "chain_event",
            Self::MempoolEvent => "mempool_event",
        }
    }

    pub(crate) const fn all() -> [Self; 21] {
        [
            Self::StorageControl,
            Self::ChainEpoch,
            Self::BlockHeader,
            Self::BlockBlob,
            Self::CompactBlock,
            Self::BlockTransactionIndex,
            Self::TransactionLocation,
            Self::TransactionFacts,
            Self::TransactionBlob,
            Self::TreeState,
            Self::SubtreeRoot,
            Self::AddressOutputIndex,
            Self::TransparentOutput,
            Self::TransparentOutputBlockIndex,
            Self::TransparentSpendFact,
            Self::TransparentSpendFactBlockIndex,
            Self::TransparentAddressTxIndex,
            Self::BlockHashIndex,
            Self::ReorgWindow,
            Self::ChainEvent,
            Self::MempoolEvent,
        ]
    }
}
