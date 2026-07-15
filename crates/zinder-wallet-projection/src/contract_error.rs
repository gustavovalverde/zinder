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
    /// A checkpoint manifest captured an incomplete wallet store.
    #[error("wallet checkpoint requires a ready wallet control record")]
    CheckpointRequiresReadyControl,
    /// Ready evidence must cover complete history beginning at height one.
    #[error("wallet readiness coverage must begin at block height one")]
    ReadyCoverageMustBeginAtHeightOne,
    /// Ready coverage and source position identify different tips.
    #[error("wallet readiness coverage tip does not match its source position")]
    ReadyCoverageTipMismatch,
    /// A ready projection cannot retain unresolved transparent predecessors.
    #[error("wallet readiness requires zero unresolved transparent predecessors")]
    ReadyHasUnresolvedPredecessors,
    /// Every live output must have exactly one address secondary-index row.
    #[error("wallet live-output and address-index row counts do not match")]
    ReadyLiveOutputIndexCountMismatch,
    /// The ready UTXO count must equal the live-output row count.
    #[error("wallet UTXO count does not match the live-output row count")]
    ReadyUtxoCountMismatch,
    /// The source sequence must contain one digest for every projected height.
    #[error("wallet source sequence length does not match its projected tip")]
    ReadySourceSequenceLengthMismatch,
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
