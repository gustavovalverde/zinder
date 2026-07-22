//! Checked-in generated artifact invariants.

use zinder_proto::compat::lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT;
use zinder_proto::external::zebra_indexer_rpc::ZEBRA_INDEXER_PROTOCOL_COMMIT;

#[test]
fn vendored_protocol_commit_constants_are_exact_trimmed_ids() {
    assert_commit_id(
        LIGHTWALLETD_PROTOCOL_COMMIT,
        include_str!("../../proto/compat/lightwalletd/COMMIT").trim(),
    );
    assert_commit_id(
        ZEBRA_INDEXER_PROTOCOL_COMMIT,
        include_str!("../../proto/external/zebra/COMMIT").trim(),
    );
}

fn assert_commit_id(actual: &str, expected: &str) {
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 40);
    assert!(actual.bytes().all(|byte| byte.is_ascii_hexdigit()));
}
