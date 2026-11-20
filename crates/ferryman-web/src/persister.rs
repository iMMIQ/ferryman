//! Asynchronous ordered job state persistence off the worker critical path.

use super::job_store::JobStore;
use super::JobEntry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::error;
use uuid::Uuid;

/// Minimum spacing between disk writes of pure-progress updates. A translating
/// job ticks its counters every 350 ms; the terminal update that follows always
/// carries the final counters, so skipping intermediate writes loses nothing
/// that a crash-recovery requeue wouldn't reset anyway.
pub(crate) const PROGRESS_WRITE_INTERVAL: Duration = Duration::from_secs(3);

pub(crate) enum PersistCommand {
    Save {
        entry: Box<JobEntry>,
        /// Set for status/error changes: the sender awaits the reply so callers
        /// observe `mutate_job` as durable, without holding any lock across the
        /// database write. Progress writes are fire-and-forget.
        done: Option<oneshot::Sender<()>>,
    },
    Flush(oneshot::Sender<()>),
}

/// Serializes job persistence off the workers' critical path. `mutate_job`
/// snapshots the entry under the in-memory lock and hands it to a single
/// writer task through an ordered channel — the lock is never held across a
/// database write, yet snapshots still hit the disk in mutation order (the
/// channel send happens while the lock is held, so two concurrent mutations
/// cannot reorder).
#[derive(Clone)]
pub(crate) struct JobPersister {
    tx: mpsc::UnboundedSender<PersistCommand>,
    last_progress_write: Arc<StdMutex<HashMap<Uuid, Instant>>>,
}

impl JobPersister {
    pub(crate) fn spawn(store: JobStore) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<PersistCommand>();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    PersistCommand::Save { entry, done } => {
                        if let Err(error) = store.update(*entry.clone()).await {
                            error!(
                                id = %entry.record.id,
                                %error, "persist job state"
                            );
                        }
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                    PersistCommand::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            tx,
            last_progress_write: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(crate) fn enqueue(&self, entry: JobEntry, done: Option<oneshot::Sender<()>>) {
        let _ = self.tx.send(PersistCommand::Save {
            entry: Box::new(entry),
            done,
        });
    }

    /// Wait until all previously enqueued writes have been committed to SQLite.
    pub(crate) async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(PersistCommand::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Whether a progress-only write for `id` should go out now (and record
    /// the decision). Sync + brief: called with the `active_jobs` lock held.
    pub(crate) fn progress_write_due(&self, id: Uuid) -> bool {
        let mut last = self.last_progress_write.lock().unwrap();
        match last.get(&id) {
            Some(at) if at.elapsed() < PROGRESS_WRITE_INTERVAL => false,
            _ => {
                last.insert(id, Instant::now());
                true
            }
        }
    }
}
