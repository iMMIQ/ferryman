//! Authenticated, bounded spool for the Agent-owned Orinfer batch child.
use super::*;
use axum::extract::Query;
use ferryman::jsonl::{
    BatchPage, BatchPhase, BatchSubmission, MAX_BATCH_BYTES, MAX_BATCH_REQUESTS,
};
use std::collections::HashSet;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
pub(super) struct BatchJobs {
    root: PathBuf,
    jobs: Arc<Mutex<HashMap<Uuid, Arc<BatchJob>>>>,
    removed: Arc<Mutex<HashMap<Uuid, Instant>>>,
}

pub(super) struct BatchJob {
    pub id: Uuid,
    pub preset: Preset,
    pub lease_id: String,
    pub directory: PathBuf,
    pub status: RwLock<(BatchPhase, Option<String>)>,
    last_access: std::sync::Mutex<Instant>,
}

impl BatchJobs {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            jobs: Default::default(),
            removed: Default::default(),
        }
    }

    pub fn start_cleanup(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let mut jobs = store.jobs.lock().await;
                let mut stale = Vec::new();
                for (&id, job) in jobs.iter() {
                    let idle = job.last_access.lock().unwrap().elapsed() > Duration::from_secs(300);
                    if idle && job.status.read().await.0 != BatchPhase::Running {
                        stale.push(id);
                    }
                }
                for id in stale {
                    jobs.remove(&id);
                    let _ = tokio::fs::remove_dir_all(store.root.join(id.to_string())).await;
                }
                // A previous Agent/Web crash may leave an unregistered spool.
                // Keep a day for reconnection/resume, then remove only UUID dirs.
                if let Ok(mut entries) = tokio::fs::read_dir(&store.root).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let Ok(id) = entry.file_name().to_string_lossy().parse::<Uuid>() else {
                            continue;
                        };
                        if jobs.contains_key(&id) {
                            continue;
                        }
                        if let Ok(metadata) = entry.metadata().await {
                            if metadata.is_dir()
                                && metadata
                                    .modified()
                                    .ok()
                                    .and_then(|time| time.elapsed().ok())
                                    .is_some_and(|age| age > Duration::from_secs(86400))
                            {
                                let _ = tokio::fs::remove_dir_all(entry.path()).await;
                            }
                        }
                    }
                }
            }
        });
    }

    async fn prepare(
        &self,
        id: Uuid,
        submission: BatchSubmission,
    ) -> anyhow::Result<Arc<BatchJob>> {
        anyhow::ensure!(
            !submission.requests.is_empty() && submission.requests.len() <= MAX_BATCH_REQUESTS,
            "invalid JSONL request count"
        );
        let mut ids = HashSet::new();
        let mut input = Vec::new();
        for request in &submission.requests {
            anyhow::ensure!(
                !request.custom_id.is_empty()
                    && request.custom_id.len() <= 256
                    && ids.insert(&request.custom_id),
                "invalid or duplicate custom_id"
            );
            anyhow::ensure!(
                request.body.is_object() && request.body["model"] == submission.preset.api_model(),
                "batch model must match the acquired preset"
            );
            anyhow::ensure!(
                request.body["stream"] != true && request.body.get("stream_options").is_none(),
                "batch requests must be non-streaming"
            );
            let offset = input.len();
            serde_json::to_writer(
                &mut input,
                &serde_json::json!({
                    "custom_id": request.custom_id, "method": "POST", "url": "/v1/chat/completions", "body": request.body
                }),
            )?;
            anyhow::ensure!(
                input.len() - offset <= 32 * 1024 * 1024,
                "JSONL line exceeds 32 MiB"
            );
            input.push(b'\n');
            anyhow::ensure!(input.len() <= MAX_BATCH_BYTES, "JSONL input exceeds 64 MiB");
        }
        let bytes = serde_json::to_vec(&submission)?;
        let mut jobs = self.jobs.lock().await;
        anyhow::ensure!(
            !self.removed.lock().await.contains_key(&id),
            "batch has been removed"
        );
        let directory = self.root.join(id.to_string());
        let metadata = directory.join("submission.json");
        match tokio::fs::read(&metadata).await {
            Ok(existing) => anyhow::ensure!(
                existing == bytes,
                "batch ID already belongs to different input"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(jobs.len() < 16, "too many JSONL jobs");
                tokio::fs::create_dir_all(&directory).await?;
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .await?;
                ferryman::atomic_file::write(directory.join("input.jsonl"), input).await?;
                ferryman::atomic_file::write(metadata, bytes).await?;
            }
            Err(error) => return Err(error.into()),
        }
        if let Some(job) = jobs.get(&id) {
            *job.last_access.lock().unwrap() = Instant::now();
            return Ok(job.clone());
        }
        anyhow::ensure!(jobs.len() < 16, "too many JSONL jobs");
        let job = Arc::new(BatchJob {
            id,
            preset: submission.preset,
            lease_id: submission.lease_id,
            directory,
            status: RwLock::new((BatchPhase::Pending, None)),
            last_access: std::sync::Mutex::new(Instant::now()),
        });
        jobs.insert(id, job.clone());
        Ok(job)
    }
}

pub(super) async fn submit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(submission): Json<BatchSubmission>,
) -> Response {
    // Reject before writing document contents unless this caller owns a batch lease.
    match state
        .controller
        .check_batch_lease(submission.preset, &submission.lease_id)
        .await
    {
        Ok(()) => {}
        Err(error) => return failure(StatusCode::CONFLICT, error),
    }
    let job = match state.batches.prepare(id, submission).await {
        Ok(job) => job,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.controller.start_batch(job).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(error) => failure(StatusCode::CONFLICT, error),
    }
}

#[derive(Default, Deserialize)]
pub(super) struct Cursor {
    #[serde(default)]
    cursor: u64,
}

pub(super) async fn poll(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<Cursor>,
) -> Response {
    let job = state.batches.jobs.lock().await.get(&id).cloned();
    let Some(job) = job else {
        return StatusCode::NOT_FOUND.into_response();
    };
    *job.last_access.lock().unwrap() = Instant::now();
    match read_page(&job, query.cursor).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn read_page(job: &BatchJob, cursor: u64) -> anyhow::Result<BatchPage> {
    // Snapshot before reading: a worker may commit its final line and exit while
    // we are at EOF. Reporting Running then guarantees one more poll will drain it.
    let (state, error) = job.status.read().await.clone();
    let mut page = BatchPage {
        state,
        cursor,
        outputs: vec![],
        error,
    };
    let mut file = match tokio::fs::File::open(job.directory.join("output.jsonl")).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(page),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        cursor <= file.metadata().await?.len(),
        "invalid JSONL cursor"
    );
    file.seek(std::io::SeekFrom::Start(cursor)).await?;
    let mut reader = BufReader::new(file);
    for _ in 0..128 {
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let output = serde_json::from_slice(&line)?;
        page.cursor += line.len() as u64;
        page.outputs.push(output);
        if page.cursor - cursor >= 4 * 1024 * 1024 {
            break;
        }
    }
    Ok(page)
}

pub(super) async fn remove(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    // Serialize deletion with preparation, and keep a short tombstone so a
    // delayed POST cannot resurrect a cancelled generation after DELETE returns.
    let mut jobs = state.batches.jobs.lock().await;
    {
        let mut removed = state.batches.removed.lock().await;
        removed.retain(|_, time| time.elapsed() < Duration::from_secs(300));
        removed.insert(id, Instant::now());
    }
    if let Some(job) = jobs.get(&id) {
        *job.status.write().await = (BatchPhase::Cancelled, Some("batch cancelled".into()));
    }
    if let Err(error) = state.controller.cancel_batch(id).await {
        return failure(StatusCode::SERVICE_UNAVAILABLE, error);
    }
    jobs.remove(&id);
    match tokio::fs::remove_dir_all(state.batches.root.join(id.to_string())).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

fn failure(status: StatusCode, error: String) -> Response {
    (status, Json(ErrorBody { error })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_cursor_commits_only_complete_lines_and_drains_multiple_pages() {
        let root = tempfile::tempdir().unwrap();
        let job = BatchJob {
            id: Uuid::new_v4(),
            preset: Preset::Index2B,
            lease_id: "test".into(),
            directory: root.path().into(),
            status: RwLock::new((BatchPhase::Running, None)),
            last_access: std::sync::Mutex::new(Instant::now()),
        };
        let path = root.path().join("output.jsonl");
        tokio::fs::write(&path, b"{\"custom_id\":\"one\"}\n{\"custom_id\":")
            .await
            .unwrap();
        let first = read_page(&job, 0).await.unwrap();
        assert_eq!(first.outputs.len(), 1);
        assert_eq!(first.cursor, 20);
        assert!(read_page(&job, first.cursor)
            .await
            .unwrap()
            .outputs
            .is_empty());
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"\"two\"}\n").await.unwrap();
        for i in 0..300 {
            file.write_all(format!("{{\"custom_id\":\"item-{i}\"}}\n").as_bytes())
                .await
                .unwrap();
        }
        file.flush().await.unwrap();
        *job.status.write().await = (BatchPhase::Completed, None);
        let mut cursor = first.cursor;
        let mut count = 0;
        loop {
            let page = read_page(&job, cursor).await.unwrap();
            assert_eq!(page.state, BatchPhase::Completed);
            if page.cursor == cursor {
                break;
            }
            count += page.outputs.len();
            cursor = page.cursor;
        }
        assert_eq!(count, 301);
    }
}
