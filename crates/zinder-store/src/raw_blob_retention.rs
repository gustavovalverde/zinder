//! Raw consensus-blob retention contract.

use std::fmt;

/// Raw-blob retention fixed by the primary writer after the first canonical commit.
///
/// Readers use this persisted value for capability discovery. Changing it on a
/// non-empty store would make that capability lie about historical coverage,
/// so opening with a different value fails closed and requires a rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RawBlobRetention {
    /// Neither block nor transaction blobs are retained.
    None,
    /// Transaction blobs are retained; block blobs are not.
    Transactions,
    /// Both block and transaction blobs are retained.
    All,
}

impl RawBlobRetention {
    /// Returns the configuration spelling for this retention contract.
    #[must_use]
    pub const fn as_kebab_case(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Transactions => "transactions",
            Self::All => "all",
        }
    }

    /// Whether full block blobs are retained.
    #[must_use]
    pub const fn retains_block_blobs(self) -> bool {
        matches!(self, Self::All)
    }

    /// Whether transaction blobs are retained.
    #[must_use]
    pub const fn retains_transaction_blobs(self) -> bool {
        matches!(self, Self::Transactions | Self::All)
    }
}

impl fmt::Display for RawBlobRetention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_kebab_case())
    }
}
