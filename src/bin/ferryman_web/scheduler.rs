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

// Remove stale entries as we scan, so selection and removal use the same index.
fn take_dispatchable<K>(
    pending: &mut VecDeque<Uuid>,
    active_preset: Option<Preset>,
    active_outputs: &HashMap<K, PathBuf>,
    mut lookup: impl FnMut(Uuid) -> Option<(Preset, Option<PathBuf>)>,
) -> Option<(Uuid, Preset, Option<PathBuf>)> {
    let mut index = 0;
    while let Some(&id) = pending.get(index) {
        match lookup(id) {
            None => {
                pending.remove(index);
            }
            Some((preset, output)) => {
                if can_dispatch_job(
                    active_preset,
                    active_outputs.values(),
                    preset,
                    output.as_ref(),
                ) {
                    pending.remove(index);
                    return Some((id, preset, output));
                }
                index += 1;
            }
        }
    }
    None
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
            let next = {
                let jobs = state.active_jobs.read().await;
                take_dispatchable(&mut pending, active_preset, &active_outputs, |id| {
                    jobs.get(&id)
                        .filter(|entry| entry.record.status == JobStatus::Queued)
                        .map(|entry| (entry.record.preset, entry.save_to.clone()))
                })
            };
            let Some((id, preset, output)) = next else {
                break;
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
    fn cancelled_jobs_do_not_shift_selected_job_or_output() {
        let cancelled = Uuid::new_v4();
        let first = Uuid::new_v4();
        let other_model = Uuid::new_v4();
        let blocked = Uuid::new_v4();
        let output = PathBuf::from("chosen.txt");
        let busy = PathBuf::from("busy.txt");
        let jobs = HashMap::from([
            (first, (Preset::SevenBFp8, Some(output.clone()))),
            (other_model, (Preset::ThirtyBFp8, None)),
            (blocked, (Preset::SevenBFp8, Some(busy.clone()))),
        ]);
        let mut pending = VecDeque::from([cancelled, blocked, first, other_model]);
        let active = HashMap::from([(0, busy)]);
        assert_eq!(
            take_dispatchable(&mut pending, Some(Preset::SevenBFp8), &active, |id| jobs
                .get(&id)
                .cloned()),
            Some((first, Preset::SevenBFp8, Some(output)))
        );
        assert_eq!(pending, VecDeque::from([blocked, other_model]));
        assert!(
            take_dispatchable(&mut pending, Some(Preset::SevenBFp8), &active, |id| jobs
                .get(&id)
                .cloned())
            .is_none()
        );
    }

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
