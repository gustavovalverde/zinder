//! Structured wallet scan data shared by mined and mempool transactions.

/// One compact Sapling spend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactSaplingSpend {
    /// Spend nullifier as exactly 32 consensus bytes.
    pub nullifier: [u8; 32],
}

/// One compact Sapling output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactSaplingOutput {
    /// Note commitment as exactly 32 consensus bytes.
    pub commitment: [u8; 32],
    /// Ephemeral key as exactly 32 consensus bytes.
    pub ephemeral_key: [u8; 32],
    /// First exactly 52 bytes of encrypted note ciphertext.
    pub ciphertext: [u8; 52],
}

/// One compact Orchard or Ironwood action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactShieldedAction {
    /// Action nullifier as exactly 32 consensus bytes.
    pub nullifier: [u8; 32],
    /// Note commitment as exactly 32 consensus bytes.
    pub commitment: [u8; 32],
    /// Ephemeral key as exactly 32 consensus bytes.
    pub ephemeral_key: [u8; 32],
    /// First exactly 52 bytes of encrypted note ciphertext.
    pub ciphertext: [u8; 52],
}

/// One compact transparent input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactTransparentInput {
    /// Transaction that created the spent output.
    pub previous_transaction_id: crate::TransactionId,
    /// Output index in the creating transaction.
    pub previous_output_index: u32,
}

/// One compact transparent output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactTransparentOutput {
    /// Output value in zatoshis.
    pub value_zat: u64,
    /// Consensus scriptPubKey bytes.
    pub script_pub_key: Vec<u8>,
}

/// Wallet scan fields shared by mined and mempool transactions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactTransactionData {
    /// Transaction fee when the source can derive it; `None` means unknown.
    pub fee_zat: Option<u64>,
    /// Sapling spends in consensus order.
    pub sapling_spends: Vec<CompactSaplingSpend>,
    /// Sapling outputs in consensus order.
    pub sapling_outputs: Vec<CompactSaplingOutput>,
    /// Orchard actions in consensus order.
    pub orchard_actions: Vec<CompactShieldedAction>,
    /// Ironwood actions in consensus order.
    pub ironwood_actions: Vec<CompactShieldedAction>,
    /// Transparent inputs in consensus order.
    pub transparent_inputs: Vec<CompactTransparentInput>,
    /// Transparent outputs in consensus order.
    pub transparent_outputs: Vec<CompactTransparentOutput>,
}
