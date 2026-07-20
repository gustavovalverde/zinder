//! Spawned-task handle that aborts its task on drop, so a cancelled
//! construction pipeline cannot leave detached work running.

use std::future::Future;

use tokio::task::{JoinError, JoinHandle};

/// Spawned-task handle that aborts the task when dropped.
///
/// Cancelling an owning pipeline therefore cannot leave detached asynchronous
/// work running. Blocking work must enforce its own lifecycle invariants
/// because Tokio cannot interrupt a task after it starts running.
pub(crate) struct AbortOnDropTask<T> {
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> AbortOnDropTask<T> {
    pub(crate) fn spawn(task: impl Future<Output = T> + Send + 'static) -> Self {
        Self {
            handle: tokio::spawn(task),
        }
    }

    pub(crate) async fn join(mut self) -> Result<T, JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
