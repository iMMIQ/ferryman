//! Queue-to-worker scheduling: picks queued jobs and dispatches them with
//! preset/output exclusivity constraints.

use super::{claim_queued_job, mutate_job, AppState, JobStatus, MAX_ACTIVE_JOBS};
use crate::runner::run_job;
use ferryman::preset::Preset;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::warn;
use uuid::Uuid;

async fn process_queued_job(state: AppState, id: Uuid) {
    let Some(entry) = claim_queued_job(&state, id).await else {
        return;
    };
    if let Err(error) = run_job(&state, entry).await {
        warn!(%id, %error, "job failed");
        let cancelled = state
            .active_jobs
            .read()
            .await
            .get(&id)
            .is_some_and(|entry| entry.record.status == JobStatus::Cancelled);
        if !cancelled {
            mutate_job(&state, id, |job| {
                job.status = JobStatus::Failed;
                job.error = Some(format!("{error:#}"));
            })
            .await;
        }
    }
    state.active_jobs.write().await.remove(&id);
}

async fn queued_job_schedule(state: &AppState, id: Uuid) -> Option<(Preset, Option<PathBuf>)> {
    state
        .active_jobs
        .read()
        .await
        .get(&id)
        .filter(|entry| entry.record.status == JobStatus::Queued)
        .map(|entry| (entry.record.preset, entry.save_to.clone()))
}

fn can_dispatch_job<'a>(
    active_preset: Option<Preset>,
    active_outputs: impl IntoIterator<Item = &'a PathBuf>,
    preset: Preset,
    output: Option<&PathBuf>,
) -> bool {
    if active_preset.is_some_and(|current| current != preset) {
        return false;
    }
    !output.is_some_and(|candidate| active_outputs.into_iter().any(|active| active == candidate))
}

pub(super) async fn job_worker(state: AppState, mut queue: mpsc::Receiver<Uuid>) {
    let mut pending = VecDeque::new();
    let mut active = JoinSet::new();
    let mut active_outputs = HashMap::new();
    let mut active_preset = None;
    let mut queue_open = true;

    loop {
        while active.len() < MAX_ACTIVE_JOBS {
            // Scan for the first dispatchable job instead of only the queue
            // head: a 30B job waiting at the head must not starve every 7B
            // job behind it (and vice versa) while the active preset differs.
            let mut next = None;
            let mut stale = Vec::new();
            for (index, id) in pending.iter().enumerate() {
                match queued_job_schedule(&state, *id).await {
                    None => stale.push(index),
                    Some((preset, output)) => {
                        if can_dispatch_job(
                            active_preset,
                            active_outputs.values(),
                            preset,
                            output.as_ref(),
                        ) {
                            next = Some((index, preset, output));
                            break;
                        }
                    }
                }
            }
            // Drop entries that left the queue while waiting (cancelled,
            // deleted) — newest first so earlier indexes stay valid.
            for &index in stale.iter().rev() {
                pending.remove(index);
            }
            let Some((index, preset, output)) = next else {
                break;
            };
            let Some(id) = pending.remove(index) else {
                continue;
            };

            active_preset = Some(preset);
            let job_state = state.clone();
            let task = active.spawn(async move {
                process_queued_job(job_state, id).await;
            });
            if let Some(output) = output {
                active_outputs.insert(task.id(), output);
            }
        }

        if !queue_open && pending.is_empty() && active.is_empty() {
            break;
        }

        tokio::select! {
            next = queue.recv(), if queue_open => {
                match next {
                    Some(id) => pending.push_back(id),
                    None => queue_open = false,
                }
            }
            result = active.join_next_with_id(), if !active.is_empty() => {
                match result {
                    Some(Ok((task_id, ()))) => {
                        active_outputs.remove(&task_id);
                    }
                    Some(Err(error)) => {
                        active_outputs.remove(&error.id());
                        warn!(%error, "job task stopped unexpectedly");
                    }
                    None => {}
                }
                if active.is_empty() {
                    active_preset = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_only_combines_compatible_jobs() {
        let first = PathBuf::from("documents/result-a.txt");
        let second = PathBuf::from("documents/result-b.txt");
        let active_outputs = [first.clone()];

        assert!(can_dispatch_job(
            Some(Preset::SevenBFp8),
            active_outputs.iter(),
            Preset::SevenBFp8,
            Some(&second),
        ));
        assert!(!can_dispatch_job(
            Some(Preset::SevenBFp8),
            active_outputs.iter(),
            Preset::ThirtyBFp8,
            Some(&second),
        ));
        assert!(!can_dispatch_job(
            Some(Preset::SevenBFp8),
            active_outputs.iter(),
            Preset::SevenBFp8,
            Some(&first),
        ));
    }
}
