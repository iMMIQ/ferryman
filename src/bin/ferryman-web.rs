use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferryman::engine::build_translation_client;
use ferryman::format::OutputMode;
use ferryman::preset::Preset;
use ferryman::settings::{
    TranslationSettings, DEFAULT_REQUEST_TIMEOUT_SECONDS, MAX_WEB_BATCH_SIZE,
    MAX_WEB_CONTEXT_SEGMENTS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[path = "ferryman_web/agent_proxy.rs"]
mod agent_proxy;
#[path = "ferryman_web/documents.rs"]
mod documents;
#[path = "ferryman_web/job_store.rs"]
mod job_store;
#[path = "ferryman_web/jobs_api.rs"]
mod jobs_api;
#[path = "ferryman_web/runner.rs"]
mod runner;
#[path = "ferryman_web/scheduler.rs"]
mod scheduler;

use job_store::JobStore;

const MAX_DIRECTORY_FILES: usize = 1000;
const MAX_ACTIVE_JOBS: usize = 8;
/// Hard cap on the streamed `file` field. The route disables the axum body
/// limit so uploads stream to disk, so this is the real enforcement point.
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// Cap for scalar multipart fields (`preset`, `target`, ...). Without it a
/// client could buffer an unbounded "target" value in memory.
const MAX_TEXT_FIELD_BYTES: usize = 8 * 1024;
const DEFAULT_JOB_PAGE_SIZE: usize = 50;
const MAX_JOB_PAGE_SIZE: usize = 100;
const MAX_USER_NONTERMINAL_JOBS: usize = 2000;
const MAX_MODEL_START_ATTEMPTS: usize = 3;
/// Terminal jobs (completed/failed/cancelled) older than this are swept: job
/// directory and DB row both go. 0 disables the sweep via env
/// `FERRYMAN_JOB_RETENTION_SECONDS`.
const DEFAULT_JOB_RETENTION_SECONDS: u64 = 7 * 24 * 3600;
const JOB_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
const JOB_SWEEP_BATCH: usize = 500;

#[derive(Clone)]
struct AppState {
    active_jobs: Arc<RwLock<HashMap<Uuid, JobEntry>>>,
    store: JobStore,
    cancellations: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    queue: mpsc::Sender<Uuid>,
    request_limiters: Arc<HashMap<Preset, Arc<Semaphore>>>,
    translation_client: reqwest::Client,
    config: Arc<Config>,
    persister: JobPersister,
}

struct Config {
    data_dir: PathBuf,
    user_documents_dir: PathBuf,
    remote_fs_dir: PathBuf,
    agent_url: String,
    agent_token: String,
    allow_local_user: bool,
    client: reqwest::Client,
}

#[derive(Clone)]
struct JobEntry {
    owner: String,
    dir: PathBuf,
    input: PathBuf,
    output: PathBuf,
    save_to: Option<PathBuf>,
    save_root: Option<PathBuf>,
    overwrite: bool,
    record: JobRecord,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JobStatus {
    Queued,
    StartingModel,
    Translating,
    Writing,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::StartingModel => "starting_model",
            Self::Translating => "translating",
            Self::Writing => "writing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn from_str(value: &str) -> std::result::Result<Self, String> {
        match value {
            "queued" => Ok(Self::Queued),
            "starting_model" => Ok(Self::StartingModel),
            "translating" => Ok(Self::Translating),
            "writing" => Ok(Self::Writing),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid job status: {value}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobRecord {
    id: Uuid,
    filename: String,
    preset: Preset,
    target: String,
    mode: OutputMode,
    status: JobStatus,
    total: usize,
    completed: usize,
    translated: usize,
    failed_segments: usize,
    error: Option<String>,
    #[serde(default)]
    settings: TranslationSettings,
    #[serde(default)]
    result_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_storage: Option<StorageKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    save_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    save_storage: Option<StorageKind>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

struct UserIdentity {
    uid: String,
    owner: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
enum StorageKind {
    Documents,
    RemoteFs,
}

impl StorageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Documents => "documents",
            Self::RemoteFs => "remote_fs",
        }
    }

    fn from_str(value: &str) -> std::result::Result<Self, String> {
        match value {
            "documents" => Ok(Self::Documents),
            "remote_fs" => Ok(Self::RemoteFs),
            _ => Err(format!("invalid storage kind: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SaveStrategy {
    SiblingSuffix,
    SiblingOverwrite,
    #[default]
    Directory,
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn json_error(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: error.into(),
        }),
    )
        .into_response()
}

fn agent_response(status: StatusCode, value: serde_json::Value) -> Response {
    if status == StatusCode::NO_CONTENT {
        status.into_response()
    } else {
        (status, Json(value)).into_response()
    }
}

async fn require_cache_revalidation(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn user_owner(headers: &HeaderMap, allow_local: bool) -> Result<String, StatusCode> {
    Ok(user_identity(headers, allow_local)?.owner)
}

fn user_identity(headers: &HeaderMap, allow_local: bool) -> Result<UserIdentity, StatusCode> {
    let uid = headers
        .get("safe_uid")
        .or_else(|| headers.get("x-hc-user-id"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| allow_local.then(|| "local-development-user".to_string()))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if uid == "." || uid == ".." || uid.contains('/') || uid.contains('\\') || uid.contains('\0') {
        return Err(StatusCode::BAD_REQUEST);
    }
    let digest = Sha256::digest(uid.as_bytes());
    let owner = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(UserIdentity { uid, owner })
}

fn normalize_relative_path(value: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in FsPath::new(value).components() {
        match component {
            Component::Normal(part) if !part.is_empty() => normalized.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("invalid document path"),
        }
    }
    Ok(normalized)
}

fn path_for_api(path: &FsPath) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn storage_root(config: &Config, storage: StorageKind) -> &FsPath {
    match storage {
        StorageKind::Documents => &config.user_documents_dir,
        StorageKind::RemoteFs => &config.remote_fs_dir,
    }
}

async fn user_storage_root(
    config: &Config,
    identity: &UserIdentity,
    storage: StorageKind,
) -> Result<PathBuf> {
    let configured_root = storage_root(config, storage);
    let mount_root = tokio::fs::canonicalize(configured_root)
        .await
        .with_context(|| {
            format!(
                "storage mount is unavailable: {}",
                configured_root.display()
            )
        })?;
    let user_root = tokio::fs::canonicalize(mount_root.join(&identity.uid))
        .await
        .with_context(|| format!("storage is unavailable for user {}", identity.uid))?;
    if !user_root.starts_with(&mount_root) {
        anyhow::bail!("user storage escapes its mounted directory");
    }
    Ok(user_root)
}

async fn resolve_storage_path(
    config: &Config,
    identity: &UserIdentity,
    storage: StorageKind,
    value: &str,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let relative = normalize_relative_path(value)?;
    let root = user_storage_root(config, identity, storage).await?;
    let candidate = tokio::fs::canonicalize(root.join(&relative))
        .await
        .with_context(|| format!("open storage path {}", path_for_api(&relative)))?;
    if !candidate.starts_with(&root) {
        anyhow::bail!("storage path escapes its mounted directory");
    }
    Ok((root, candidate, relative))
}

/// Minimum spacing between disk writes of pure-progress updates. A translating
/// job ticks its counters every 350 ms; the terminal update that follows always
/// carries the final counters, so skipping intermediate writes loses nothing
/// that a crash-recovery requeue wouldn't reset anyway.
const PROGRESS_WRITE_INTERVAL: Duration = Duration::from_secs(3);

struct PersistMessage {
    entry: JobEntry,
    /// Set for status/error changes: the sender awaits the reply so callers
    /// observe `mutate_job` as durable, without holding any lock across the
    /// database write. Progress writes are fire-and-forget.
    done: Option<oneshot::Sender<()>>,
}

/// Serializes job persistence off the workers' critical path. `mutate_job`
/// snapshots the entry under the in-memory lock and hands it to a single
/// writer task through an ordered channel — the lock is never held across a
/// database write, yet snapshots still hit the disk in mutation order (the
/// channel send happens while the lock is held, so two concurrent mutations
/// cannot reorder).
#[derive(Clone)]
struct JobPersister {
    tx: mpsc::UnboundedSender<PersistMessage>,
    last_progress_write: Arc<StdMutex<HashMap<Uuid, Instant>>>,
}

impl JobPersister {
    fn spawn(store: JobStore) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<PersistMessage>();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(error) = store.update(message.entry.clone()).await {
                    error!(
                        id = %message.entry.record.id,
                        %error, "persist job state"
                    );
                }
                if let Some(done) = message.done {
                    let _ = done.send(());
                }
            }
        });
        Self {
            tx,
            last_progress_write: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Whether a progress-only write for `id` should go out now (and record
    /// the decision). Sync + brief: called with the `active_jobs` lock held.
    fn progress_write_due(&self, id: Uuid) -> bool {
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

async fn mutate_job<F>(state: &AppState, id: Uuid, mutate: F) -> Option<JobRecord>
where
    F: FnOnce(&mut JobRecord),
{
    // Mutate in memory and enqueue persistence while the lock is held (the
    // channel send is sync and never blocks), then drop the lock before
    // awaiting the disk write. Status/error/result mutations wait for the
    // write to land; progress-only mutations are throttled and fire-and-forget.
    let (record, done) = {
        let mut jobs = state.active_jobs.write().await;
        let entry = jobs.get_mut(&id)?;
        let before = entry.record.clone();
        mutate(&mut entry.record);
        let progress_only = entry.record.status == before.status
            && entry.record.error == before.error
            && entry.record.result_available == before.result_available;
        entry.record.updated_at = now_epoch_seconds();
        let mut done = None;
        let message = if progress_only {
            state
                .persister
                .progress_write_due(id)
                .then(|| PersistMessage {
                    entry: entry.clone(),
                    done: None,
                })
        } else {
            let (sender, rx) = oneshot::channel();
            done = Some(rx);
            Some(PersistMessage {
                entry: entry.clone(),
                done: Some(sender),
            })
        };
        if let Some(message) = message {
            let _ = state.persister.tx.send(message);
        }
        (entry.record.clone(), done)
    };
    if let Some(done) = done {
        let _ = done.await;
    }
    Some(record)
}

async fn claim_queued_job(state: &AppState, id: Uuid) -> Option<JobEntry> {
    // The in-memory check is only a fast gate; SQLite's
    // `UPDATE ... WHERE status='queued'` is the real double-claim guard, so the
    // lock must not be held across the claim.
    if state
        .active_jobs
        .read()
        .await
        .get(&id)
        .is_none_or(|entry| entry.record.status != JobStatus::Queued)
    {
        return None;
    }
    match state.store.claim(id, now_epoch_seconds()).await {
        Ok(Some(entry)) => {
            state.active_jobs.write().await.insert(id, entry.clone());
            Some(entry)
        }
        Ok(None) => None,
        Err(error) => {
            error!(%id, %error, "claim queued job");
            None
        }
    }
}

async fn config() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "presets": ["7b-fp8", "30b-fp8"],
        "formats": ["epub", "docx", "srt", "vtt", "ass", "ssa", "lrc", "txt", "md"],
        "storages": ["documents", "remote_fs"],
        "max_upload_bytes": MAX_UPLOAD_BYTES,
        "translation_defaults": TranslationSettings::default(),
        "translation_limits": {
            "max_batch_size": MAX_WEB_BATCH_SIZE,
            "max_context_segments": MAX_WEB_CONTEXT_SEGMENTS
        }
    }))
}

/// One retention pass: remove the job directories of expired terminal jobs,
/// then their DB rows. Rows whose directory refuses removal keep their row so
/// the next sweep retries instead of orphaning files on disk.
async fn sweep_terminal_jobs(state: &AppState, retention: Duration) {
    sweep_terminal_jobs_at(state, retention, now_epoch_seconds()).await;
}

async fn sweep_terminal_jobs_at(state: &AppState, retention: Duration, now: u64) {
    let cutoff = now.saturating_sub(retention.as_secs());
    let mut swept = 0usize;
    loop {
        let expired = match state.store.expired_terminal(cutoff, JOB_SWEEP_BATCH).await {
            Ok(expired) => expired,
            Err(error) => {
                error!(%error, "list expired terminal jobs");
                return;
            }
        };
        if expired.is_empty() {
            break;
        }
        let mut removed = Vec::with_capacity(expired.len());
        for entry in &expired {
            match tokio::fs::remove_dir_all(&entry.dir).await {
                Ok(()) => removed.push(entry.record.id),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    removed.push(entry.record.id)
                }
                Err(error) => {
                    warn!(
                        id = %entry.record.id,
                        dir = %entry.dir.display(),
                        %error,
                        "remove expired job directory"
                    );
                }
            }
        }
        if removed.is_empty() {
            // Every dir in this batch failed; deleting nothing and looping
            // again would spin forever on the same rows.
            break;
        }
        match state.store.delete_terminal_ids(removed).await {
            Ok(count) => swept += count,
            Err(error) => {
                error!(%error, "delete expired terminal job rows");
                break;
            }
        }
        if expired.len() < JOB_SWEEP_BATCH {
            break;
        }
    }
    if swept > 0 {
        info!(jobs = swept, "swept expired terminal jobs");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let listen = env::var("FERRYMAN_WEB_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data_dir =
        PathBuf::from(env::var("FERRYMAN_DATA_DIR").unwrap_or_else(|_| "./ferryman-data".into()));
    let user_documents_dir = PathBuf::from(
        env::var("FERRYMAN_USER_DOCUMENTS_DIR")
            .or_else(|_| env::var("FERRYMAN_DOCUMENTS_DIR"))
            .unwrap_or_else(|_| "./ferryman-documents".into()),
    );
    let remote_fs_dir = PathBuf::from(
        env::var("FERRYMAN_REMOTE_FS_DIR").unwrap_or_else(|_| "./ferryman-remotefs".into()),
    );
    let web_dir = PathBuf::from(env::var("FERRYMAN_WEB_DIR").unwrap_or_else(|_| "./web".into()));
    let agent_url = env::var("FERRYMAN_AGENT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8090".into())
        .trim_end_matches('/')
        .to_string();
    let agent_token = env::var("FERRYMAN_AGENT_TOKEN")
        .map_err(|_| anyhow::anyhow!("FERRYMAN_AGENT_TOKEN must be set"))?;
    if agent_token.len() < 16 {
        anyhow::bail!("FERRYMAN_AGENT_TOKEN must be at least 16 characters");
    }
    let allow_local_user = env::var("FERRYMAN_ALLOW_LOCAL_USER")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    tokio::fs::create_dir_all(&data_dir).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let translation_client = build_translation_client(
        Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS),
        Some(&agent_token),
    )?;
    let store = JobStore::open(data_dir.join("jobs.sqlite3"))
        .await
        .context("open job database")?;
    let recovered = store
        .recover_nonterminal(now_epoch_seconds())
        .await
        .context("recover queued jobs")?;
    let pending: Vec<Uuid> = recovered.iter().map(|entry| entry.record.id).collect();
    if !pending.is_empty() {
        info!(
            jobs = pending.len(),
            "recovered interrupted jobs from SQLite"
        );
    }
    let active_jobs = recovered
        .into_iter()
        .map(|entry| (entry.record.id, entry))
        .collect();
    let (queue, receiver) = mpsc::channel(256);
    let persister = JobPersister::spawn(store.clone());
    let state = AppState {
        active_jobs: Arc::new(RwLock::new(active_jobs)),
        store,
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        queue,
        persister,
        request_limiters: Arc::new(HashMap::from([
            (
                Preset::SevenBFp8,
                Arc::new(Semaphore::new(Preset::SevenBFp8.config().concurrency)),
            ),
            (
                Preset::ThirtyBFp8,
                Arc::new(Semaphore::new(Preset::ThirtyBFp8.config().concurrency)),
            ),
        ])),
        translation_client,
        config: Arc::new(Config {
            data_dir,
            user_documents_dir,
            remote_fs_dir,
            agent_url,
            agent_token,
            allow_local_user,
            client,
        }),
    };
    tokio::spawn(scheduler::job_worker(state.clone(), receiver));
    for id in pending {
        state.queue.send(id).await.ok();
    }

    // Retention sweep for terminal jobs (dirs + rows). 0 disables.
    let retention_seconds = env::var("FERRYMAN_JOB_RETENTION_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_JOB_RETENTION_SECONDS);
    if retention_seconds > 0 {
        let sweep_state = state.clone();
        tokio::spawn(async move {
            let retention = Duration::from_secs(retention_seconds);
            loop {
                sweep_terminal_jobs(&sweep_state, retention).await;
                tokio::time::sleep(JOB_SWEEP_INTERVAL).await;
            }
        });
    }

    let app = Router::new()
        .route("/api/config", get(config))
        .route("/api/documents", get(documents::list_documents))
        .route(
            "/api/documents/directories",
            post(documents::create_document_directory),
        )
        .route(
            "/api/jobs",
            get(jobs_api::list_jobs)
                .post(jobs_api::create_job)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/jobs/active", get(jobs_api::list_active_jobs))
        .route(
            "/api/jobs/{id}",
            axum::routing::delete(jobs_api::delete_job),
        )
        .route("/api/jobs/directory", post(jobs_api::create_directory_jobs))
        .route("/api/jobs/selection", post(jobs_api::create_directory_jobs))
        .route("/api/jobs/{id}/cancel", post(jobs_api::cancel_job))
        .route("/api/jobs/{id}/retry", post(jobs_api::retry_job))
        .route("/api/jobs/{id}/result", get(jobs_api::download_result))
        .route("/api/runtime", get(agent_proxy::runtime_status))
        .route("/api/runtime/start", post(agent_proxy::runtime_start))
        .route("/api/runtime/stop", post(agent_proxy::runtime_stop))
        .route("/api/models", get(agent_proxy::model_catalog))
        .route(
            "/api/models/{preset}",
            axum::routing::delete(agent_proxy::delete_model),
        )
        .route(
            "/api/models/{preset}/download",
            post(agent_proxy::start_model_download),
        )
        .route(
            "/api/models/{preset}/pause",
            post(agent_proxy::pause_model_download),
        )
        .route(
            "/api/model-sources/benchmark",
            post(agent_proxy::start_source_benchmark).delete(agent_proxy::cancel_source_benchmark),
        )
        .route("/api/storage", get(agent_proxy::model_storage))
        .route(
            "/api/runtime-cache",
            axum::routing::delete(agent_proxy::clear_runtime_cache),
        )
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
        .layer(middleware::from_fn(require_cache_revalidation))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!(%listen, "Ferryman Web listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_assets_use_the_package_version_as_the_cache_key() {
        let index = include_str!("../../web/index.html");
        let version = env!("CARGO_PKG_VERSION");
        assert!(index.contains(&format!("/app.js?v={version}")));
        assert!(index.contains(&format!("/styles.css?v={version}")));
    }

    #[test]
    fn job_records_receive_optional_field_defaults() {
        let record: JobRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "filename": "notes.txt",
            "preset": "7b-fp8",
            "target": "中文",
            "mode": "bilingual",
            "status": "queued",
            "total": 0,
            "completed": 0,
            "translated": 0,
            "failed_segments": 0,
            "error": null,
            "created_at": 1,
            "updated_at": 1
        }))
        .unwrap();

        assert_eq!(record.mode, OutputMode::Bilingual);
        assert_eq!(record.settings, TranslationSettings::default());
        assert!(!record.result_available);
    }

    async fn persister_test_state(base: &FsPath) -> AppState {
        let store = JobStore::open(base.join("jobs.sqlite3")).await.unwrap();
        AppState {
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            store: store.clone(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            queue: mpsc::channel(1).0,
            request_limiters: Arc::new(HashMap::new()),
            translation_client: reqwest::Client::new(),
            config: Arc::new(Config {
                data_dir: base.join("data"),
                user_documents_dir: base.join("documents"),
                remote_fs_dir: base.join("remote-fs"),
                agent_url: String::new(),
                agent_token: String::new(),
                allow_local_user: true,
                client: reqwest::Client::new(),
            }),
            persister: JobPersister::spawn(store),
        }
    }

    #[tokio::test]
    async fn progress_writes_are_throttled_and_terminal_writes_are_durable() {
        let base = env::temp_dir().join(format!("ferryman-persist-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let state = persister_test_state(&base).await;
        let id = Uuid::new_v4();
        let entry = JobEntry {
            owner: "alice".to_string(),
            dir: base.join("job"),
            input: base.join("job/input.txt"),
            output: base.join("job/result.txt"),
            save_to: None,
            save_root: None,
            overwrite: false,
            record: JobRecord {
                id,
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status: JobStatus::Translating,
                total: 100,
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
        };
        state.store.insert(entry.clone(), 10).await.unwrap();
        state.active_jobs.write().await.insert(id, entry);

        // First progress tick lands on disk (the throttle window opens here).
        // Progress writes are fire-and-forget, so poll for the row.
        mutate_job(&state, id, |job| job.completed = 10)
            .await
            .unwrap();
        for _ in 0..100 {
            let row = state
                .store
                .get("alice".to_string(), id)
                .await
                .unwrap()
                .unwrap();
            if row.record.completed == 10 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let row = state
            .store
            .get("alice".to_string(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.record.completed, 10);

        // Within the throttle window a second tick stays in memory only…
        mutate_job(&state, id, |job| job.completed = 20)
            .await
            .unwrap();
        let row = state
            .store
            .get("alice".to_string(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.record.completed, 10);

        // …and the terminal update — awaited by contract — carries the final
        // counters and status to disk in one write.
        mutate_job(&state, id, |job| {
            job.status = JobStatus::Completed;
            job.completed = 100;
            job.result_available = true;
        })
        .await
        .unwrap();
        let row = state
            .store
            .get("alice".to_string(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.record.status, JobStatus::Completed);
        assert_eq!(row.record.completed, 100);
        assert!(row.record.result_available);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn retention_sweep_removes_expired_terminal_dirs_and_rows_only() {
        let base = env::temp_dir().join(format!("ferryman-sweep-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let state = persister_test_state(&base).await;

        // One expired terminal job (dir + row), one fresh terminal job, and
        // one expired-but-nonterminal job — only the first may be swept.
        let old_done = Uuid::new_v4();
        let fresh_done = Uuid::new_v4();
        let old_live = Uuid::new_v4();
        for (id, created, status) in [
            (old_done, 1_u64, JobStatus::Completed),
            (fresh_done, 10_000, JobStatus::Failed),
            (old_live, 1, JobStatus::Cancelled),
        ] {
            let mut entry = JobEntry {
                owner: "alice".to_string(),
                dir: base.join("jobs").join(id.to_string()),
                input: base.join("jobs").join(id.to_string()).join("input.txt"),
                output: base.join("jobs").join(id.to_string()).join("result.txt"),
                save_to: None,
                save_root: None,
                overwrite: false,
                record: JobRecord {
                    id,
                    filename: "notes.txt".to_string(),
                    preset: Preset::SevenBFp8,
                    target: "中文".to_string(),
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
                    created_at: created,
                    updated_at: created,
                },
            };
            tokio::fs::create_dir_all(&entry.dir).await.unwrap();
            state.store.insert(entry.clone(), 10).await.unwrap();
            if status == JobStatus::Cancelled {
                // Nonterminal rows would never reach the sweep, but keep the
                // invariant honest: only terminal statuses are ever listed.
                entry.record.status = JobStatus::Translating;
                state.store.update(entry).await.unwrap();
            }
        }

        // "now" such that updated_at 1 is far past the retention window.
        sweep_terminal_jobs_at(&state, Duration::from_secs(100), 5_000).await;

        assert!(!state
            .store
            .get("alice".to_string(), old_done)
            .await
            .unwrap()
            .is_some());
        assert!(!base.join("jobs").join(old_done.to_string()).exists());
        assert!(state
            .store
            .get("alice".to_string(), fresh_done)
            .await
            .unwrap()
            .is_some());
        assert!(base.join("jobs").join(fresh_done.to_string()).exists());
        assert!(state
            .store
            .get("alice".to_string(), old_live)
            .await
            .unwrap()
            .is_some());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn document_paths_only_accept_relative_components() {
        assert_eq!(
            normalize_relative_path("Books/History").unwrap(),
            PathBuf::from("Books/History")
        );
        assert!(normalize_relative_path("../private").is_err());
        assert!(normalize_relative_path("Books/../../private").is_err());
        assert!(normalize_relative_path("/etc").is_err());
    }

    #[test]
    fn storage_kinds_select_independent_mount_roots() {
        let config = Config {
            data_dir: PathBuf::from("data"),
            user_documents_dir: PathBuf::from("documents"),
            remote_fs_dir: PathBuf::from("remote-fs"),
            agent_url: String::new(),
            agent_token: String::new(),
            allow_local_user: true,
            client: reqwest::Client::new(),
        };
        assert_eq!(
            storage_root(&config, StorageKind::Documents),
            FsPath::new("documents")
        );
        assert_eq!(
            storage_root(&config, StorageKind::RemoteFs),
            FsPath::new("remote-fs")
        );
        assert_eq!(
            serde_json::to_string(&StorageKind::RemoteFs).unwrap(),
            "\"remote_fs\""
        );
    }

    #[tokio::test]
    async fn storage_paths_are_scoped_to_the_authenticated_uid() {
        let base = env::temp_dir().join(format!("ferryman-user-root-test-{}", Uuid::new_v4()));
        let documents = base.join("documents");
        let remote_fs = base.join("remote-fs");
        tokio::fs::create_dir_all(documents.join("alice/Books"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(documents.join("bob/Private"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(remote_fs.join("alice/Drive"))
            .await
            .unwrap();
        let config = Config {
            data_dir: base.join("data"),
            user_documents_dir: documents,
            remote_fs_dir: remote_fs,
            agent_url: String::new(),
            agent_token: String::new(),
            allow_local_user: false,
            client: reqwest::Client::new(),
        };
        let identity = UserIdentity {
            uid: "alice".to_string(),
            owner: "owner".to_string(),
        };

        let (root, directory, relative) =
            resolve_storage_path(&config, &identity, StorageKind::Documents, "Books")
                .await
                .unwrap();
        assert!(root.ends_with("documents/alice"));
        assert!(directory.ends_with("documents/alice/Books"));
        assert_eq!(relative, PathBuf::from("Books"));
        assert!(
            resolve_storage_path(&config, &identity, StorageKind::Documents, "../bob/Private")
                .await
                .is_err()
        );

        let remote_root = user_storage_root(&config, &identity, StorageKind::RemoteFs)
            .await
            .unwrap();
        assert!(remote_root.ends_with("remote-fs/alice"));
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }
}
