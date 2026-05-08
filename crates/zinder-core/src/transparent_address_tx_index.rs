//! Canonical artifact: per-row index of confirmed transparent-address
//! transaction history.
//!
//! One artifact per `(address_script_hash, transaction_id)` pair, regardless
//! of how many transparent inputs or outputs the transaction has for that
//! address. The artifact carries the position metadata needed to construct
//! stable per-tx links without a follow-up call.
//!
//! Storage uses the dynamic-filter visibility pattern from M4 Slice A's
//! transparent UTXO family: rows are written and never physically deleted on
//! reorg; visibility is enforced at read time via the trailing
//! `chain_epoch_id` source-epoch filter and `block_is_visible` against the
//! row's `block_hash`.

use crate::{BlockHash, BlockHeight, TransactionId, TransparentAddressScriptHash};

/// One transparent-address tx-history artifact.
///
/// Keyed by the address script hash and the
/// `(block_height, tx_index_in_block)` position. Pagination cursors point
/// at exact `(height, tx_index)` boundaries so consumers resume cleanly
/// under reorgs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransparentAddressTxIndexArtifact {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Height of the block the indexed transaction was mined into.
    pub block_height: BlockHeight,
    /// Position of the indexed transaction inside its block, in mined order.
    pub tx_index_in_block: u32,
    /// Identifier of the indexed transaction.
    pub transaction_id: TransactionId,
    /// Hash of the block the indexed transaction was mined into. Compared
    /// against the visible chain at `block_height` by the dynamic-filter
    /// visibility check.
    pub block_hash: BlockHash,
}

impl TransparentAddressTxIndexArtifact {
    /// Constructs a new transparent-address tx-history artifact.
    #[must_use]
    pub const fn new(
        address_script_hash: TransparentAddressScriptHash,
        block_height: BlockHeight,
        tx_index_in_block: u32,
        transaction_id: TransactionId,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            address_script_hash,
            block_height,
            tx_index_in_block,
            transaction_id,
            block_hash,
        }
    }
}
