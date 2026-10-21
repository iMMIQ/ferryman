//! Private Web ↔ AI Pod protocol for Orinfer's local JSONL runner.
use crate::preset::Preset;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_BATCH_REQUESTS: usize = 2048;
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BatchRequest {
    pub custom_id: String,
    pub body: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BatchSubmission {
    pub preset: Preset,
    pub lease_id: String,
    pub requests: Vec<BatchRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchPhase {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl BatchPhase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BatchPage {
    pub state: BatchPhase,
    pub cursor: u64,
    pub outputs: Vec<Value>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct JsonlBackend {
    pub client: reqwest::Client,
    pub endpoint: String,
    pub preset: Preset,
    pub lease_id: String,
}

type Reply = oneshot::Sender<Result<Value, String>>;
struct Pending {
    bytes: usize,
    body: Value,
    reply: Reply,
}

struct Preparation {
    remaining: AtomicUsize,
    changed: tokio::sync::Notify,
}

pub(crate) struct JsonlSender {
    tx: mpsc::UnboundedSender<Pending>,
    preparation: Arc<Preparation>,
    ready: Option<AtomicBool>,
}

impl Drop for JsonlSender {
    fn drop(&mut self) {
        self.mark_ready();
    }
}

impl JsonlSender {
    pub fn for_unit(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            preparation: self.preparation.clone(),
            ready: Some(AtomicBool::new(false)),
        }
    }

    fn mark_ready(&self) {
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !ready.swap(true, Ordering::AcqRel))
        {
            self.preparation.remaining.fetch_sub(1, Ordering::AcqRel);
            self.preparation.changed.notify_one();
        }
    }

    pub async fn chat(&self, body: Value) -> Result<Value> {
        let bytes = serde_json::to_vec(&body)?.len() + 256;
        if bytes > 32 * 1024 * 1024 {
            return Ok(
                serde_json::json!({"error":{"code":"request_failed", "message":"input prompt exceeds JSONL context length limit"}}),
            );
        }
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Pending { bytes, body, reply })
            .context("JSONL driver stopped")?;
        self.mark_ready();
        result
            .await
            .context("JSONL driver stopped")?
            .map_err(anyhow::Error::msg)
    }
}

impl JsonlBackend {
    pub(crate) fn start(
        &self,
        cancel: CancellationToken,
        units: usize,
    ) -> (JsonlSender, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Pending>();
        let backend = self.clone();
        let preparation = Arc::new(Preparation {
            remaining: AtomicUsize::new(units),
            changed: tokio::sync::Notify::new(),
        });
        let worker_preparation = preparation.clone();
        let worker = tokio::spawn(async move {
            let mut deferred = None;
            loop {
                let first = if let Some(first) = deferred.take() {
                    first
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        first = rx.recv() => match first { Some(first) => first, None => break },
                    }
                };
                let mut bytes = first.bytes;
                let mut pending = vec![first];
                // Coalesce independently prepared units into bounded rounds.
                // Recovery uses new files; an active JSONL stays immutable.
                let deadline = tokio::time::sleep(Duration::from_millis(20));
                tokio::pin!(deadline);
                while pending.len() < MAX_BATCH_REQUESTS {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        next = rx.recv() => match next {
                            Some(next) if bytes + next.bytes > MAX_BATCH_BYTES - 1024 * 1024 => {
                                deferred = Some(next);
                                break;
                            }
                            Some(next) => { bytes += next.bytes; pending.push(next); },
                            None => break,
                        },
                        _ = worker_preparation.changed.notified() => {},
                        _ = &mut deadline, if worker_preparation.remaining.load(Ordering::Acquire) == 0 => break,
                    }
                }
                if cancel.is_cancelled() {
                    break;
                }
                backend.execute(pending, &cancel).await;
            }
        });
        (
            JsonlSender {
                tx,
                preparation,
                ready: None,
            },
            worker,
        )
    }

    async fn execute(&self, pending: Vec<Pending>, cancel: &CancellationToken) {
        let id = Uuid::new_v4();
        let url = format!("{}/batch/jobs/{id}", self.endpoint.trim_end_matches('/'));
        let mut replies = HashMap::new();
        let requests = pending
            .into_iter()
            .enumerate()
            .map(|(i, pending)| {
                let custom_id = format!("request-{i}");
                replies.insert(custom_id.clone(), pending.reply);
                BatchRequest {
                    custom_id,
                    body: pending.body,
                }
            })
            .collect();
        let submission = BatchSubmission {
            preset: self.preset,
            lease_id: self.lease_id.clone(),
            requests,
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("job cancelled")),
            result = self.exchange(&url, &submission, &mut replies) => result,
        };
        // DELETE is idempotent and also cancels a submit whose response was lost.
        // Give the Agent time to drain Orinfer's completed rows and reap its group.
        for _ in 0..3 {
            match self
                .client
                .delete(&url)
                .timeout(Duration::from_secs(30))
                .send()
                .await
            {
                Ok(response)
                    if response.status().is_success() || response.status().as_u16() == 404 =>
                {
                    break
                }
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        let error = result
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "JSONL job finished without a response for this request".into());
        for (_, reply) in replies {
            let _ = reply.send(Err(error.clone()));
        }
    }

    async fn exchange(
        &self,
        url: &str,
        submission: &BatchSubmission,
        replies: &mut HashMap<String, Reply>,
    ) -> Result<()> {
        let mut submitted = false;
        let mut cursor = 0;
        let mut last_contact = Instant::now();
        let mut failed_outputs = HashMap::new();
        loop {
            let response = if submitted {
                self.client
                    .get(url)
                    .query(&[("cursor", cursor)])
                    .timeout(Duration::from_secs(30))
                    .send()
                    .await
            } else {
                self.client
                    .post(url)
                    .json(submission)
                    .timeout(Duration::from_secs(30))
                    .send()
                    .await
            };
            match response {
                Ok(response) if response.status().is_success() => {
                    if !submitted {
                        submitted = true;
                        last_contact = Instant::now();
                        continue;
                    }
                    match response.json::<BatchPage>().await {
                        Ok(page) => {
                            anyhow::ensure!(
                                page.cursor >= cursor,
                                "JSONL output cursor moved backwards"
                            );
                            let advanced = page.cursor > cursor;
                            cursor = page.cursor;
                            for output in page.outputs {
                                let custom_id = output
                                    .get("custom_id")
                                    .and_then(Value::as_str)
                                    .context("JSONL response missing custom_id")?;
                                if !output["error"].is_null() {
                                    failed_outputs.insert(custom_id.to_string(), output);
                                } else if let Some(reply) = replies.remove(custom_id) {
                                    failed_outputs.remove(custom_id);
                                    let _ = reply.send(Ok(output));
                                }
                            }
                            last_contact = Instant::now();
                            // A terminal process may still have more than one page
                            // of committed output. Always drain to EOF first.
                            if page.state.terminal() && !advanced {
                                // Resume may append a successful retry after a
                                // failed row. The last committed result wins.
                                for (id, output) in failed_outputs.drain() {
                                    if let Some(reply) = replies.remove(&id) {
                                        let _ = reply.send(Ok(output));
                                    }
                                }
                                if let Some(error) = page.error {
                                    bail!("{error}");
                                }
                                return Ok(());
                            }
                            if advanced {
                                continue;
                            }
                        }
                        Err(_) => submitted = false,
                    }
                }
                Ok(response) => {
                    let status = response.status();
                    if status.as_u16() == 404 || status.is_server_error() {
                        submitted = false;
                    } else if !matches!(status.as_u16(), 409 | 429 | 408) {
                        bail!(
                            "Agent JSONL request failed ({status}): {}",
                            response.text().await.unwrap_or_default()
                        );
                    }
                    // Queuing behind another document is expected, and remains
                    // cancellable while the Web runner renews its model lease.
                    if status.as_u16() == 409 || status.as_u16() == 429 {
                        last_contact = Instant::now();
                    }
                }
                Err(_) => submitted = false,
            }
            anyhow::ensure!(
                last_contact.elapsed() < Duration::from_secs(300),
                "Agent JSONL unavailable for five minutes"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};

    #[tokio::test]
    async fn reconnection_resubmits_same_input_and_uses_latest_resumed_result() {
        let polls = Arc::new(AtomicUsize::new(0));
        let submissions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = submissions.clone();
        let handler_polls = polls.clone();
        let app = Router::new().route("/batch/jobs/{id}", post(move |Json(body): Json<Value>| {
            let received = received.clone();
            async move { received.lock().unwrap().push(body); axum::http::StatusCode::ACCEPTED }
        }).get(move || {
            use axum::response::IntoResponse;
            let number = handler_polls.fetch_add(1, Ordering::SeqCst);
            async move {
                if number == 1 { return axum::http::StatusCode::NOT_FOUND.into_response(); }
                let (cursor, outputs) = if number == 0 {
                    (1, vec![serde_json::json!({"custom_id":"request-0", "error":{"code":"request_failed","message":"old failure"}})])
                } else if number == 2 {
                    (2, vec![serde_json::json!({"custom_id":"request-0", "error":null,"response":{"status_code":200,"body":{"ok":true}}})])
                } else { (2, vec![]) };
                Json(BatchPage { state: if number == 0 { BatchPhase::Running } else { BatchPhase::Completed }, cursor, outputs, error: None }).into_response()
            }
        }).delete(|| async { axum::http::StatusCode::NO_CONTENT }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let backend = JsonlBackend {
            client: reqwest::Client::new(),
            endpoint,
            preset: Preset::Index2B,
            lease_id: "test".into(),
        };
        let cancel = CancellationToken::new();
        let (sender, worker) = backend.start(cancel.clone(), 1);
        let unit = sender.for_unit();
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            unit.chat(serde_json::json!({"messages":[]})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response["response"]["body"]["ok"], true);
        {
            let submissions = submissions.lock().unwrap();
            assert_eq!(submissions.len(), 2);
            assert_eq!(submissions[0], submissions[1]);
        }
        cancel.cancel();
        worker.await.unwrap();
        server.abort();
    }
}
