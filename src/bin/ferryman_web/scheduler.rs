//! Queue-to-worker scheduling: picks queued jobs and dispatches them with
//! preset/output exclusivity constraints.

use super::{claim_queued_job, mutate_job, AppState, JobStatus, MAX_ACTIVE_JOBS};
use crate::runner::run_job;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::warn;
use uuid::Uuid;

async fn process_queued_job(state: AppState, id: Uuid) {
    let Some(entry) = claim_queued_job(&state, id).await else {
        return;
    };
    let output = entry.output.clone();
    if let Err(error) = run_job(&state, entry).await {
        warn!(%id, %error, "job failed");
        finalize_failed_job(&state, id, &output, &error).await;
    }
    state.active_jobs.write().await.remove(&id);
}

pub(super) async fn finalize_failed_job(
    state: &AppState,
    id: Uuid,
    output: &Path,
    error: &anyhow::Error,
) {
    let available = tokio::fs::metadata(output)
        .await
        .is_ok_and(|metadata| metadata.is_file());
    // Cancellation and error completion are competing terminal transitions.
    // Recheck cancellation under its transition lock after the filesystem await.
    let _transition = super::lock_job(state, id).await;
    mutate_job(state, id, |job| {
        job.result_available = available;
        if job.status != JobStatus::Cancelled {
            job.status = JobStatus::Failed;
            job.error = Some(format!("{error:#}"));
        }
    })
    .await;
}

fn can_dispatch_job<'a>(
    active_outputs: impl IntoIterator<Item = &'a PathBuf>,
    output: Option<&PathBuf>,
) -> bool {
    !output.is_some_and(|candidate| active_outputs.into_iter().any(|active| active == candidate))
}

// Independent external model APIs share only the output-file exclusion.
fn take_dispatchable<K>(
    pending: &mut VecDeque<Uuid>,
    active_outputs: &HashMap<K, PathBuf>,
    mut lookup: impl FnMut(Uuid) -> Option<Option<PathBuf>>,
) -> Option<(Uuid, Option<PathBuf>)> {
    let mut index = 0;
    while let Some(&id) = pending.get(index) {
        match lookup(id) {
            None => {
                pending.remove(index);
            }
            Some(output) if can_dispatch_job(active_outputs.values(), output.as_ref()) => {
                pending.remove(index);
                return Some((id, output));
            }
            Some(_) => index += 1,
        }
    }
    None
}

pub(super) async fn job_worker(state: AppState, mut queue: mpsc::Receiver<Uuid>) {
    let mut pending = VecDeque::new();
    let mut active = JoinSet::new();
    let mut active_outputs = HashMap::new();
    let mut queue_open = true;

    loop {
        while active.len() < MAX_ACTIVE_JOBS {
            // A blocked output must not stall jobs writing elsewhere.
            let next = {
                let jobs = state.active_jobs.read().await;
                take_dispatchable(&mut pending, &active_outputs, |id| {
                    jobs.get(&id)
                        .filter(|entry| entry.record.status == JobStatus::Queued)
                        .map(|entry| entry.save_to.clone())
                })
            };
            let Some((id, output)) = next else {
                break;
            };

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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_jobs_do_not_shift_selected_job_or_output() {
        let cancelled = Uuid::new_v4();
        let blocked = Uuid::new_v4();
        let first = Uuid::new_v4();
        let other = Uuid::new_v4();
        let output = PathBuf::from("chosen.txt");
        let busy = PathBuf::from("busy.txt");
        let jobs = HashMap::from([
            (first, Some(output.clone())),
            (other, None),
            (blocked, Some(busy.clone())),
        ]);
        let mut pending = VecDeque::from([cancelled, blocked, first, other]);
        let active = HashMap::from([(0, busy)]);
        assert_eq!(
            take_dispatchable(&mut pending, &active, |id| jobs.get(&id).cloned()),
            Some((first, Some(output)))
        );
        assert_eq!(
            take_dispatchable(&mut pending, &active, |id| jobs.get(&id).cloned()),
            Some((other, None))
        );
        assert_eq!(pending, VecDeque::from([blocked]));
    }
    #[test]
    fn external_models_only_exclude_conflicting_outputs() {
        let first = PathBuf::from("result-a.txt");
        let second = PathBuf::from("result-b.txt");
        assert!(can_dispatch_job([&first], Some(&second)));
        assert!(can_dispatch_job([&first], None));
        assert!(!can_dispatch_job([&first], Some(&first)));
    }
}
