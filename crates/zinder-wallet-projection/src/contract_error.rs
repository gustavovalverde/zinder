//! Fail-closed errors shared by wallet projection contracts.

use thiserror::Error;

/// Contract construction or serial-oracle failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum WalletProjectionContractError {
    /// An output's outpoint and creation position name different transactions.
    #[error("wallet output creator does not match its outpoint transaction")]
    OutputCreatorMismatch,
    /// A variable-length field exceeds the version-1 `u32` length prefix.
    #[error("{field} exceeds the version-1 u32 length limit")]
    FieldTooLong {
        /// Field whose length could not be represented.
        field: &'static str,
    },
    /// A fixed-length durable field has the wrong number of bytes.
    #[error("{field} requires exactly {expected} bytes, observed {actual}")]
    DurableFieldLengthMismatch {
        /// Durable field being decoded.
        field: &'static str,
        /// Exact version-1 byte length.
        expected: usize,
        /// Observed byte length.
        actual: usize,
    },
    /// A variable-length durable value is shorter than its fixed prefix.
    #[error("{field} requires at least {minimum} bytes, observed {actual}")]
    DurableValueTooShort {
        /// Durable value being decoded.
        field: &'static str,
        /// Minimum version-1 byte length.
        minimum: usize,
        /// Observed byte length.
        actual: usize,
    },
    /// A durable value's length prefix does not consume the complete value.
    #[error("{field} length prefix does not match its encoded bytes")]
    DurableLengthPrefixMismatch {
        /// Durable value being decoded.
        field: &'static str,
    },
    /// A durable list is not in strict canonical key order.
    #[error("{field} keys must be strictly increasing")]
    DurableKeyOrder {
        /// Durable list whose ordering was invalid.
        field: &'static str,
    },
    /// A durable identity marker does not match the version-1 contract.
    #[error("{field} identity does not match version 1")]
    DurableIdentityMismatch {
        /// Durable contract being decoded.
        field: &'static str,
    },
    /// A durable numeric discriminator is unsupported by version 1.
    #[error("{field} has unsupported encoded value {encoded}")]
    UnsupportedEncodedValue {
        /// Durable discriminator being decoded.
        field: &'static str,
        /// Unsupported numeric value.
        encoded: u64,
    },
    /// A durable record contains bytes after its complete version-1 value.
    #[error("{field} contains trailing bytes")]
    DurableTrailingBytes {
        /// Durable record being decoded.
        field: &'static str,
    },
    /// A parsed durable record does not reproduce its exact input bytes.
    #[error("{field} is not canonically encoded")]
    DurableNonCanonicalEncoding {
        /// Durable record being decoded.
        field: &'static str,
    },
    /// Projection digest rows are not strictly ordered by durable key.
    #[error("wallet projection digest row keys must be strictly increasing")]
    ProjectionDigestKeyOrder,
    /// A projection digest family contains more than `u64::MAX` rows.
    #[error("wallet projection digest family row count exceeds u64::MAX")]
    ProjectionDigestRowCountOverflow,
    /// A canonical block does not extend the oracle's current tip.
    #[error("canonical block does not extend the wallet projection serial oracle")]
    NonContiguousBlock,
    /// Canonical facts repeated an already-created outpoint.
    #[error("canonical facts contain a duplicate transparent output")]
    DuplicateOutput,
    /// A transparent input references an output absent from complete history.
    #[error("transparent input predecessor is absent from complete history")]
    MissingTransparentPredecessor,
    /// Canonical facts repeat a transparent spend.
    #[error("canonical facts contain a duplicate transparent spend")]
    DuplicateSpend,
    /// Canonical transaction or transparent fact order cannot fit its `u32` contract.
    #[error("canonical fact position exceeds the version-1 u32 index limit")]
    FactIndexOverflow,
    /// Canonical transparent fact index disagrees with its ordered vector position.
    #[error("canonical transparent fact index disagrees with vector order")]
    FactIndexMismatch,
    /// An address balance addition exceeded `u64::MAX`.
    #[error("wallet address balance exceeds u64::MAX zatoshi")]
    AddressBalanceOverflow,
    /// An address balance could not cover a spent output.
    #[error("wallet address balance is below a spent output value")]
    AddressBalanceUnderflow,
    /// The UTXO count exceeded `u64::MAX`.
    #[error("wallet UTXO count exceeds u64::MAX")]
    UtxoCountOverflow,
    /// The UTXO value total exceeded `u64::MAX`.
    #[error("wallet UTXO value total exceeds u64::MAX zatoshi")]
    UtxoValueOverflow,
    /// The oracle could not remove one known UTXO from its count.
    #[error("wallet UTXO count underflow")]
    UtxoCountUnderflow,
    /// The oracle could not remove one known UTXO from its value total.
    #[error("wallet UTXO value underflow")]
    UtxoValueUnderflow,
    /// Every unspent output must have exactly one address secondary-index row.
    #[error("wallet unspent-output and address-index row counts do not match")]
    ReadyUnspentOutputIndexCountMismatch,
    /// The ready UTXO count must equal the unspent-output row count.
    #[error("wallet UTXO count does not match the unspent-output row count")]
    ReadyUtxoCountMismatch,
    /// The ready undo family must cover the configured tip window exactly.
    #[error("wallet reorg-undo count does not match its supported depth")]
    ReadyReorgUndoCountMismatch,
    /// The source sequence must contain one digest for every projected height.
    #[error("wallet source sequence length does not match its projected tip")]
    ReadySourceSequenceLengthMismatch,
    /// Version 1 accepts only canonical sequence digest version 1.
    #[error("wallet source sequence digest must use version 1")]
    ReadySourceSequenceVersionMismatch,
    /// Version 1 requires an `LtHash16` UTXO commitment.
    #[error("wallet readiness requires the LtHash16 UTXO commitment scheme")]
    ReadyUtxoCommitmentSchemeMismatch,
}

pub(crate) fn encoded_len(
    len: usize,
    field: &'static str,
) -> Result<u32, WalletProjectionContractError> {
    u32::try_from(len).map_err(|_| WalletProjectionContractError::FieldTooLong { field })
}
