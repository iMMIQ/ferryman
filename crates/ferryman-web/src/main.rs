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
#[cfg(test)]
use std::path::Path as FsPath;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "lazycat")]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod crypto;
mod documents;
mod events;
mod job_store;
mod jobs_api;
mod models;
mod persister;
mod runner;
mod scheduler;
mod storage;
mod sweeper;

use events::JobEventHub;
use job_store::JobStore;
use persister::JobPersister;
use storage::{
    normalize_relative_path, path_for_api, resolve_storage_path, SaveStrategy, StorageKind,
};
#[cfg(test)]
use storage::{storage_root, user_storage_root};
#[cfg(test)]
use sweeper::sweep_terminal_jobs_at;
use sweeper::{
    evict_user_caches, sweep_terminal_jobs, DEFAULT_CACHE_MAX_BYTES, DEFAULT_CACHE_TTL_SECONDS,
};

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
/// Terminal jobs (completed/failed/cancelled) older than this are swept: job
/// directory and DB row both go. 0 disables the sweep via env
/// `FERRYMAN_JOB_RETENTION_SECONDS`.
const DEFAULT_JOB_RETENTION_SECONDS: u64 = 7 * 24 * 3600;
const JOB_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
const JOB_SWEEP_BATCH: usize = 500;

#[derive(Clone)]
struct AppState {
    #[cfg(feature = "lazycat")]
    delegate_tickets: Arc<Mutex<HashMap<String, (HeaderValue, Instant)>>>,
    active_jobs: Arc<RwLock<HashMap<Uuid, JobEntry>>>,
    store: JobStore,
    cancellations: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    job_locks: Arc<Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>>,
    queue: mpsc::Sender<Uuid>,
    events: JobEventHub,
    request_limits: Arc<ferryman::model_api::RequestLimits>,
    translation_client: reqwest::Client,
    config: Arc<Config>,
    persister: JobPersister,
}

struct Config {
    data_dir: PathBuf,
    user_documents_dir: PathBuf,
    remote_fs_dir: PathBuf,
    discovery_url: String,
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
#[derive(ts_rs::TS)]
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

#[derive(Clone, Debug, Deserialize, Serialize, ts_rs::TS)]
struct JobRecord {
    id: Uuid,
    filename: String,
    preset: Preset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    model: Option<ferryman::model_api::ModelTarget>,
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

async fn require_cache_revalidation(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

impl axum::extract::FromRequestParts<AppState> for UserIdentity {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        user_identity(&parts.headers, state.config.allow_local_user)
            .map_err(|status| json_error(status, "missing or invalid user identity"))
    }
}

#[cfg(feature = "lazycat")]
async fn capture_delegate_ticket(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if let (Ok(identity), Some(ticket)) = (
        user_identity(request.headers(), state.config.allow_local_user),
        request.headers().get("x-hc-user-ticket"),
    ) {
        let mut tickets = state.delegate_tickets.lock().await;
        tickets.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(900));
        if tickets.len() < 1024 || tickets.contains_key(&identity.owner) {
            let mut ticket = ticket.clone();
            ticket.set_sensitive(true);
            tickets.insert(identity.owner, (ticket, Instant::now()));
        }
    }
    next.run(request).await
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
    let owner = hex::encode(&digest[..16]);
    Ok(UserIdentity { uid, owner })
}

async fn mutate_job<F>(state: &AppState, id: Uuid, mutate: F) -> Option<JobRecord>
where
    F: FnOnce(&mut JobRecord),
{
    // Mutate in memory and enqueue persistence while the lock is held (the
    // channel send is sync and never blocks), then drop the lock before
    // awaiting the disk write. Status/error/result mutations wait for the
    // write to land; progress-only mutations are throttled and fire-and-forget.
    let (owner, record, done) = {
        let mut jobs = state.active_jobs.write().await;
        let entry = jobs.get_mut(&id)?;
        let before = entry.record.clone();
        mutate(&mut entry.record);
        let progress_only = entry.record.status == before.status
            && entry.record.error == before.error
            && entry.record.result_available == before.result_available;
        entry.record.updated_at = now_epoch_seconds();
        let mut done = None;
        if progress_only {
            if state.persister.progress_write_due(id) {
                state.persister.enqueue(entry.clone(), None);
            }
        } else {
            let (sender, rx) = oneshot::channel();
            done = Some(rx);
            state.persister.enqueue(entry.clone(), Some(sender));
        }
        (entry.owner.clone(), entry.record.clone(), done)
    };
    if let Some(done) = done {
        let _ = done.await;
    }
    state.events.emit_updated(&owner, &record);
    Some(record)
}

// One transition lock per job; weak entries do not keep completed jobs alive.
async fn lock_job(state: &AppState, id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = state.job_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&id).and_then(std::sync::Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(id, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

async fn claim_queued_job(state: &AppState, id: Uuid) -> Option<JobEntry> {
    let _transition = lock_job(state, id).await;
    // Serialize claim with cancellation, including persistence and token registration.
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
            state
                .cancellations
                .lock()
                .await
                .insert(id, CancellationToken::new());
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

#[derive(Serialize, ts_rs::TS)]
struct TranslationLimits {
    max_batch_size: usize,
    max_context_segments: usize,
}
#[derive(Serialize, ts_rs::TS)]
struct ClientConfig {
    formats: Vec<&'static str>,
    storages: Vec<StorageKind>,
    max_upload_bytes: u64,
    translation_defaults: TranslationSettings,
    translation_limits: TranslationLimits,
}
async fn config() -> Json<ClientConfig> {
    Json(ClientConfig {
        formats: vec![
            "epub", "docx", "pdf", "srt", "vtt", "ass", "ssa", "lrc", "txt", "md",
        ],
        storages: vec![StorageKind::Documents, StorageKind::RemoteFs],
        max_upload_bytes: MAX_UPLOAD_BYTES,
        translation_defaults: TranslationSettings::default(),
        translation_limits: TranslationLimits {
            max_batch_size: MAX_WEB_BATCH_SIZE,
            max_context_segments: MAX_WEB_CONTEXT_SEGMENTS,
        },
    })
}

#[cfg(test)]
#[test]
#[ignore = "run via bun run types:generate or types:check"]
fn export_frontend_types() {
    use ts_rs::TS;
    let config = ts_rs::Config::new().with_large_int("number").with_out_dir(
        std::env::var_os("FERRYMAN_TYPES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/generated")
            }),
    );
    ClientConfig::export_all(&config).unwrap();
    models::ModelsResponse::export_all(&config).unwrap();
    models::CustomModelRequest::export_all(&config).unwrap();
    models::ProbeRequest::export_all(&config).unwrap();
    models::ProbeResponse::export_all(&config).unwrap();
}

async fn healthz(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    match state.store.ping().await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(error) => {
            error!(%error, "health check failed");
            json_error(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutdown signal received");
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
    let web_dir =
        PathBuf::from(env::var("FERRYMAN_WEB_DIR").unwrap_or_else(|_| "./dist/web".into()));
    tokio::fs::create_dir_all(&data_dir).await?;
    #[cfg(feature = "lazycat")]
    let discovery_url = env::var("FERRYMAN_MODEL_DISCOVERY_URL")
        .unwrap_or_else(|_| "http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments".into());
    #[cfg(not(feature = "lazycat"))]
    let discovery_url = String::new();
    let allow_local_user = env::var("FERRYMAN_ALLOW_LOCAL_USER")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let translation_client =
        build_translation_client(Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS), None)?;
    if allow_local_user {
        for root in [&user_documents_dir, &remote_fs_dir] {
            tokio::fs::create_dir_all(root.join("local-development-user")).await?;
        }
    }
    let runtime_config = Arc::new(Config {
        data_dir: data_dir.clone(),
        user_documents_dir,
        remote_fs_dir,
        discovery_url,
        allow_local_user,
        client,
    });
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
        #[cfg(feature = "lazycat")]
        delegate_tickets: Arc::default(),
        active_jobs: Arc::new(RwLock::new(active_jobs)),
        store,
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        job_locks: Arc::default(),
        queue,
        events: JobEventHub::default(),
        persister,
        request_limits: Arc::default(),
        translation_client,
        config: runtime_config,
    };
    let worker_handle = tokio::spawn(scheduler::job_worker(state.clone(), receiver));
    for id in pending {
        state.queue.send(id).await.ok();
    }

    // Retention sweep for terminal jobs, submissions, and per-user translation caches.
    let retention_seconds = env::var("FERRYMAN_JOB_RETENTION_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_JOB_RETENTION_SECONDS);
    let cache_max_bytes = env::var("FERRYMAN_CACHE_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_MAX_BYTES);
    let cache_ttl_seconds = env::var("FERRYMAN_CACHE_TTL_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECONDS);
    let sweep_state = state.clone();
    let sweep_handle = tokio::spawn(async move {
        let retention = Duration::from_secs(retention_seconds);
        let cache_ttl = Duration::from_secs(cache_ttl_seconds);
        loop {
            if retention_seconds > 0 {
                sweep_terminal_jobs(&sweep_state, retention).await;
            }
            if cache_max_bytes > 0 || cache_ttl_seconds > 0 {
                evict_user_caches(&sweep_state.config.data_dir, cache_max_bytes, cache_ttl).await;
            }
            tokio::time::sleep(JOB_SWEEP_INTERVAL).await;
        }
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/healthz", get(healthz))
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
        .route("/api/jobs/events", get(events::job_events))
        .route("/api/jobs/directory", post(jobs_api::create_directory_jobs))
        .route(
            "/api/jobs/{id}",
            get(jobs_api::get_job).delete(jobs_api::delete_job),
        )
        .route("/api/jobs/selection", post(jobs_api::create_directory_jobs))
        .route(
            "/api/jobs/selection/preview",
            post(jobs_api::preview_directory_jobs),
        )
        .route("/api/jobs/{id}/cancel", post(jobs_api::cancel_job))
        .route("/api/jobs/{id}/retry", post(jobs_api::retry_job))
        .route("/api/jobs/{id}/result", get(jobs_api::download_result))
        .route("/api/models", get(models::list).post(models::save))
        .route("/api/models/probe", post(models::probe))
        .route("/api/models/{id}", axum::routing::delete(models::delete));
    #[cfg(feature = "lazycat")]
    let app = app.route("/api/models/discover", post(models::discover));
    let app = app
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
        .layer(middleware::from_fn(require_cache_revalidation));
    #[cfg(feature = "lazycat")]
    let app = app.layer(middleware::from_fn_with_state(
        state.clone(),
        capture_delegate_ticket,
    ));
    let shutdown_state = state.clone();
    let app = app.layer(TraceLayer::new_for_http()).with_state(state);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!(%listen, "Ferryman Web listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Cancel all active jobs and drain durable writes before process exit.
    sweep_handle.abort();
    {
        let tokens = shutdown_state.cancellations.lock().await;
        for token in tokens.values() {
            token.cancel();
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), worker_handle).await;
    shutdown_state.persister.flush().await;
    info!("Ferryman Web shut down cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn config_exposes_document_formats_without_a_fixed_model_whitelist() {
        let Json(config) = config().await;
        assert!(config.formats.contains(&"txt"));
        assert!(config.max_upload_bytes > 0);
    }

    #[test]
    fn job_records_receive_optional_field_defaults() {
        let record: JobRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "filename": "notes.txt",
            "preset": "index-translate-2b",
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
            #[cfg(feature = "lazycat")]
            delegate_tickets: Arc::default(),
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            store: store.clone(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            job_locks: Arc::default(),
            queue: mpsc::channel(1).0,
            events: JobEventHub::default(),
            request_limits: Arc::default(),
            translation_client: reqwest::Client::new(),
            config: Arc::new(Config {
                data_dir: base.join("data"),
                user_documents_dir: base.join("documents"),
                remote_fs_dir: base.join("remote-fs"),
                discovery_url: String::new(),
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
                preset: Preset::Index2B,
                model: None,
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
                    preset: Preset::Index2B,
                    model: None,
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

    #[tokio::test]
    async fn identity_extractor_rejects_missing_and_invalid_users() {
        let root = tempfile::tempdir().unwrap();
        let mut state = persister_test_state(root.path()).await;
        Arc::get_mut(&mut state.config).unwrap().allow_local_user = false;
        let app = Router::new()
            .route(
                "/identity",
                get(|identity: UserIdentity| async move { identity.uid }),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/identity", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(&url)
                .header("safe_uid", "../escape")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .get(&url)
                .header("safe_uid", "alice")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "alice"
        );
        assert_eq!(
            client
                .get(&url)
                .header("x-hc-user-id", "bob")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "bob"
        );
        server.abort();
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
            discovery_url: String::new(),
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
            discovery_url: String::new(),
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

#[cfg(test)]
mod regression_tests;
