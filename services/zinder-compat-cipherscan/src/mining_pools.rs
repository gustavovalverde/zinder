//! Cipherscan's known mining-pool payout-address labels.

static POOL_NAME_BY_ADDRESS: [(&str, &str); 17] = [
    ("t1SqwRAAdSig6dE4EBPLonAait219VmkUjP", "Foundry USA"),
    ("t1PEp2GJLSdhDfCKqc2J211WKDUS1NfoQNy", "F2Pool"),
    ("t1at7nVNsv6taLRrNRvnQdtfLNRDfsGc3Ak", "ViaBTC"),
    ("t1K79TgQbqu74d6rBmsMu2oFEXEwAmdYiT7", "Unidentified #5"),
    (
        "t1MKn34KBa8Xh4g8qU8psibBXvURafphVn7",
        "Unidentified (Dominant)",
    ),
    ("t1ZVi2YGk98tEGYcNpXYnJFWCoLG2oYwv3J", "AntPool"),
    ("t1L2b66MXbgpVMXDfUa94GCBFAN4dCxGohM", "AntPool"),
    ("t1bnxtY7aLCjWx9Ru1YcGwRWch3eEWUFK7u", "2Miners"),
    ("t1fu6KgYtHEXk2ZhTpM1XD7jbnSmW6wokDM", "2Miners"),
    ("t1Mofe2EigYNfgqSTPbK4k1iJTxyCEEQCEC", "Kryptex"),
    ("t1XQZdZMnzXBcL8yx2PR27dSNrqctgwLgux", "Luxor"),
    ("t1egMFNkP7EfkK25y8s4GeiMkEGnqcMnTb1", "Mining Dutch"),
    ("t1SEgZvXCu3ceE42qrq5pCeSq7HbLjX8NJv", "NiceHash"),
    ("t1fpcZ2Dbwn4oj35oWBTUhtmUciSq7HG7LU", "Solopool"),
    ("t1Na7ykQ6vE4CbxBPuUDUQx5n6aEWXu1VQq", "Binance Pool"),
    ("t1e6hceYHkzCbwcwGZzKeMfXXW7x7gr19Cw", "Poolin"),
    ("t3cFfPt1Bcvgez9ZbMBFWeZsskxTkPzGCow", "Dev Fund"),
];

/// Returns the Cipherscan pool identity for a known payout address.
pub(crate) fn get_pool_name(address: Option<&str>) -> Option<&'static str> {
    address
        .filter(|address| !address.is_empty())
        .and_then(|address| {
            POOL_NAME_BY_ADDRESS
                .iter()
                .find_map(|(known_address, name)| (*known_address == address).then_some(*name))
        })
}

#[cfg(test)]
mod tests {
    use super::get_pool_name;

    #[test]
    fn known_address_returns_pool_name() {
        assert_eq!(
            get_pool_name(Some("t1SqwRAAdSig6dE4EBPLonAait219VmkUjP")),
            Some("Foundry USA")
        );
    }

    #[test]
    fn duplicate_addresses_return_the_same_pool_name() {
        assert_eq!(
            get_pool_name(Some("t1ZVi2YGk98tEGYcNpXYnJFWCoLG2oYwv3J")),
            get_pool_name(Some("t1L2b66MXbgpVMXDfUa94GCBFAN4dCxGohM"))
        );
        assert_eq!(
            get_pool_name(Some("t1ZVi2YGk98tEGYcNpXYnJFWCoLG2oYwv3J")),
            Some("AntPool")
        );
    }

    #[test]
    fn unknown_address_returns_none() {
        assert_eq!(get_pool_name(Some("t1unknown")), None);
    }

    #[test]
    fn absent_address_returns_none() {
        assert_eq!(get_pool_name(None), None);
        assert_eq!(get_pool_name(Some("")), None);
    }
}
