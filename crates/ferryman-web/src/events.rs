//! Server-Sent Events (SSE) hub for real-time per-user job updates.

use super::{AppState, JobRecord, UserIdentity};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct JobEvent {
    pub(crate) owner: String,
    pub(crate) kind: JobEventKind,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum JobEventKind {
    Updated { job: Box<JobRecord> },
    Deleted { id: Uuid },
}

#[derive(Clone)]
pub(crate) struct JobEventHub {
    tx: broadcast::Sender<JobEvent>,
}

impl Default for JobEventHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self { tx }
    }
}

impl JobEventHub {
    pub(crate) fn emit_updated(&self, owner: &str, job: &JobRecord) {
        let _ = self.tx.send(JobEvent {
            owner: owner.to_string(),
            kind: JobEventKind::Updated {
                job: Box::new(job.clone()),
            },
        });
    }

    pub(crate) fn emit_deleted(&self, owner: &str, id: Uuid) {
        let _ = self.tx.send(JobEvent {
            owner: owner.to_string(),
            kind: JobEventKind::Deleted { id },
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }
}

pub(super) async fn job_events(
    State(state): State<AppState>,
    identity: UserIdentity,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let owner = identity.owner;
    let stream = futures::stream::unfold((rx, owner), |(mut rx, owner)| async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.owner == owner => {
                    if let Ok(data) = serde_json::to_string(&event.kind) {
                        let sse = Event::default().event("job").data(data);
                        return Some((Ok(sse), (rx, owner)));
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let sse = Event::default().event("resync").data("{}");
                    return Some((Ok(sse), (rx, owner)));
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("keep-alive"),
    )
}
