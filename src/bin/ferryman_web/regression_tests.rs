use super::*;
use axum::extract::{Path, State};

async fn state_at(base: &FsPath) -> AppState {
    let store = JobStore::open(base.join("jobs.sqlite3")).await.unwrap();
    AppState {
        active_jobs: Arc::new(RwLock::new(HashMap::new())),
        store: store.clone(),
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        job_locks: Arc::default(),
        queue: mpsc::channel(100).0,
        request_limiters: Arc::new(HashMap::from([(
            Preset::SevenBFp8,
            Arc::new(Semaphore::new(1)),
        )])),
        translation_client: reqwest::Client::new(),
        config: Arc::new(Config {
            data_dir: base.join("data"),
            user_documents_dir: base.join("documents"),
            remote_fs_dir: base.join("remote"),
            agent_url: String::new(),
            agent_token: "audit-only-token".into(),
            agent_pairing: false,
            allow_local_user: true,
            client: reqwest::Client::new(),
        }),
        persister: JobPersister::spawn(store),
    }
}
fn entry_at(base: &FsPath, status: JobStatus) -> JobEntry {
    let dir = base.join("job");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("input.txt"), "").unwrap();
    JobEntry {
        owner: "alice".into(),
        input: dir.join("input.txt"),
        output: dir.join("result.txt"),
        dir,
        save_to: None,
        save_root: None,
        overwrite: false,
        record: JobRecord {
            id: Uuid::new_v4(),
            filename: "x.txt".into(),
            preset: Preset::SevenBFp8,
            target: "中文".into(),
            mode: OutputMode::Bilingual,
            status,
            total: 0,
            completed: 0,
            translated: 0,
            failed_segments: 0,
            error: None,
            settings: TranslationSettings::default(),
            result_available: false,
            source_path: None,
            source_storage: None,
            save_path: None,
            save_storage: None,
            created_at: 1,
            updated_at: 1,
        },
    }
}
fn identity() -> UserIdentity {
    UserIdentity {
        uid: "alice".into(),
        owner: "alice".into(),
    }
}

#[tokio::test]
async fn cancellation_and_claim_cannot_resurrect_a_job() {
    let temp = tempfile::tempdir().unwrap();
    let state = state_at(temp.path()).await;
    let entry = entry_at(temp.path(), JobStatus::Queued);
    let id = entry.record.id;
    state.store.insert(entry.clone(), 100).await.unwrap();
    state.active_jobs.write().await.insert(id, entry);
    let (claim, cancel) = tokio::join!(
        claim_queued_job(&state, id),
        jobs_api::cancel_job(State(state.clone()), identity(), Path(id))
    );
    assert_eq!(cancel.status(), StatusCode::OK);
    let row = state.store.get("alice".into(), id).await.unwrap().unwrap();
    assert_eq!(row.record.status, JobStatus::Cancelled);
    if claim.is_some() {
        assert!(state
            .cancellations
            .lock()
            .await
            .get(&id)
            .unwrap()
            .is_cancelled());
    }
    assert!(claim_queued_job(&state, id).await.is_none());
}

#[tokio::test]
async fn save_commit_rejects_cancellation_and_preserves_state() {
    let temp = tempfile::tempdir().unwrap();
    let state = state_at(temp.path()).await;
    let entry = entry_at(temp.path(), JobStatus::Writing);
    let id = entry.record.id;
    state.store.insert(entry.clone(), 100).await.unwrap();
    state.active_jobs.write().await.insert(id, entry);
    let token = CancellationToken::new();
    state.cancellations.lock().await.insert(id, token.clone());
    let response = jobs_api::cancel_job(State(state.clone()), identity(), Path(id)).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!token.is_cancelled());
    assert_eq!(
        state
            .store
            .get("alice".into(), id)
            .await
            .unwrap()
            .unwrap()
            .record
            .status,
        JobStatus::Writing
    );
}

#[tokio::test]
async fn sweep_rechecks_retry_before_removing_files() {
    let temp = tempfile::tempdir().unwrap();
    let state = state_at(temp.path()).await;
    let entry = entry_at(temp.path(), JobStatus::Failed);
    let id = entry.record.id;
    state.store.insert(entry.clone(), 100).await.unwrap();
    let transition = lock_job(&state, id).await;
    let worker = state.clone();
    let sweep = tokio::spawn(async move {
        sweep_terminal_jobs_at(&worker, Duration::from_secs(10), 1000).await;
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .job_locks
                .lock()
                .await
                .get(&id)
                .unwrap()
                .strong_count()
                >= 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Commit the retry while holding its transition lock, after sweep selected
    // the old terminal row and started waiting for the same lock.
    let retry = state
        .store
        .retry_incomplete("alice".into(), id, 1000, 100)
        .await
        .unwrap();
    assert!(matches!(retry, job_store::RetryJobOutcome::Retried(_)));
    drop(transition);
    sweep.await.unwrap();
    assert!(entry.input.exists());
    assert_eq!(
        state
            .store
            .get("alice".into(), id)
            .await
            .unwrap()
            .unwrap()
            .record
            .status,
        JobStatus::Queued
    );
}

#[tokio::test]
async fn batch_publication_rolls_back_and_replays_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let state = state_at(temp.path()).await;
    let entry = entry_at(temp.path(), JobStatus::Queued);
    let id = Uuid::new_v4();
    let receipt = Some(("alice".into(), id, "request".into(), "response".into()));
    assert!(state
        .store
        .insert_batch(vec![entry.clone(), entry.clone()], 100, receipt.clone())
        .await
        .is_err());
    assert_eq!(state.store.count_active("alice".into()).await.unwrap(), 0);
    assert!(state
        .store
        .submission("alice".into(), id, "request".into())
        .await
        .unwrap()
        .is_none());
    state
        .store
        .insert_batch(vec![entry], 100, receipt)
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .submission("alice".into(), id, "request".into())
            .await
            .unwrap()
            .as_deref(),
        Some("response")
    );
    assert!(state
        .store
        .submission("alice".into(), id, "different".into())
        .await
        .is_err());
    assert!(state
        .store
        .submission("bob".into(), id, "request".into())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn batch_api_replays_original_jobs_after_sources_change() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state_at(temp.path()).await;
    let (queue, mut receiver) = mpsc::channel(10);
    state.queue = queue;
    let books = state.config.user_documents_dir.join("alice/Books");
    tokio::fs::create_dir_all(&books).await.unwrap();
    tokio::fs::write(books.join("book.txt"), "First book")
        .await
        .unwrap();
    let payload = serde_json::json!({
        "request_id": Uuid::new_v4(),
        "sources": [{"storage": "documents", "path": "Books"}],
        "save_strategy": "sibling_suffix", "preset": "7b-fp8",
        "target": "中文", "mode": "bilingual"
    });
    let mut responses = Vec::new();
    for _ in 0..2 {
        let response = jobs_api::create_directory_jobs(
            State(state.clone()),
            identity(),
            axum::Json(serde_json::from_value(payload.clone()).unwrap()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        responses.push(
            axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        );
        tokio::fs::write(books.join("another.txt"), "Another book")
            .await
            .unwrap();
    }
    assert_eq!(responses[0], responses[1]);
    assert_eq!(state.store.count_active("alice".into()).await.unwrap(), 1);
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_err());
}
