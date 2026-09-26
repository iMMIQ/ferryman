//! Job upload, listing, retry, cancel, result download, and delete handlers.

use super::job_store::{JobCursor, RetryJobOutcome};
use super::{
    json_error, mutate_job, now_epoch_seconds, path_for_api, resolve_storage_path, user_identity,
    user_owner, AppState, JobEntry, JobRecord, JobStatus, SaveStrategy, StorageKind, UserIdentity,
    DEFAULT_JOB_PAGE_SIZE, MAX_DIRECTORY_FILES, MAX_JOB_PAGE_SIZE, MAX_TEXT_FIELD_BYTES,
    MAX_UPLOAD_BYTES, MAX_USER_NONTERMINAL_JOBS,
};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferryman::batch::{is_generated_output, suffixed_output_path};
use ferryman::format::{Format, OutputMode};
use ferryman::preset::Preset;
use ferryman::settings::TranslationSettings;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path as FsPath, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use tracing::error;
use uuid::Uuid;

#[derive(Deserialize)]
struct SourceSelection {
    storage: StorageKind,
    path: String,
}

#[derive(Deserialize)]
pub(super) struct CreateDirectoryJobsRequest {
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
struct DirectoryJobsResponse {
    jobs: Vec<JobRecord>,
    skipped_existing: usize,
    skipped_incompatible: usize,
    skipped_unsupported: usize,
    skipped_generated: usize,
}

#[derive(Deserialize)]
pub(super) struct JobListQuery {
    cursor: Option<String>,
    limit: Option<usize>,
    phase: Option<JobPhase>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobPhase {
    Queued,
    InProgress,
    Completed,
    Failed,
    Partial,
    Cancelled,
}

impl JobPhase {
    pub(crate) fn sql_filter(self) -> &'static str {
        match self {
            Self::Queued => " AND status='queued'",
            Self::InProgress => " AND status IN ('starting_model', 'translating', 'writing')",
            Self::Completed => " AND status='completed' AND failed_segments=0",
            Self::Partial => " AND status='completed' AND failed_segments>0",
            Self::Cancelled => " AND status='cancelled'",
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
    fn visit(
        storage: StorageKind,
        path: &FsPath,
        inputs: &mut BTreeSet<(StorageKind, PathBuf)>,
    ) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                visit(storage, &entry?.path(), inputs)?;
            }
        } else if metadata.is_file() {
            inputs.insert((storage, path.to_path_buf()));
            anyhow::ensure!(
                inputs.len() <= MAX_DIRECTORY_FILES,
                "selection contains too many files"
            );
        }
        Ok(())
    }
    let mut inputs = BTreeSet::new();
    for (storage, path) in paths {
        visit(*storage, path, &mut inputs)?;
    }
    Ok(inputs.into_iter().collect())
}

fn storage_output_segment(storage: StorageKind) -> &'static str {
    match storage {
        StorageKind::Documents => "documents",
        StorageKind::RemoteFs => "remote_fs",
    }
}

pub(super) async fn list_jobs(
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

pub(super) async fn list_active_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    match state.store.list_active(owner).await {
        Ok(jobs) => Json(ActiveJobsResponse { jobs }).into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

/// Abort an in-progress upload: remove the job directory and return an error
/// response. Every failure path in `create_job` funnels through here so a
/// half-written upload never leaves an orphan directory behind.
async fn reject_upload(dir: &FsPath, status: StatusCode, message: impl Into<String>) -> Response {
    let _ = tokio::fs::remove_dir_all(dir).await;
    json_error(status, message)
}

/// Stream one multipart `file` field to `path` without buffering it in
/// memory, enforcing `max_bytes`. Returns the number of bytes written and a
/// (status, message) pair suitable for `reject_upload` on failure.
async fn stream_field_to_file(
    field: &mut axum::extract::multipart::Field<'_>,
    path: &FsPath,
    max_bytes: u64,
) -> Result<u64, (StatusCode, String)> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let mut written: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                written += chunk.len() as u64;
                if written > max_bytes {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("upload exceeds {} bytes", max_bytes),
                    ));
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            }
            Ok(None) => return Ok(written),
            Err(error) => return Err((StatusCode::BAD_REQUEST, error.to_string())),
        }
    }
}

/// Read one multipart text field with a hard length cap so a client cannot
/// buffer an unbounded field value in memory.
async fn bounded_field_text(field: &mut axum::extract::multipart::Field<'_>) -> Result<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if bytes.len() + chunk.len() > MAX_TEXT_FIELD_BYTES {
            anyhow::bail!("field exceeds {} bytes", MAX_TEXT_FIELD_BYTES);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) async fn create_job(
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
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            // A malformed body (truncated upload, broken boundary) must abort
            // the job instead of silently proceeding with whatever fields
            // happened to parse.
            Err(error) => {
                return reject_upload(
                    &dir,
                    StatusCode::BAD_REQUEST,
                    format!("malformed multipart body: {error}"),
                )
                .await;
            }
        };
        match field.name().unwrap_or_default() {
            "file" if filename.is_none() => {
                let original = field.file_name().unwrap_or("document").to_string();
                if Format::from_path(FsPath::new(&original)).is_err() {
                    return reject_upload(
                        &dir,
                        StatusCode::BAD_REQUEST,
                        "unsupported document format",
                    )
                    .await;
                }
                if let Err((status, message)) =
                    stream_field_to_file(&mut field, &upload, MAX_UPLOAD_BYTES).await
                {
                    return reject_upload(&dir, status, message).await;
                }
                filename = Some(original);
            }
            other => {
                let name = other.to_string();
                let value = match bounded_field_text(&mut field).await {
                    Ok(value) => value,
                    Err(error) => {
                        return reject_upload(&dir, StatusCode::BAD_REQUEST, format!("{error:#}"))
                            .await;
                    }
                };
                match name.as_str() {
                    "preset" => {
                        preset = match value.as_str() {
                            "30b-fp8" => Preset::ThirtyBFp8,
                            _ => Preset::SevenBFp8,
                        };
                    }
                    "target" => {
                        if !value.trim().is_empty() && value.len() <= 64 {
                            target = value.trim().to_string();
                        }
                    }
                    "mode" => {
                        if let Ok(value) = value.parse() {
                            mode = value;
                        }
                    }
                    "save_path" => {
                        if !value.trim().is_empty() {
                            save_directory = Some(value.trim().to_string());
                        }
                    }
                    "save_storage" => {
                        save_storage = match value.as_str() {
                            "remote_fs" => StorageKind::RemoteFs,
                            _ => StorageKind::Documents,
                        };
                    }
                    "batch_size" => {
                        let Ok(value) = value.parse() else {
                            return reject_upload(
                                &dir,
                                StatusCode::BAD_REQUEST,
                                "invalid batch size",
                            )
                            .await;
                        };
                        settings.batch_size = value;
                    }
                    "context_segments" => {
                        let Ok(value) = value.parse() else {
                            return reject_upload(
                                &dir,
                                StatusCode::BAD_REQUEST,
                                "invalid context segments",
                            )
                            .await;
                        };
                        settings.context_segments = value;
                    }
                    "cache_enabled" => {
                        settings.cache_enabled = matches!(value.as_str(), "1" | "true" | "on");
                    }
                    _ => {}
                }
            }
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

pub(super) async fn create_directory_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDirectoryJobsRequest>,
) -> Response {
    directory_jobs(state, headers, request, false).await
}

pub(super) async fn preview_directory_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDirectoryJobsRequest>,
) -> Response {
    directory_jobs(state, headers, request, true).await
}

async fn directory_jobs(
    state: AppState,
    headers: HeaderMap,
    request: CreateDirectoryJobsRequest,
    preview: bool,
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
    let mut jobs = Vec::new();
    let mut preview_files = Vec::new();
    let mut planned = Vec::new();
    let mut skipped_existing = 0usize;
    let mut skipped_incompatible = 0usize;
    let mut skipped_unsupported = 0usize;
    let mut skipped_generated = 0usize;
    for (source_storage, input) in inputs {
        let unsupported = Format::from_path(&input).is_err();
        let generated = is_generated_output(&input);
        let incompatible = mode == OutputMode::Replace
            && input
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"));
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
        let source_path = path_for_api(relative_input);
        let existing = request.save_strategy != SaveStrategy::SiblingOverwrite
            && tokio::fs::symlink_metadata(&save_to).await.is_ok();
        let reason = if unsupported {
            Some("不支持的文件格式")
        } else if generated {
            Some("已生成的译文，不重复翻译")
        } else if incompatible {
            Some("DOCX 仅支持双语对照")
        } else if existing {
            Some("输出文件已存在，不会覆盖")
        } else {
            None
        };
        preview_files.push(serde_json::json!({
            "source_path": source_path, "source_storage": source_storage,
            "save_path": save_path, "save_storage": save_storage,
            "overwrite": request.save_strategy == SaveStrategy::SiblingOverwrite,
            "skip_reason": reason,
        }));
        if unsupported {
            skipped_unsupported += 1;
            continue;
        }
        if generated {
            skipped_generated += 1;
            continue;
        }
        if incompatible {
            skipped_incompatible += 1;
            continue;
        }
        if existing {
            skipped_existing += 1;
            continue;
        }
        planned.push((
            input,
            source_path,
            source_storage,
            save_root,
            save_to,
            save_path,
            save_storage,
        ));
    }
    let eligible_count = planned.len();
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

    if preview {
        return Json(serde_json::json!({
            "eligible_count": eligible_count,
            "skipped_existing": skipped_existing, "skipped_incompatible": skipped_incompatible,
            "skipped_unsupported": skipped_unsupported, "skipped_generated": skipped_generated,
            "files": preview_files,
        }))
        .into_response();
    }
    for (input, source_path, source_storage, save_root, save_to, save_path, save_storage) in planned
    {
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
            skipped_unsupported,
            skipped_generated,
        }),
    )
        .into_response()
}

pub(super) async fn cancel_job(
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

pub(super) async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    if state
        .active_jobs
        .read()
        .await
        .get(&id)
        .is_some_and(|entry| entry.owner == owner)
    {
        return json_error(StatusCode::CONFLICT, "任务仍在运行或停止中，请稍后重试");
    }
    let mut entry = match state
        .store
        .retry_incomplete(owner, id, now_epoch_seconds(), MAX_USER_NONTERMINAL_JOBS)
        .await
    {
        Ok(RetryJobOutcome::Retried(entry)) => *entry,
        Ok(RetryJobOutcome::NotFound) => return json_error(StatusCode::NOT_FOUND, "job not found"),
        Ok(RetryJobOutcome::NotRetryable) => {
            return json_error(
                StatusCode::CONFLICT,
                "only failed, cancelled or partially completed jobs can be retried",
            )
        }
        Ok(RetryJobOutcome::AtLimit) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many queued or active jobs",
            )
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };

    if let Err(error) = prepare_retry_output(&entry).await {
        entry.record.status = JobStatus::Failed;
        entry.record.error = Some(format!("{error:#}"));
        entry.record.result_available = tokio::fs::metadata(&entry.output).await.is_ok();
        entry.record.updated_at = now_epoch_seconds();
        if let Err(store_error) = state.store.update(entry).await {
            error!(%id, %store_error, "restore failed job after retry preparation");
        }
        return json_error(StatusCode::CONFLICT, format!("{error:#}"));
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

// Preserve the previous saved copy until a retry has a replacement ready.
// The snapshot also prevents a retry from overwriting later user edits.
pub(crate) async fn prepare_retry_output(entry: &JobEntry) -> Result<()> {
    let snapshot = entry.dir.join("retry-save-base");
    if let Some(target) = entry.save_to.as_ref().filter(|_| entry.save_root.is_some()) {
        match tokio::fs::symlink_metadata(target).await {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "保存位置不是普通文件"
                );
                let previous_result = crate::runner::files_are_identical(&entry.output, target)
                    .await
                    .unwrap_or(false);
                let original = entry.overwrite
                    && crate::runner::files_are_identical(&entry.input, target)
                        .await
                        .unwrap_or(false);
                let previous_base = crate::runner::files_are_identical(&snapshot, target)
                    .await
                    .unwrap_or(false);
                anyhow::ensure!(
                    previous_result || original || previous_base,
                    "保存位置的文件已被修改，无法安全重试；请另建任务并选择新的保存位置"
                );
                let temp = entry.dir.join("retry-save-base.tmp");
                tokio::fs::copy(target, &temp).await?;
                tokio::fs::rename(temp, snapshot).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match tokio::fs::remove_file(snapshot).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn result_is_downloadable(status: JobStatus, result_available: bool) -> bool {
    status == JobStatus::Completed || result_available
}

fn result_disposition(name: &str, extension: &str) -> String {
    let encoded: String = name
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"ferryman-result.{extension}\"; filename*=UTF-8''{encoded}")
}

pub(super) async fn download_result(
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
    let name = suffixed_output_path(FsPath::new(&entry.record.filename), entry.record.mode);
    let name = name.file_name().unwrap_or_default().to_string_lossy();
    let disposition = result_disposition(&name, extension);
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

pub(super) async fn delete_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let owner = match user_owner(&headers, state.config.allow_local_user) {
        Ok(owner) => owner,
        Err(status) => return json_error(status, "missing SAFE_UID header"),
    };
    if state
        .active_jobs
        .read()
        .await
        .get(&id)
        .is_some_and(|entry| entry.owner == owner)
    {
        return json_error(StatusCode::CONFLICT, "任务仍在停止中，请稍后删除");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_store::JobStore;
    use crate::{Config, JobPersister};
    use axum::extract::{FromRequest, Request};
    use std::env;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex, RwLock};

    #[test]
    fn download_names_preserve_unicode_and_escape_header_characters() {
        let header = result_disposition("报告.bilingual.pdf", "pdf");
        assert!(header.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8A.bilingual.pdf"));
        let header = result_disposition("a\"\r\n.txt", "txt");
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
        assert!(header.contains("%22%0D%0A"));
    }

    #[test]
    fn completed_or_partial_results_can_be_downloaded() {
        assert!(result_is_downloadable(JobStatus::Completed, false));
        assert!(result_is_downloadable(JobStatus::Cancelled, true));
        assert!(!result_is_downloadable(JobStatus::Cancelled, false));
        assert!(!result_is_downloadable(JobStatus::Failed, false));
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
                (StorageKind::Documents, books.join("cover.jpg")),
            ]
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    async fn test_app_state(base: &FsPath) -> AppState {
        let (queue, _receiver) = mpsc::channel(1);
        let store = JobStore::open(base.join("jobs.sqlite3")).await.unwrap();
        AppState {
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            persister: JobPersister::spawn(store.clone()),
            store,
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            queue,
            request_limiters: Arc::new(HashMap::new()),
            translation_client: reqwest::Client::new(),
            config: Arc::new(Config {
                data_dir: base.join("data"),
                user_documents_dir: base.join("documents"),
                remote_fs_dir: base.join("remote-fs"),
                agent_url: String::new(),
                agent_token: String::new(),
                agent_pairing: false,
                allow_local_user: true,
                client: reqwest::Client::new(),
            }),
        }
    }

    #[tokio::test]
    async fn selection_preview_reports_destinations_and_skips_without_creating_jobs() {
        let base = env::temp_dir().join(format!("ferryman-preview-{}", Uuid::new_v4()));
        let state = test_app_state(&base).await;
        let books = state
            .config
            .user_documents_dir
            .join("local-development-user/Books");
        tokio::fs::create_dir_all(&books).await.unwrap();
        for name in [
            "book.txt",
            "existing.txt",
            "existing.translated.txt",
            "manual.docx",
            "cover.jpg",
        ] {
            tokio::fs::write(books.join(name), b"fixture")
                .await
                .unwrap();
        }
        let request: CreateDirectoryJobsRequest = serde_json::from_value(serde_json::json!({
            "sources": [{"storage":"documents","path":"Books"}],
            "save_strategy":"sibling_suffix", "preset":"7b-fp8", "target":"中文", "mode":"replace"
        }))
        .unwrap();
        let response =
            preview_directory_jobs(State(state.clone()), HeaderMap::new(), Json(request)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let preview: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(preview["eligible_count"], 1);
        assert_eq!(preview["skipped_existing"], 1);
        assert_eq!(preview["skipped_incompatible"], 1);
        assert_eq!(preview["skipped_unsupported"], 1);
        assert_eq!(preview["skipped_generated"], 1);
        assert!(preview["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["save_path"] == "Books/book.translated.txt"));
        assert_no_job_dirs(&state).await;
        assert_eq!(
            state
                .store
                .count_active("local-development-user".into())
                .await
                .unwrap(),
            0
        );
        let invalid: CreateDirectoryJobsRequest = serde_json::from_value(serde_json::json!({
            "sources": [{"storage":"documents","path":"../outside"}],
            "save_strategy":"sibling_overwrite", "preset":"7b-fp8", "target":"中文", "mode":"bilingual"
        })).unwrap();
        assert_eq!(
            preview_directory_jobs(State(state), HeaderMap::new(), Json(invalid))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        tokio::fs::remove_dir_all(base).await.unwrap();
    }

    async fn multipart_from_body(body: String, boundary: &str) -> Multipart {
        let request = Request::builder()
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        Multipart::from_request(request, &()).await.unwrap()
    }

    #[tokio::test]
    async fn oversized_file_uploads_are_rejected_without_partial_writes() {
        let base = env::temp_dir().join(format!("ferryman-upload-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let boundary = "X-TEST-BOUNDARY";
        let body = format!(
        "--{b}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"book.txt\"\r\n\r\n0123456789\r\n--{b}--\r\n",
        b = boundary
    );
        let mut multipart = multipart_from_body(body, boundary).await;
        let mut field = multipart.next_field().await.unwrap().unwrap();
        let path = base.join("upload.tmp");
        let error = stream_field_to_file(&mut field, &path, 4)
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    /// No rejected upload may leave a job directory behind. Owner-level empty
    /// directories (`users/<owner>/jobs`) are fine; their contents are not.
    async fn assert_no_job_dirs(state: &AppState) {
        let users = state.config.data_dir.join("users");
        if !users.exists() {
            return;
        }
        let mut owners = tokio::fs::read_dir(&users).await.unwrap();
        while let Some(owner_dir) = owners.next_entry().await.unwrap() {
            let mut entries = tokio::fs::read_dir(owner_dir.path().join("jobs"))
                .await
                .unwrap();
            let mut leftover = 0;
            while entries.next_entry().await.unwrap().is_some() {
                leftover += 1;
            }
            assert_eq!(leftover, 0, "job directory leaked after rejected upload");
        }
    }

    #[tokio::test]
    async fn create_job_rejects_oversized_text_fields_and_cleans_up() {
        let base = env::temp_dir().join(format!("ferryman-field-test-{}", Uuid::new_v4()));
        let state = test_app_state(&base).await;
        let boundary = "X-TEST-BOUNDARY";
        let huge_target = "x".repeat(MAX_TEXT_FIELD_BYTES + 1);
        let body = format!(
        "--{b}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"book.txt\"\r\n\r\nhello\r\n--{b}\r\ncontent-disposition: form-data; name=\"target\"\r\n\r\n{target}\r\n--{b}--\r\n",
        b = boundary,
        target = huge_target
    );
        let multipart = multipart_from_body(body, boundary).await;
        let response = create_job(State(state.clone()), HeaderMap::new(), multipart).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_no_job_dirs(&state).await;
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[tokio::test]
    async fn create_job_rejects_truncated_multipart_bodies() {
        let base = env::temp_dir().join(format!("ferryman-truncated-test-{}", Uuid::new_v4()));
        let state = test_app_state(&base).await;
        let boundary = "X-TEST-BOUNDARY";
        // No terminating boundary: the body is truncated mid-field.
        let body = format!(
        "--{b}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"book.txt\"\r\n\r\nhel",
        b = boundary
    );
        let multipart = multipart_from_body(body, boundary).await;
        let response = create_job(State(state.clone()), HeaderMap::new(), multipart).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_no_job_dirs(&state).await;
        tokio::fs::remove_dir_all(&base).await.unwrap();
    }
}
