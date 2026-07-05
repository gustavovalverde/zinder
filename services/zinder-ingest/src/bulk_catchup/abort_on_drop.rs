use std::future::Future;

use tokio::task::{JoinError, JoinHandle};

/// Spawned-task handle that aborts the task when dropped.
///
/// Cancelling the bulk-catchup future therefore cannot leave detached
/// block-prepare or commit work running. An abort cannot stop an inner
/// `spawn_blocking` store write; the store's commit-order validation rejects
/// a late orphan commit.
pub(super) struct AbortOnDropTask<T> {
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> AbortOnDropTask<T> {
    pub(super) fn spawn(task: impl Future<Output = T> + Send + 'static) -> Self {
        Self {
            handle: tokio::spawn(task),
        }
    }

    pub(super) async fn join(mut self) -> Result<T, JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
