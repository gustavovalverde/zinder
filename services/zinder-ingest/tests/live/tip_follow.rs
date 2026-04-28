#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{num::NonZeroU32, time::Duration};

use eyre::{Result, eyre};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_ingest::tip_follow;
use zinder_runtime::Readiness;
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live};

use crate::common::{live_tip_follow_config, zebra_source_from_tip_follow};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn tip_follow_advances_to_node_tip() -> Result<()> {
    let _guard = init();
    let env = require_live()?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let tip_follow_config = live_tip_follow_config(
        &env,
        &storage_path,
        100,
        NonZeroU32::new(1).ok_or_else(|| eyre!("invalid test batch size"))?,
        Duration::from_millis(100),
    );
    let source = zebra_source_from_tip_follow(&tip_follow_config)?;
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();
    let cancel_handle = cancel.clone();
    let tip_follow_handle =
        tokio::spawn(
            async move { tip_follow(&tip_follow_config, &source, &readiness, cancel).await },
        );

    tokio::time::sleep(Duration::from_secs(2)).await;
    cancel_handle.cancel();
    tip_follow_handle.await??;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let chain_epoch = store
        .current_chain_epoch()?
        .ok_or_else(|| eyre!("tip-follow did not commit any chain epoch"))?;
    assert_eq!(chain_epoch.network, env.network());
    assert!(
        chain_epoch.tip_height >= BlockHeight::new(1),
        "tip-follow did not advance the visible chain epoch"
    );
    Ok(())
}
