use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferryman::batch::{
    collect_inputs, is_generated_output, run_batch_controlled, suffixed_output_path, BatchOpts,
    BatchProgress, ProgressCallback,
};
use ferryman::cache::Cache;
use ferryman::engine::{build_translation_client, Engine};
use ferryman::format::{Format, OutputMode};
use ferryman::preset::Preset;
use ferryman::settings::{
    TranslationSettings, DEFAULT_REQUEST_TIMEOUT_SECONDS, MAX_WEB_BATCH_SIZE,
    MAX_WEB_CONTEXT_SEGMENTS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch, Mutex, RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[path = "ferryman_web/job_store.rs"]
mod job_store;

use job_store::{JobCursor, JobStore, RetryJobOutcome};

const MAX_DIRECTORY_FILES: usize = 1000;
const MAX_ACTIVE_JOBS: usize = 8;
const DEFAULT_JOB_PAGE_SIZE: usize = 50;
const MAX_JOB_PAGE_SIZE: usize = 100;
const MAX_USER_NONTERMINAL_JOBS: usize = 2000;
const MAX_MODEL_START_ATTEMPTS: usize = 3;

#[derive(Clone)]
struct AppState {
    active_jobs: Arc<RwLock<HashMap<Uuid, JobEntry>>>,
    store: JobStore,
    cancellations: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    queue: mpsc::Sender<Uuid>,
    request_limiters: Arc<HashMap<Preset, Arc<Semaphore>>>,
    translation_client: reqwest::Client,
    config: Arc<Config>,
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

#[derive(Deserialize)]
struct RuntimeStartRequest {
    preset: Preset,
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

#[derive(Deserialize)]
struct DocumentQuery {
    storage: StorageKind,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct CreateDirectoryRequest {
    storage: StorageKind,
    path: String,
}

#[derive(Deserialize)]
struct SourceSelection {
    storage: StorageKind,
    path: String,
}

#[derive(Deserialize)]
struct CreateDirectoryJobsRequest {
    #[serde(default)]
    sources: Vec<SourceSelection>,
    #[serde(default)]
    source_storage: Option<StorageKind>,
    #[serde(default)]
    source_paths: Vec<String>,
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    save_strategy: SaveStrategy,
    #[serde(default)]
    save_storage: Option<StorageKind>,
    #[serde(default)]
    save_path: Option<String>,
    preset: Preset,
    target: String,
    mode: OutputMode,
    #[serde(default)]
    settings: TranslationSettings,
}

#[derive(Serialize)]
struct DocumentEntry {
    name: String,
    path: String,
    kind: &'static str,
    supported: bool,
    size: Option<u64>,
}

#[derive(Serialize)]
struct DocumentListing {
    path: String,
    parent: Option<String>,
    entries: Vec<DocumentEntry>,
}

#[derive(Serialize)]
struct DirectoryJobsResponse {
    jobs: Vec<JobRecord>,
    skipped_existing: usize,
    skipped_incompatible: usize,
}

#[derive(Deserialize)]
struct JobListQuery {
    cursor: Option<String>,
    limit: Option<usize>,
    phase: Option<JobPhase>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JobPhase {
    Queued,
    InProgress,
    Completed,
    Failed,
}

impl JobPhase {
    fn sql_filter(self) -> &'static str {
        match self {
            Self::Queued => " AND status='queued'",
            Self::InProgress => " AND status IN ('starting_model', 'translating', 'writing')",
            Self::Completed => " AND status='completed'",
            Self::Failed => " AND status='failed'",
        }
    }
}

#[derive(Serialize)]
struct JobListResponse {
    jobs: Vec<JobRecord>,
    next_cursor: Option<String>,
    total: usize,
}

#[derive(Serialize)]
struct ActiveJobsResponse {
    jobs: Vec<JobRecord>,
}

fn encode_job_cursor(cursor: JobCursor) -> String {
    format!("{}:{}", cursor.created_at, cursor.id)
}

fn decode_job_cursor(value: &str) -> Result<JobCursor> {
    let (created_at, id) = value.split_once(':').context("invalid job cursor")?;
    Ok(JobCursor {
        created_at: created_at.parse().context("invalid job cursor timestamp")?,
        id: Uuid::parse_str(id).context("invalid job cursor id")?,
    })
}

fn collect_selected_inputs(
    paths: &[(StorageKind, PathBuf)],
) -> Result<Vec<(StorageKind, PathBuf)>> {
    let mut inputs = BTreeSet::new();
    for (storage, path) in paths {
        inputs.extend(
            collect_inputs(path)?
                .into_iter()
                .map(|input| (*storage, input)),
        );
    }
    Ok(inputs.into_iter().collect())
}

fn storage_output_segment(storage: StorageKind) -> &'static str {
    match storage {
        StorageKind::Documents => "documents",
        StorageKind::RemoteFs => "remote_fs",
    }
}

#[derive(Deserialize, Serialize)]
struct AgentAcquireRequest {
    preset: Preset,
    lease_id: String,
    ttl_seconds: u64,
}

#[derive(Deserialize)]
struct AgentRuntime {
    state: String,
    preset: Option<Preset>,
    last_error: Option<String>,
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

async fn mutate_job<F>(state: &AppState, id: Uuid, mutate: F) -> Option<JobRecord>
where
    F: FnOnce(&mut JobRecord),
{
    // Keep persistence ordered with in-memory mutations. Progress and terminal
    // updates can arrive close together; writing cloned snapshots after dropping
    // the lock could let an older snapshot overwrite the completed job on disk.
    let mut jobs = state.active_jobs.write().await;
    let entry = jobs.get_mut(&id)?;
    mutate(&mut entry.record);
    entry.record.updated_at = now_epoch_seconds();
    let entry = entry.clone();
    if let Err(error) = state.store.update(entry.clone()).await {
        error!(%id, %error, "persist job state");
    }
    Some(entry.record)
}

async fn claim_queued_job(state: &AppState, id: Uuid) -> Option<JobEntry> {
    let mut jobs = state.active_jobs.write().await;
    if jobs
        .get(&id)
        .is_none_or(|entry| entry.record.status != JobStatus::Queued)
    {
        return None;
    }
    match state.store.claim(id, now_epoch_seconds()).await {
        Ok(Some(entry)) => {
            jobs.insert(id, entry.clone());
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
        "max_upload_bytes": null,
        "translation_defaults": TranslationSettings::default(),
        "translation_limits": {
            "max_batch_size": MAX_WEB_BATCH_SIZE,
            "max_context_segments": MAX_WEB_CONTEXT_SEGMENTS
        }
    }))
}

async fn list_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DocumentQuery>,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing or invalid user identity"),
    };
    let (_, directory, relative) =
        match resolve_storage_path(&state.config, &identity, query.storage, &query.path).await {
            Ok(paths) => paths,
            Err(error) => return json_error(StatusCode::NOT_FOUND, format!("{error:#}")),
        };
    match tokio::fs::metadata(&directory).await {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return json_error(StatusCode::BAD_REQUEST, "document path is not a directory"),
        Err(error) => return json_error(StatusCode::NOT_FOUND, error.to_string()),
    }

    let mut reader = match tokio::fs::read_dir(&directory).await {
        Ok(reader) => reader,
        Err(error) => return json_error(StatusCode::FORBIDDEN, error.to_string()),
    };
    let mut entries = Vec::new();
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let metadata = match tokio::fs::symlink_metadata(entry.path()).await {
            Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
            _ => continue,
        };
        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            continue;
        };
        let entry_relative = relative.join(&name);
        entries.push(DocumentEntry {
            name,
            path: path_for_api(&entry_relative),
            kind,
            supported: metadata.is_file() && Format::from_path(&entry.path()).is_ok(),
            size: metadata.is_file().then_some(metadata.len()),
        });
    }
    entries.sort_by(|left, right| {
        (left.kind != "directory", left.name.to_lowercase())
            .cmp(&(right.kind != "directory", right.name.to_lowercase()))
    });
    let parent = relative
        .parent()
        .map(path_for_api)
        .filter(|_| !relative.as_os_str().is_empty());
    Json(DocumentListing {
        path: path_for_api(&relative),
        parent,
        entries,
    })
    .into_response()
}

async fn create_document_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDirectoryRequest>,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing or invalid user identity"),
    };
    let relative = match normalize_relative_path(&request.path) {
        Ok(path) if !path.as_os_str().is_empty() => path,
        _ => return json_error(StatusCode::BAD_REQUEST, "folder name is required"),
    };
    let Some(name) = relative.file_name() else {
        return json_error(StatusCode::BAD_REQUEST, "invalid folder name");
    };
    let parent_relative = relative.parent().unwrap_or_else(|| FsPath::new(""));
    let (root, parent, _) = match resolve_storage_path(
        &state.config,
        &identity,
        request.storage,
        &path_for_api(parent_relative),
    )
    .await
    {
        Ok(paths) => paths,
        Err(error) => return json_error(StatusCode::NOT_FOUND, format!("{error:#}")),
    };
    let destination = parent.join(name);
    match tokio::fs::create_dir(&destination).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return json_error(StatusCode::CONFLICT, "folder already exists")
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
    match tokio::fs::canonicalize(&destination).await {
        Ok(path) if path.starts_with(&root) => {}
        _ => {
            let _ = tokio::fs::remove_dir(&destination).await;
            return json_error(StatusCode::BAD_REQUEST, "folder escapes the user directory");
        }
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"path": path_for_api(&relative)})),
    )
        .into_response()
}

async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<JobListQuery>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let cursor = match query.cursor.as_deref().map(decode_job_cursor).transpose() {
        Ok(cursor) => cursor,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_JOB_PAGE_SIZE)
        .clamp(1, MAX_JOB_PAGE_SIZE);
    match state
        .store
        .list_page(owner, query.phase, cursor, limit)
        .await
    {
        Ok(page) => Json(JobListResponse {
            jobs: page.jobs,
            next_cursor: page.next_cursor.map(encode_job_cursor),
            total: page.total,
        })
        .into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

async fn list_active_jobs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    match state.store.list_active(owner).await {
        Ok(jobs) => Json(ActiveJobsResponse { jobs }).into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let owner = identity.owner.clone();
    match state.store.count_active(owner.clone()).await {
        Ok(count) if count >= MAX_USER_NONTERMINAL_JOBS => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many queued or active jobs",
            )
        }
        Ok(_) => {}
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
    let id = Uuid::new_v4();
    let dir = state
        .config
        .data_dir
        .join("users")
        .join(&owner)
        .join("jobs")
        .join(id.to_string());
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }

    let upload = dir.join(".upload.tmp");
    let mut filename = None;
    let mut preset = Preset::SevenBFp8;
    let mut target = "中文".to_string();
    let mut mode = OutputMode::Bilingual;
    let mut save_directory = None;
    let mut save_storage = StorageKind::Documents;
    let mut settings = TranslationSettings::default();
    while let Ok(Some(mut field)) = multipart.next_field().await {
        match field.name().unwrap_or_default() {
            "file" if filename.is_none() => {
                let original = field.file_name().unwrap_or("document").to_string();
                if Format::from_path(FsPath::new(&original)).is_err() {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    return json_error(StatusCode::BAD_REQUEST, "unsupported document format");
                }
                let mut file = match tokio::fs::File::create(&upload).await {
                    Ok(file) => file,
                    Err(error) => {
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                    }
                };
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if let Err(error) = file.write_all(&chunk).await {
                                return json_error(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    error.to_string(),
                                );
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            return json_error(StatusCode::BAD_REQUEST, error.to_string())
                        }
                    }
                }
                filename = Some(original);
            }
            "preset" => {
                if let Ok(value) = field.text().await {
                    preset = match value.as_str() {
                        "30b-fp8" => Preset::ThirtyBFp8,
                        _ => Preset::SevenBFp8,
                    };
                }
            }
            "target" => {
                if let Ok(value) = field.text().await {
                    if !value.trim().is_empty() && value.len() <= 64 {
                        target = value.trim().to_string();
                    }
                }
            }
            "mode" => {
                if let Ok(value) = field.text().await {
                    if let Ok(value) = value.parse() {
                        mode = value;
                    }
                }
            }
            "save_path" => {
                if let Ok(value) = field.text().await {
                    if !value.trim().is_empty() {
                        save_directory = Some(value.trim().to_string());
                    }
                }
            }
            "save_storage" => {
                if let Ok(value) = field.text().await {
                    save_storage = match value.as_str() {
                        "remote_fs" => StorageKind::RemoteFs,
                        _ => StorageKind::Documents,
                    };
                }
            }
            "batch_size" => {
                if let Ok(value) = field.text().await {
                    let Ok(value) = value.parse() else {
                        let _ = tokio::fs::remove_dir_all(&dir).await;
                        return json_error(StatusCode::BAD_REQUEST, "invalid batch size");
                    };
                    settings.batch_size = value;
                }
            }
            "context_segments" => {
                if let Ok(value) = field.text().await {
                    let Ok(value) = value.parse() else {
                        let _ = tokio::fs::remove_dir_all(&dir).await;
                        return json_error(StatusCode::BAD_REQUEST, "invalid context segments");
                    };
                    settings.context_segments = value;
                }
            }
            "cache_enabled" => {
                if let Ok(value) = field.text().await {
                    settings.cache_enabled = matches!(value.as_str(), "1" | "true" | "on");
                }
            }
            _ => {}
        }
    }

    let Some(filename) = filename else {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return json_error(StatusCode::BAD_REQUEST, "file field is required");
    };
    let settings = match settings.validate_for_web() {
        Ok(settings) => settings,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return json_error(StatusCode::BAD_REQUEST, error.to_string());
        }
    };
    let extension = FsPath::new(&filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    if extension == "docx" && mode == OutputMode::Replace {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return json_error(
            StatusCode::BAD_REQUEST,
            "DOCX currently supports bilingual output only",
        );
    }
    let selected_mode = mode;
    let (save_root, save_to, save_path) = if let Some(save_directory) = save_directory {
        let (root, destination, relative) =
            match resolve_storage_path(&state.config, &identity, save_storage, &save_directory)
                .await
            {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    return json_error(StatusCode::BAD_REQUEST, format!("{error:#}"));
                }
            };
        if !destination.is_dir() {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return json_error(StatusCode::BAD_REQUEST, "save path is not a directory");
        }
        let output_name = suffixed_output_path(FsPath::new(&filename), selected_mode)
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ferryman-result".to_string());
        let output_path = destination.join(&output_name);
        if tokio::fs::symlink_metadata(&output_path).await.is_ok() {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return json_error(
                StatusCode::CONFLICT,
                "an output file already exists in the save location",
            );
        }
        let display = path_for_api(&relative.join(output_name));
        (Some(root), Some(output_path), Some(display))
    } else {
        (None, None, None)
    };
    let input = dir.join(format!("input.{extension}"));
    let output = dir.join(format!("result.{extension}"));
    if let Err(error) = tokio::fs::rename(&upload, &input).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let now = now_epoch_seconds();
    let record_save_storage = save_to.as_ref().map(|_| save_storage);
    let entry = JobEntry {
        owner,
        dir,
        input,
        output,
        save_to,
        save_root,
        overwrite: false,
        record: JobRecord {
            id,
            filename,
            preset,
            target,
            mode,
            status: JobStatus::Queued,
            total: 0,
            completed: 0,
            translated: 0,
            failed_segments: 0,
            error: None,
            settings,
            result_available: false,
            source_path: None,
            source_storage: None,
            save_path,
            save_storage: record_save_storage,
            created_at: now,
            updated_at: now,
        },
    };
    if let Err(error) = state
        .store
        .insert(entry.clone(), MAX_USER_NONTERMINAL_JOBS)
        .await
    {
        let _ = tokio::fs::remove_dir_all(&entry.dir).await;
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let record = entry.record.clone();
    state.active_jobs.write().await.insert(id, entry);
    if state.queue.send(id).await.is_err() {
        mutate_job(&state, id, |job| {
            job.status = JobStatus::Failed;
            job.error = Some("job worker is unavailable".to_string());
        })
        .await;
        state.active_jobs.write().await.remove(&id);
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "job worker unavailable");
    }
    (StatusCode::CREATED, Json(record)).into_response()
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_document_job(
    state: &AppState,
    identity: &UserIdentity,
    source: &FsPath,
    source_path: String,
    source_storage: StorageKind,
    save_root: &FsPath,
    save_to: PathBuf,
    save_path: String,
    save_storage: StorageKind,
    overwrite: bool,
    preset: Preset,
    target: &str,
    mode: OutputMode,
    settings: TranslationSettings,
) -> Result<JobRecord> {
    let id = Uuid::new_v4();
    let dir = state
        .config
        .data_dir
        .join("users")
        .join(&identity.owner)
        .join("jobs")
        .join(id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let input = dir.join(format!("input.{extension}"));
    let output = dir.join(format!("result.{extension}"));
    if let Err(error) = tokio::fs::copy(source, &input).await {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(error.into());
    }
    let now = now_epoch_seconds();
    let entry = JobEntry {
        owner: identity.owner.clone(),
        dir,
        input,
        output,
        save_to: Some(save_to),
        save_root: Some(save_root.to_path_buf()),
        overwrite,
        record: JobRecord {
            id,
            filename: source
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "document".to_string()),
            preset,
            target: target.to_string(),
            mode,
            status: JobStatus::Queued,
            total: 0,
            completed: 0,
            translated: 0,
            failed_segments: 0,
            error: None,
            settings,
            result_available: false,
            source_path: Some(source_path),
            source_storage: Some(source_storage),
            save_path: Some(save_path),
            save_storage: Some(save_storage),
            created_at: now,
            updated_at: now,
        },
    };
    state
        .store
        .insert(entry.clone(), MAX_USER_NONTERMINAL_JOBS)
        .await?;
    let record = entry.record.clone();
    state.active_jobs.write().await.insert(id, entry);
    if state.queue.send(id).await.is_err() {
        mutate_job(state, id, |job| {
            job.status = JobStatus::Failed;
            job.error = Some("job worker unavailable".to_string());
        })
        .await;
        state.active_jobs.write().await.remove(&id);
        anyhow::bail!("job worker unavailable");
    }
    Ok(record)
}

async fn create_directory_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDirectoryJobsRequest>,
) -> Response {
    let identity = match user_identity(&headers, state.config.allow_local_user) {
        Ok(identity) => identity,
        Err(status) => return json_error(status, "missing or invalid user identity"),
    };
    let mode = request.mode;
    let settings = match request.settings.validate_for_web() {
        Ok(settings) => settings,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let target = request.target.trim();
    if target.is_empty() || target.len() > 64 {
        return json_error(StatusCode::BAD_REQUEST, "invalid target language");
    }
    let mut requested_paths: Vec<(StorageKind, String)> = request
        .sources
        .iter()
        .map(|source| (source.storage, source.path.clone()))
        .collect();
    if let Some(storage) = request.source_storage {
        requested_paths.extend(
            request
                .source_paths
                .iter()
                .cloned()
                .map(|path| (storage, path)),
        );
        if let Some(path) = request.source_path.as_deref() {
            requested_paths.push((storage, path.to_string()));
        }
    }
    requested_paths.sort();
    requested_paths.dedup();
    if requested_paths.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "select at least one file or directory",
        );
    }
    if requested_paths.len() > MAX_DIRECTORY_FILES {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("select no more than {MAX_DIRECTORY_FILES} files or directories"),
        );
    }

    let mut source_roots = HashMap::new();
    let mut selected_paths = Vec::with_capacity(requested_paths.len());
    for (storage, path) in requested_paths {
        let (root, selected, _) =
            match resolve_storage_path(&state.config, &identity, storage, &path).await {
                Ok(paths) => paths,
                Err(error) => return json_error(StatusCode::BAD_REQUEST, format!("{error:#}")),
            };
        source_roots.insert(storage, root);
        selected_paths.push((storage, selected));
    }
    let mixed_storages = source_roots.len() > 1;
    let directory_destination = if request.save_strategy == SaveStrategy::Directory {
        let (Some(save_storage), Some(save_path)) =
            (request.save_storage, request.save_path.as_deref())
        else {
            return json_error(StatusCode::BAD_REQUEST, "save directory is required");
        };
        let destination =
            match resolve_storage_path(&state.config, &identity, save_storage, save_path).await {
                Ok(paths) => paths,
                Err(error) => return json_error(StatusCode::BAD_REQUEST, format!("{error:#}")),
            };
        if !destination.1.is_dir() {
            return json_error(StatusCode::BAD_REQUEST, "save path must be a directory");
        }
        Some((save_storage, destination))
    } else {
        None
    };

    let inputs =
        match tokio::task::spawn_blocking(move || collect_selected_inputs(&selected_paths)).await {
            Ok(Ok(inputs)) => inputs,
            Ok(Err(error)) => return json_error(StatusCode::BAD_REQUEST, format!("{error:#}")),
            Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        };
    if inputs.len() > MAX_DIRECTORY_FILES {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("selection contains more than {MAX_DIRECTORY_FILES} supported files"),
        );
    }
    let eligible_count = inputs
        .iter()
        .filter(|(_, path)| !is_generated_output(path))
        .filter(|(_, path)| {
            mode != OutputMode::Replace
                || path
                    .extension()
                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("docx"))
        })
        .count();
    match state.store.count_active(identity.owner.clone()).await {
        Ok(count) if count.saturating_add(eligible_count) > MAX_USER_NONTERMINAL_JOBS => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "selection would exceed the queued or active job limit",
            )
        }
        Ok(_) => {}
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }

    let mut jobs = Vec::new();
    let mut skipped_existing = 0usize;
    let mut skipped_incompatible = 0usize;
    for (source_storage, input) in inputs
        .into_iter()
        .filter(|(_, path)| !is_generated_output(path))
    {
        if mode == OutputMode::Replace
            && input
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"))
        {
            skipped_incompatible += 1;
            continue;
        }
        let source_root = source_roots
            .get(&source_storage)
            .expect("resolved source storage root");
        let relative_input = match input.strip_prefix(source_root) {
            Ok(path) => path,
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "source path escaped its storage")
            }
        };
        let (save_storage, save_root, save_to, save_path) = match request.save_strategy {
            SaveStrategy::SiblingSuffix => {
                let mut relative_output = relative_input.to_path_buf();
                relative_output.set_file_name(
                    suffixed_output_path(&input, mode)
                        .file_name()
                        .expect("supported input has a filename"),
                );
                let save_to = source_root.join(&relative_output);
                (
                    source_storage,
                    source_root.clone(),
                    save_to,
                    path_for_api(&relative_output),
                )
            }
            SaveStrategy::SiblingOverwrite => (
                source_storage,
                source_root.clone(),
                input.clone(),
                path_for_api(relative_input),
            ),
            SaveStrategy::Directory => {
                let (save_storage, (save_root, save_directory, save_relative)) =
                    directory_destination
                        .as_ref()
                        .expect("directory destination");
                let mut relative_output = if mixed_storages {
                    PathBuf::from(storage_output_segment(source_storage)).join(relative_input)
                } else {
                    relative_input.to_path_buf()
                };
                relative_output.set_file_name(
                    suffixed_output_path(&input, mode)
                        .file_name()
                        .expect("supported input has a filename"),
                );
                (
                    *save_storage,
                    save_root.clone(),
                    save_directory.join(&relative_output),
                    path_for_api(&save_relative.join(&relative_output)),
                )
            }
        };
        if request.save_strategy != SaveStrategy::SiblingOverwrite
            && tokio::fs::symlink_metadata(&save_to).await.is_ok()
        {
            skipped_existing += 1;
            continue;
        }
        let source_path = path_for_api(relative_input);
        match enqueue_document_job(
            &state,
            &identity,
            &input,
            source_path,
            source_storage,
            &save_root,
            save_to,
            save_path,
            save_storage,
            request.save_strategy == SaveStrategy::SiblingOverwrite,
            request.preset,
            target,
            request.mode,
            settings,
        )
        .await
        {
            Ok(job) => jobs.push(job),
            Err(error) => {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
            }
        }
    }
    (
        StatusCode::CREATED,
        Json(DirectoryJobsResponse {
            jobs,
            skipped_existing,
            skipped_incompatible,
        }),
    )
        .into_response()
}

async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let entry = match state.store.get(owner, id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };
    if entry.record.status.is_terminal() {
        return Json(entry.record).into_response();
    }
    if let Some(cancel) = state.cancellations.lock().await.get(&id) {
        cancel.cancel();
    }
    let was_queued = entry.record.status == JobStatus::Queued;
    let response = match mutate_job(&state, id, |job| {
        job.status = JobStatus::Cancelled;
        job.error = None;
    })
    .await
    {
        Some(job) => Json(job).into_response(),
        None => json_error(StatusCode::NOT_FOUND, "job not found"),
    };
    if was_queued {
        state.active_jobs.write().await.remove(&id);
    }
    response
}

async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let mut entry = match state
        .store
        .retry_failed(owner, id, now_epoch_seconds(), MAX_USER_NONTERMINAL_JOBS)
        .await
    {
        Ok(RetryJobOutcome::Retried(entry)) => *entry,
        Ok(RetryJobOutcome::NotFound) => return json_error(StatusCode::NOT_FOUND, "job not found"),
        Ok(RetryJobOutcome::NotFailed) => {
            return json_error(StatusCode::CONFLICT, "only failed jobs can be retried")
        }
        Ok(RetryJobOutcome::AtLimit) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many queued or active jobs",
            )
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };

    match tokio::fs::remove_file(&entry.output).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            entry.record.status = JobStatus::Failed;
            entry.record.error = Some(format!("clear previous result before retry: {error}"));
            entry.record.updated_at = now_epoch_seconds();
            if let Err(store_error) = state.store.update(entry).await {
                error!(%id, %store_error, "restore failed job after retry cleanup error");
            }
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    }

    let record = entry.record.clone();
    state.active_jobs.write().await.insert(id, entry);
    if state.queue.send(id).await.is_err() {
        mutate_job(&state, id, |job| {
            job.status = JobStatus::Failed;
            job.error = Some("job worker is unavailable".to_string());
        })
        .await;
        state.active_jobs.write().await.remove(&id);
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "job worker unavailable");
    }
    Json(record).into_response()
}

fn result_is_downloadable(status: JobStatus, result_available: bool) -> bool {
    status == JobStatus::Completed || result_available
}

async fn download_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let entry = match state.store.get(owner, id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };
    if !result_is_downloadable(entry.record.status, entry.record.result_available) {
        return json_error(StatusCode::CONFLICT, "job is not complete");
    }
    let file = match tokio::fs::File::open(&entry.output).await {
        Ok(file) => file,
        Err(error) => return json_error(StatusCode::NOT_FOUND, error.to_string()),
    };
    let extension = entry
        .output
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let disposition = format!("attachment; filename=\"ferryman-result.{extension}\"");
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

async fn delete_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let entry = match state.store.get(owner.clone(), id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "job not found"),
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };
    if !entry.record.status.is_terminal() {
        return json_error(StatusCode::CONFLICT, "cancel the job before deleting it");
    }

    let trash_root = state.config.data_dir.join("trash");
    if let Err(error) = tokio::fs::create_dir_all(&trash_root).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let trash = trash_root.join(id.to_string());
    let moved = match tokio::fs::rename(&entry.dir, &trash).await {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    match state.store.delete_terminal(owner, id).await {
        Ok(true) => {
            if moved {
                let _ = tokio::fs::remove_dir_all(&trash).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            if moved {
                let _ = tokio::fs::rename(&trash, &entry.dir).await;
            }
            json_error(StatusCode::CONFLICT, "job is no longer deletable")
        }
        Err(error) => {
            if moved {
                let _ = tokio::fs::rename(&trash, &entry.dir).await;
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
        }
    }
}

async fn agent_json(
    config: &Config,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(StatusCode, serde_json::Value)> {
    let mut request = config
        .client
        .request(method, format!("{}{}", config.agent_url, path))
        .bearer_auth(&config.agent_token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    let value = response.json().await.unwrap_or_else(
        |error| serde_json::json!({"error": format!("invalid agent response: {error}")}),
    );
    Ok((status, value))
}

async fn runtime_status(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/runtime", None).await {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn model_catalog(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/models", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn model_storage(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/storage", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn start_model_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(preset): Path<Preset>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    let path = format!("/models/{preset}/download");
    match agent_json(&state.config, reqwest::Method::POST, &path, Some(body)).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn pause_model_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(preset): Path<Preset>,
) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    let path = format!("/models/{preset}/pause");
    match agent_json(&state.config, reqwest::Method::POST, &path, None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn delete_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(preset): Path<Preset>,
) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    let path = format!("/models/{preset}");
    match agent_json(&state.config, reqwest::Method::DELETE, &path, None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn start_source_benchmark(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    match agent_json(
        &state.config,
        reqwest::Method::POST,
        "/model-sources/benchmark",
        Some(body),
    )
    .await
    {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn cancel_source_benchmark(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    match agent_json(
        &state.config,
        reqwest::Method::DELETE,
        "/model-sources/benchmark",
        None,
    )
    .await
    {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn clear_runtime_cache(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    match agent_json(&state.config, reqwest::Method::DELETE, "/cache", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn runtime_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RuntimeStartRequest>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    let body = serde_json::json!({
        "preset": request.preset,
        "lease_id": format!("manual-{owner}"),
        "ttl_seconds": 3600
    });
    match agent_json(
        &state.config,
        reqwest::Method::POST,
        "/runtime/acquire",
        Some(body),
    )
    .await
    {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn runtime_stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    release_agent(&state.config, &format!("manual-{owner}")).await;
    match agent_json(
        &state.config,
        reqwest::Method::POST,
        "/runtime/stop",
        Some(serde_json::json!({"force": false})),
    )
    .await
    {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn acquire_agent(config: &Config, preset: Preset, lease_id: &str) -> Result<()> {
    let request = AgentAcquireRequest {
        preset,
        lease_id: lease_id.to_string(),
        ttl_seconds: 120,
    };
    let response = config
        .client
        .post(format!("{}/runtime/acquire", config.agent_url))
        .bearer_auth(&config.agent_token)
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("agent acquire failed: {}", response.text().await?);
    }
    Ok(())
}

enum AgentAcquireAttempt {
    Acquired,
    Retry(String),
    Fatal(String),
}

async fn try_acquire_agent(config: &Config, preset: Preset, lease_id: &str) -> AgentAcquireAttempt {
    let request = AgentAcquireRequest {
        preset,
        lease_id: lease_id.to_string(),
        ttl_seconds: 120,
    };
    let response = match config
        .client
        .post(format!("{}/runtime/acquire", config.agent_url))
        .bearer_auth(&config.agent_token)
        .json(&request)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return AgentAcquireAttempt::Retry(error.to_string()),
    };
    let status = response.status();
    if status.is_success() {
        return AgentAcquireAttempt::Acquired;
    }
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("invalid agent response: {error}"));
    if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        AgentAcquireAttempt::Retry(format!("agent acquire failed ({status}): {message}"))
    } else {
        AgentAcquireAttempt::Fatal(format!("agent acquire failed ({status}): {message}"))
    }
}

async fn acquire_agent_with_retry(
    config: &Config,
    preset: Preset,
    lease_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut delay = Duration::from_secs(1);
    loop {
        let attempt = tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("job cancelled"),
            attempt = try_acquire_agent(config, preset, lease_id) => attempt,
        };
        match attempt {
            AgentAcquireAttempt::Acquired => return Ok(()),
            AgentAcquireAttempt::Fatal(message) => anyhow::bail!(message),
            AgentAcquireAttempt::Retry(message) => {
                warn!(%message, retry_seconds = delay.as_secs(), "agent temporarily unavailable");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("job cancelled"),
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn release_agent(config: &Config, lease_id: &str) {
    if let Err(error) = config
        .client
        .delete(format!("{}/runtime/leases/{lease_id}", config.agent_url))
        .bearer_auth(&config.agent_token)
        .send()
        .await
    {
        warn!(%error, %lease_id, "release model lease");
    }
}

async fn wait_for_agent(
    config: &Config,
    preset: Preset,
    lease_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut poll_delay = Duration::from_secs(2);
    let mut startup_failures = 0usize;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("job cancelled"),
            _ = tokio::time::sleep(poll_delay) => {}
        }
        let response = match tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("job cancelled"),
            response = config
                .client
                .get(format!("{}/runtime", config.agent_url))
                .bearer_auth(&config.agent_token)
                .send() => response,
        } {
            Ok(response) => response,
            Err(error) => {
                warn!(%error, "wait for agent runtime");
                poll_delay = (poll_delay * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|error| format!("invalid agent response: {error}"));
            if status.is_server_error()
                || status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
            {
                warn!(%status, %message, "agent runtime temporarily unavailable");
                poll_delay = (poll_delay * 2).min(Duration::from_secs(30));
                continue;
            }
            anyhow::bail!("agent runtime failed ({status}): {message}");
        }
        let runtime = match response.json::<AgentRuntime>().await {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "invalid agent runtime response");
                poll_delay = (poll_delay * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        poll_delay = Duration::from_secs(2);
        if runtime.state == "ready" && runtime.preset == Some(preset) {
            return Ok(());
        }
        if runtime.state == "failed" {
            startup_failures += 1;
            let message = runtime.last_error.unwrap_or_else(|| "unknown error".into());
            if startup_failures >= MAX_MODEL_START_ATTEMPTS {
                anyhow::bail!(
                    "model failed to start after {MAX_MODEL_START_ATTEMPTS} attempts: {message}"
                );
            }
            warn!(
                attempt = startup_failures,
                max_attempts = MAX_MODEL_START_ATTEMPTS,
                %message,
                "model startup failed; retrying"
            );
            release_agent(config, lease_id).await;
            acquire_agent_with_retry(config, preset, lease_id, cancel).await?;
            poll_delay = Duration::from_secs(2);
        }
    }
}

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

async fn job_worker(state: AppState, mut queue: mpsc::Receiver<Uuid>) {
    let mut pending = VecDeque::new();
    let mut active = JoinSet::new();
    let mut active_outputs = HashMap::new();
    let mut active_preset = None;
    let mut queue_open = true;

    loop {
        while active.len() < MAX_ACTIVE_JOBS {
            let Some(id) = pending.front().copied() else {
                break;
            };
            let Some((preset, output)) = queued_job_schedule(&state, id).await else {
                pending.pop_front();
                continue;
            };
            if !can_dispatch_job(
                active_preset,
                active_outputs.values(),
                preset,
                output.as_ref(),
            ) {
                break;
            }

            pending.pop_front();
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

async fn ensure_safe_output_parent(root: &FsPath, target: &FsPath) -> Result<PathBuf> {
    let root = tokio::fs::canonicalize(root).await?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent"))?;
    let relative = parent
        .strip_prefix(&root)
        .context("output path escapes the user directory")?;
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            anyhow::bail!("invalid output directory");
        };
        let next = current.join(part);
        match tokio::fs::symlink_metadata(&next).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("output directory contains a symbolic link")
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => anyhow::bail!("output parent is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&next).await?;
            }
            Err(error) => return Err(error.into()),
        }
        current = tokio::fs::canonicalize(&next).await?;
        if !current.starts_with(&root) {
            anyhow::bail!("output directory escapes the user directory");
        }
    }
    Ok(current)
}

async fn save_job_result(entry: &JobEntry) -> Result<()> {
    let (Some(target), Some(root)) = (&entry.save_to, &entry.save_root) else {
        return Ok(());
    };
    let parent = ensure_safe_output_parent(root, target).await?;
    let filename = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no filename"))?;
    let target = parent.join(filename);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata)
            if entry.overwrite && metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) if entry.overwrite => anyhow::bail!("overwrite target is not a regular file"),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            if files_are_identical(&entry.output, &target).await? {
                return Ok(());
            }
            anyhow::bail!("output file already exists")
        }
        Ok(_) => anyhow::bail!("output file already exists"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !entry.overwrite => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("overwrite target no longer exists")
        }
        Err(error) => return Err(error.into()),
    }
    let temp = parent.join(format!(".ferryman-{}.tmp", Uuid::new_v4()));
    let copy_result = async {
        let mut source = tokio::fs::File::open(&entry.output).await?;
        let mut destination = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.sync_all().await?;
        if entry.overwrite {
            tokio::fs::rename(&temp, &target).await?;
        } else {
            tokio::fs::hard_link(&temp, &target).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    copy_result.with_context(|| format!("save result to {}", target.display()))
}

async fn files_are_identical(left: &FsPath, right: &FsPath) -> Result<bool> {
    let left_metadata = tokio::fs::metadata(left).await?;
    let right_metadata = tokio::fs::metadata(right).await?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    let mut left = tokio::fs::File::open(left).await?;
    let mut right = tokio::fs::File::open(right).await?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer).await?;
        let right_read = right.read(&mut right_buffer).await?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

async fn run_job(state: &AppState, entry: JobEntry) -> Result<()> {
    let id = entry.record.id;
    let preset = entry.record.preset;
    let lease_id = format!("job-{id}");
    let cancel = CancellationToken::new();
    state.cancellations.lock().await.insert(id, cancel.clone());
    if state
        .active_jobs
        .read()
        .await
        .get(&id)
        .is_none_or(|entry| entry.record.status != JobStatus::StartingModel)
    {
        state.cancellations.lock().await.remove(&id);
        return Ok(());
    }

    release_agent(&state.config, &format!("manual-{}", entry.owner)).await;
    if let Err(error) = acquire_agent_with_retry(&state.config, preset, &lease_id, &cancel).await {
        state.cancellations.lock().await.remove(&id);
        return Err(error);
    }
    let heartbeat_cancel = CancellationToken::new();
    let heartbeat = {
        let state = state.clone();
        let lease_id = lease_id.clone();
        let heartbeat_cancel = heartbeat_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = heartbeat_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        if let Err(error) = acquire_agent(&state.config, preset, &lease_id).await {
                            warn!(%error, "renew model lease");
                        }
                    }
                }
            }
        })
    };

    let result = async {
        wait_for_agent(&state.config, preset, &lease_id, &cancel).await?;
        mutate_job(state, id, |job| job.status = JobStatus::Translating).await;

        let cache_dir = state
            .config
            .data_dir
            .join("users")
            .join(&entry.owner)
            .join("cache");
        let request_limiter = state
            .request_limiters
            .get(&preset)
            .cloned()
            .context("missing request limiter for model preset")?;
        let cache = if entry.record.settings.cache_enabled {
            Cache::open(Some(cache_dir))
        } else {
            None
        };
        let engine = Engine::new(
            state.translation_client.clone(),
            state.config.agent_url.clone(),
            preset.api_model().to_string(),
            entry.record.target.clone(),
            preset.config().concurrency,
            cache,
        )
        .with_request_limiter(request_limiter);

        let (progress_tx, mut progress_rx) = watch::channel(BatchProgress::default());
        let callback: ProgressCallback = Arc::new(move |progress| {
            progress_tx.send_replace(progress);
        });
        let progress_state = state.clone();
        let progress_task = tokio::spawn(async move {
            while progress_rx.changed().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(350)).await;
                let progress = progress_rx.borrow_and_update().clone();
                mutate_job(&progress_state, id, |job| {
                    job.total = progress.total;
                    job.completed = progress.completed;
                    job.translated = progress.translated;
                    job.failed_segments = progress.failed;
                })
                .await;
            }
        });
        let summary = run_batch_controlled(
            &engine,
            vec![entry.input.clone()],
            BatchOpts {
                mode: entry.record.mode,
                in_place: false,
                output: Some(entry.output.clone()),
                batch_size: entry.record.settings.batch_size,
                context: entry.record.settings.context_segments,
                limit: None,
            },
            cancel.clone(),
            callback,
        )
        .await;
        let _ = progress_task.await;

        if cancel.is_cancelled() || summary.cancelled {
            let result_available = tokio::fs::metadata(&entry.output)
                .await
                .is_ok_and(|metadata| metadata.is_file());
            mutate_job(state, id, |job| {
                job.status = JobStatus::Cancelled;
                job.result_available = result_available;
            })
            .await;
        } else if !summary.failed_files.is_empty() {
            anyhow::bail!("{}", summary.failed_files[0].1);
        } else {
            if entry.save_to.is_some() {
                mutate_job(state, id, |job| job.status = JobStatus::Writing).await;
                save_job_result(&entry).await?;
            }
            mutate_job(state, id, |job| {
                job.status = JobStatus::Completed;
                job.total = summary.translated + summary.failed;
                job.completed = job.total;
                job.translated = summary.translated;
                job.failed_segments = summary.failed;
                job.error = None;
                job.result_available = true;
            })
            .await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    heartbeat_cancel.cancel();
    heartbeat.abort();
    release_agent(&state.config, &lease_id).await;
    state.cancellations.lock().await.remove(&id);
    result
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
    let state = AppState {
        active_jobs: Arc::new(RwLock::new(active_jobs)),
        store,
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        queue,
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
    tokio::spawn(job_worker(state.clone(), receiver));
    for id in pending {
        state.queue.send(id).await.ok();
    }

    let app = Router::new()
        .route("/api/config", get(config))
        .route("/api/documents", get(list_documents))
        .route(
            "/api/documents/directories",
            post(create_document_directory),
        )
        .route(
            "/api/jobs",
            get(list_jobs)
                .post(create_job)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/jobs/active", get(list_active_jobs))
        .route("/api/jobs/{id}", axum::routing::delete(delete_job))
        .route("/api/jobs/directory", post(create_directory_jobs))
        .route("/api/jobs/selection", post(create_directory_jobs))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/retry", post(retry_job))
        .route("/api/jobs/{id}/result", get(download_result))
        .route("/api/runtime", get(runtime_status))
        .route("/api/runtime/start", post(runtime_start))
        .route("/api/runtime/stop", post(runtime_stop))
        .route("/api/models", get(model_catalog))
        .route("/api/models/{preset}", axum::routing::delete(delete_model))
        .route("/api/models/{preset}/download", post(start_model_download))
        .route("/api/models/{preset}/pause", post(pause_model_download))
        .route(
            "/api/model-sources/benchmark",
            post(start_source_benchmark).delete(cancel_source_benchmark),
        )
        .route("/api/storage", get(model_storage))
        .route(
            "/api/runtime-cache",
            axum::routing::delete(clear_runtime_cache),
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

    #[test]
    fn completed_or_partial_results_can_be_downloaded() {
        assert!(result_is_downloadable(JobStatus::Completed, false));
        assert!(result_is_downloadable(JobStatus::Cancelled, true));
        assert!(!result_is_downloadable(JobStatus::Cancelled, false));
        assert!(!result_is_downloadable(JobStatus::Failed, false));
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
    fn output_names_distinguish_modes_and_generated_files() {
        assert_eq!(
            suffixed_output_path(FsPath::new("book.epub"), OutputMode::Bilingual),
            PathBuf::from("book.bilingual.epub")
        );
        assert_eq!(
            suffixed_output_path(FsPath::new("notes.md"), OutputMode::Replace),
            PathBuf::from("notes.translated.md")
        );
        assert!(is_generated_output(FsPath::new("book.bilingual.epub")));
        assert!(is_generated_output(FsPath::new("notes.translated.md")));
        assert!(!is_generated_output(FsPath::new("bilingual.md")));
    }

    #[test]
    fn selected_files_and_recursive_directories_are_deduplicated() {
        let base = env::temp_dir().join(format!("ferryman-selection-test-{}", Uuid::new_v4()));
        let books = base.join("Books");
        std::fs::create_dir_all(books.join("Notes")).unwrap();
        std::fs::write(books.join("book.txt"), b"book").unwrap();
        std::fs::write(books.join("Notes/chapter.md"), b"chapter").unwrap();
        std::fs::write(books.join("cover.jpg"), b"image").unwrap();

        let inputs = collect_selected_inputs(&[
            (StorageKind::Documents, books.clone()),
            (StorageKind::Documents, books.join("book.txt")),
        ])
        .unwrap();
        assert_eq!(
            inputs,
            vec![
                (StorageKind::Documents, books.join("Notes/chapter.md")),
                (StorageKind::Documents, books.join("book.txt")),
            ]
        );

        std::fs::remove_dir_all(base).unwrap();
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

    #[tokio::test]
    async fn safe_output_parent_stays_under_document_root() {
        let base = env::temp_dir().join(format!("ferryman-path-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let target = root.join("Translations/Books/book.bilingual.epub");
        let parent = ensure_safe_output_parent(&root, &target).await.unwrap();
        assert!(parent.starts_with(tokio::fs::canonicalize(&root).await.unwrap()));
        assert!(parent.ends_with("Translations/Books"));
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn safe_output_parent_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let base = env::temp_dir().join(format!("ferryman-link-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let outside = base.join("outside");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let target = root.join("escape/result.epub");
        assert!(ensure_safe_output_parent(&root, &target).await.is_err());
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn saved_results_are_copied_without_overwriting() {
        let base = env::temp_dir().join(format!("ferryman-save-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let job_dir = base.join("job");
        let output = job_dir.join("result.txt");
        let target = root.join("Translations/notes.bilingual.txt");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&job_dir).await.unwrap();
        tokio::fs::write(&output, b"translated result")
            .await
            .unwrap();
        let now = now_epoch_seconds();
        let entry = JobEntry {
            owner: "test".to_string(),
            dir: job_dir,
            input: base.join("input.txt"),
            output,
            save_to: Some(target.clone()),
            save_root: Some(root),
            overwrite: false,
            record: JobRecord {
                id: Uuid::new_v4(),
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status: JobStatus::Writing,
                total: 0,
                completed: 0,
                translated: 0,
                failed_segments: 0,
                error: None,
                settings: TranslationSettings::default(),
                result_available: false,
                source_path: None,
                source_storage: None,
                save_path: Some("Translations/notes.bilingual.txt".to_string()),
                save_storage: Some(StorageKind::Documents),
                created_at: now,
                updated_at: now,
            },
        };
        save_job_result(&entry).await.unwrap();
        assert_eq!(
            tokio::fs::read(&target).await.unwrap(),
            b"translated result"
        );
        save_job_result(&entry).await.unwrap();
        tokio::fs::write(&target, b"different result")
            .await
            .unwrap();
        assert!(save_job_result(&entry).await.is_err());
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn saved_results_atomically_overwrite_regular_files_when_requested() {
        let base = env::temp_dir().join(format!("ferryman-overwrite-test-{}", Uuid::new_v4()));
        let root = base.join("documents");
        let job_dir = base.join("job");
        let output = job_dir.join("result.txt");
        let target = root.join("notes.txt");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::create_dir_all(&job_dir).await.unwrap();
        tokio::fs::write(&target, b"original").await.unwrap();
        tokio::fs::write(&output, b"translated result")
            .await
            .unwrap();
        let now = now_epoch_seconds();
        let entry = JobEntry {
            owner: "test".to_string(),
            dir: job_dir,
            input: base.join("input.txt"),
            output,
            save_to: Some(target.clone()),
            save_root: Some(root),
            overwrite: true,
            record: JobRecord {
                id: Uuid::new_v4(),
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status: JobStatus::Writing,
                total: 0,
                completed: 0,
                translated: 0,
                failed_segments: 0,
                error: None,
                settings: TranslationSettings::default(),
                result_available: false,
                source_path: Some("notes.txt".to_string()),
                source_storage: Some(StorageKind::Documents),
                save_path: Some("notes.txt".to_string()),
                save_storage: Some(StorageKind::Documents),
                created_at: now,
                updated_at: now,
            },
        };

        save_job_result(&entry).await.unwrap();
        assert_eq!(
            tokio::fs::read(&target).await.unwrap(),
            b"translated result"
        );
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn agent_acquire_retries_temporary_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = Router::new().route(
            "/runtime/acquire",
            post(move || {
                let handler_calls = handler_calls.clone();
                async move {
                    if handler_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::SERVICE_UNAVAILABLE, "agent is starting")
                    } else {
                        (StatusCode::OK, "ready")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = Config {
            data_dir: PathBuf::new(),
            user_documents_dir: PathBuf::new(),
            remote_fs_dir: PathBuf::new(),
            agent_url: format!("http://{address}"),
            agent_token: "test-agent-token".to_string(),
            allow_local_user: true,
            client: reqwest::Client::new(),
        };

        acquire_agent_with_retry(
            &config,
            Preset::SevenBFp8,
            "job-test",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn waiting_for_unavailable_agent_remains_cancellable() {
        let config = Config {
            data_dir: PathBuf::new(),
            user_documents_dir: PathBuf::new(),
            remote_fs_dir: PathBuf::new(),
            agent_url: "http://127.0.0.1:9".to_string(),
            agent_token: "test-agent-token".to_string(),
            allow_local_user: true,
            client: reqwest::Client::new(),
        };
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_task.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_agent(&config, Preset::SevenBFp8, "job-test", &cancel),
        )
        .await
        .expect("cancellation should not wait for an HTTP timeout");
        assert!(format!("{:#}", result.unwrap_err()).contains("job cancelled"));
    }
}
