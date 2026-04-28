#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use zinder_client::CHAIN_INDEX_CAPABILITIES;
use zinder_proto::ZINDER_CAPABILITIES;

#[test]
fn typed_chain_index_covers_every_advertised_wallet_capability() {
    let missing_from_chain_index = ZINDER_CAPABILITIES
        .iter()
        .filter(|capability| !CHAIN_INDEX_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    let stale_chain_index_capabilities = CHAIN_INDEX_CAPABILITIES
        .iter()
        .filter(|capability| !ZINDER_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();

    assert!(
        missing_from_chain_index.is_empty(),
        "capabilities without ChainIndex methods: {missing_from_chain_index:?}"
    );
    assert!(
        stale_chain_index_capabilities.is_empty(),
        "ChainIndex capabilities no longer advertised by proto: {stale_chain_index_capabilities:?}"
    );
}
