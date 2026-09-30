//! Admission control and lifecycle tracking for hardware jobs.
//!
//! A permit serializes hardware operations. A background job owns its
//! permit until it finishes, including cleanup and completion notification.
//!
//! Shutdown rejects new work but does not abort accepted flash jobs.

use std::{future::Future, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::task::TaskTracker;

pub type Lease = Arc<OwnedSemaphorePermit>;

#[derive(Debug)]
pub struct Jobs {
    gate: Arc<Semaphore>,
    tracker: TaskTracker,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            gate: Arc::new(Semaphore::new(1)),
            tracker: TaskTracker::new(),
        }
    }
}

impl Jobs {
    /// Fail immediately instead of accumulating an unbounded work queue.
    pub fn try_enter(&self) -> Result<Lease, &'static str> {
        self.gate
            .clone()
            .try_acquire_owned()
            .map(Arc::new)
            .map_err(|_| {
                if self.gate.is_closed() {
                    "controller is shutting down"
                } else {
                    "another hardware operation is active"
                }
            })
    }

    pub fn is_closing(&self) -> bool {
        self.gate.is_closed()
    }

    /// Spawn work that already owns an admitted operation lease.
    ///
    /// A lease admitted before shutdown may still be transferred to a job.
    /// main must drain HTTP handlers before calling wait().
    pub fn spawn<F>(&self, lease: Lease, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tracker.spawn(async move {
            let _lease = lease;
            future.await;
        });
    }

    pub fn begin_shutdown(&self) {
        self.gate.close();
    }

    /// Call only after HTTP handlers have drained, so none can add jobs.
    pub async fn wait(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}
