use ferryman::preset::Preset;
use futures::StreamExt;
use reqwest::{header, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const DOWNLOAD_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const BENCHMARK_BYTES: u64 = 8 * 1024 * 1024;
const BENCHMARK_OFFSET: u64 = 64 * 1024 * 1024;
const BENCHMARK_CACHE_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SourceId {
    Auto,
    Modelscope,
    HfMirror,
    Huggingface,
}

impl SourceId {
    pub fn concrete() -> [Self; 3] {
        [Self::Modelscope, Self::HfMirror, Self::Huggingface]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动选择",
            Self::Modelscope => "ModelScope",
            Self::HfMirror => "HF Mirror",
            Self::Huggingface => "Hugging Face",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelPhase {
    Absent,
    Benchmarking,
    Downloading,
    Paused,
    Verifying,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStatus {
    pub preset: Preset,
    pub state: ModelPhase,
    pub expected_bytes: u64,
    pub downloaded_bytes: u64,
    pub bytes_per_second: u64,
    pub estimated_remaining_seconds: Option<u64>,
    pub source: Option<SourceId>,
    pub source_label: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkPhase {
    Idle,
    Running,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceBenchmark {
    pub source: SourceId,
    pub label: String,
    pub available: bool,
    pub latency_ms: Option<u64>,
    pub bytes_per_second: Option<u64>,
    pub range_supported: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkStatus {
    pub state: BenchmarkPhase,
    pub results: Vec<SourceBenchmark>,
    pub recommended: Option<SourceId>,
    pub tested_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelStatus>,
    pub available_bytes: u64,
    pub benchmark: BenchmarkStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageStatus {
    pub available_bytes: u64,
    pub model_bytes: u64,
    pub partial_bytes: u64,
    pub cache_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DownloadRequest {
    #[serde(default = "default_source")]
    pub source: SourceId,
}

fn default_source() -> SourceId {
    SourceId::Auto
}

#[derive(Clone)]
pub struct ModelManager {
    inner: Arc<Inner>,
}

struct Inner {
    model_root: PathBuf,
    cache_root: PathBuf,
    client: Client,
    models: RwLock<HashMap<Preset, ModelStatus>>,
    downloads: Mutex<HashMap<Preset, CancellationToken>>,
    benchmark: RwLock<BenchmarkStatus>,
    benchmark_cancel: Mutex<Option<CancellationToken>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RemoteFile {
    path: String,
    size: u64,
    sha256: Option<String>,
}

#[derive(Serialize)]
struct InstalledMarker<'a> {
    preset: Preset,
    source: SourceId,
    huggingface_revision: &'a str,
    installed_at: u64,
    files: &'a [RemoteFile],
}

impl ModelManager {
    pub async fn new(model_root: PathBuf, cache_root: PathBuf) -> Result<Self, String> {
        tokio::fs::create_dir_all(&model_root)
            .await
            .map_err(|error| format!("create model directory: {error}"))?;
        tokio::fs::create_dir_all(model_root.join(".downloads"))
            .await
            .map_err(|error| format!("create download directory: {error}"))?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("ferryman/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|error| format!("create model HTTP client: {error}"))?;
        let manager = Self {
            inner: Arc::new(Inner {
                model_root,
                cache_root,
                client,
                models: RwLock::new(HashMap::new()),
                downloads: Mutex::new(HashMap::new()),
                benchmark: RwLock::new(BenchmarkStatus {
                    state: BenchmarkPhase::Idle,
                    results: Vec::new(),
                    recommended: None,
                    tested_at: None,
                }),
                benchmark_cancel: Mutex::new(None),
            }),
        };
        manager.rescan().await;
        Ok(manager)
    }

    pub async fn rescan(&self) {
        let mut statuses = HashMap::new();
        for preset in [Preset::SevenBFp8, Preset::ThirtyBFp8] {
            statuses.insert(preset, self.scan_model(preset).await);
        }
        *self.inner.models.write().await = statuses;
    }

    pub async fn catalog(&self) -> ModelCatalog {
        let models = self.inner.models.read().await;
        let mut values: Vec<_> = models.values().cloned().collect();
        values.sort_by_key(|status| status.preset.as_str());
        ModelCatalog {
            models: values,
            available_bytes: available_bytes(&self.inner.model_root),
            benchmark: self.inner.benchmark.read().await.clone(),
        }
    }

    pub async fn status(&self, preset: Preset) -> ModelStatus {
        self.inner
            .models
            .read()
            .await
            .get(&preset)
            .cloned()
            .unwrap_or_else(|| initial_status(preset, ModelPhase::Absent, 0))
    }

    pub async fn is_ready(&self, preset: Preset) -> bool {
        self.status(preset).await.state == ModelPhase::Ready
    }

    pub async fn ensure_download(&self, preset: Preset) -> Result<(), String> {
        let status = self.status(preset).await;
        match status.state {
            ModelPhase::Ready
            | ModelPhase::Benchmarking
            | ModelPhase::Downloading
            | ModelPhase::Verifying => Ok(()),
            _ => self.start_download(preset, SourceId::Auto).await,
        }
    }

    pub async fn start_download(&self, preset: Preset, source: SourceId) -> Result<(), String> {
        if self.is_ready(preset).await {
            return Ok(());
        }
        let mut downloads = self.inner.downloads.lock().await;
        if downloads.contains_key(&preset) {
            return Ok(());
        }
        let cancel = CancellationToken::new();
        downloads.insert(preset, cancel.clone());
        drop(downloads);

        self.update_model(preset, |status| {
            status.state = if source == SourceId::Auto {
                ModelPhase::Benchmarking
            } else {
                ModelPhase::Downloading
            };
            status.source = None;
            status.source_label = None;
            status.last_error = None;
        })
        .await;

        let manager = self.clone();
        tokio::spawn(async move {
            let result = manager.download_model(preset, source, cancel.clone()).await;
            manager.inner.downloads.lock().await.remove(&preset);
            if let Err(error) = result {
                let cancelled = cancel.is_cancelled();
                let downloaded = manager.incomplete_downloaded_bytes(preset).await;
                manager
                    .update_model(preset, |status| {
                        status.state = if cancelled {
                            ModelPhase::Paused
                        } else {
                            ModelPhase::Failed
                        };
                        status.downloaded_bytes = downloaded;
                        status.last_error = if cancelled { None } else { Some(error) };
                        status.bytes_per_second = 0;
                        status.estimated_remaining_seconds = None;
                    })
                    .await;
            }
        });
        Ok(())
    }

    pub async fn pause_download(&self, preset: Preset) -> Result<(), String> {
        let downloads = self.inner.downloads.lock().await;
        let Some(cancel) = downloads.get(&preset) else {
            return Err("model is not downloading".to_string());
        };
        cancel.cancel();
        Ok(())
    }

    pub async fn delete_model(&self, preset: Preset) -> Result<(), String> {
        if self.inner.downloads.lock().await.contains_key(&preset) {
            return Err("pause the download before deleting the model".to_string());
        }
        let cfg = preset.config();
        remove_path(&self.inner.model_root.join(cfg.model_dir_name)).await?;
        remove_path(&self.download_dir(preset)).await?;
        self.update_model(preset, |status| {
            *status = initial_status(preset, ModelPhase::Absent, 0);
        })
        .await;
        Ok(())
    }

    pub async fn storage_status(&self) -> StorageStatus {
        let root = self.inner.model_root.clone();
        let downloads = root.join(".downloads");
        let cache = self.inner.cache_root.clone();
        let (model_bytes, partial_bytes, cache_bytes) = tokio::task::spawn_blocking(move || {
            (
                directory_size_without(&root, Some(&downloads)),
                directory_size(&downloads),
                directory_size(&cache),
            )
        })
        .await
        .unwrap_or_default();
        StorageStatus {
            available_bytes: available_bytes(&self.inner.model_root),
            model_bytes,
            partial_bytes,
            cache_bytes,
        }
    }

    pub async fn clear_cache(&self) -> Result<(), String> {
        let root = self.inner.cache_root.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if !root.exists() {
                fs::create_dir_all(&root).map_err(|error| error.to_string())?;
                return Ok(());
            }
            for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.is_dir() {
                    fs::remove_dir_all(path).map_err(|error| error.to_string())?;
                } else {
                    fs::remove_file(path).map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub async fn start_benchmark(&self, preset: Preset) -> Result<(), String> {
        let mut active = self.inner.benchmark_cancel.lock().await;
        if active.is_some() {
            return Ok(());
        }
        let cancel = CancellationToken::new();
        *active = Some(cancel.clone());
        drop(active);
        *self.inner.benchmark.write().await = BenchmarkStatus {
            state: BenchmarkPhase::Running,
            results: Vec::new(),
            recommended: None,
            tested_at: None,
        };
        let manager = self.clone();
        tokio::spawn(async move {
            let _ = manager.run_benchmark(preset, cancel).await;
            *manager.inner.benchmark_cancel.lock().await = None;
        });
        Ok(())
    }

    pub async fn cancel_benchmark(&self) {
        if let Some(cancel) = self.inner.benchmark_cancel.lock().await.as_ref() {
            cancel.cancel();
        }
    }

    async fn scan_model(&self, preset: Preset) -> ModelStatus {
        let model_path = self.inner.model_root.join(preset.config().model_dir_name);
        match validate_model_directory(model_path.clone()).await {
            Ok(bytes) => initial_status(preset, ModelPhase::Ready, bytes),
            Err(_) => {
                let partial = self.incomplete_downloaded_bytes(preset).await;
                let phase = if partial > 0 {
                    ModelPhase::Paused
                } else {
                    ModelPhase::Absent
                };
                initial_status(preset, phase, partial)
            }
        }
    }

    async fn download_model(
        &self,
        preset: Preset,
        requested_source: SourceId,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let mut sources = if requested_source == SourceId::Auto {
            self.preferred_sources(preset, &cancel).await
        } else {
            let mut values = vec![requested_source];
            values.extend(
                SourceId::concrete()
                    .into_iter()
                    .filter(|source| *source != requested_source),
            );
            values
        };
        sources.dedup();
        let mut errors = Vec::new();
        for source in sources {
            if cancel.is_cancelled() {
                return Err("download paused".to_string());
            }
            self.update_model(preset, |status| {
                status.state = ModelPhase::Downloading;
                status.source = Some(source);
                status.source_label = Some(source.label().to_string());
                status.last_error = None;
            })
            .await;
            match self.download_from_source(preset, source, &cancel).await {
                Ok(files) => {
                    self.update_model(preset, |status| {
                        status.state = ModelPhase::Verifying;
                        status.bytes_per_second = 0;
                        status.estimated_remaining_seconds = None;
                    })
                    .await;
                    let model_path = self.inner.model_root.join(preset.config().model_dir_name);
                    let bytes = validate_model_directory(model_path.clone()).await?;
                    self.write_marker(preset, source, &files).await?;
                    self.update_model(preset, |status| {
                        status.state = ModelPhase::Ready;
                        status.downloaded_bytes = bytes;
                        status.last_error = None;
                    })
                    .await;
                    info!(%preset, ?source, bytes, "model download complete");
                    return Ok(());
                }
                Err(error) => {
                    warn!(%preset, ?source, %error, "model source failed");
                    errors.push(format!("{}: {error}", source.label()));
                }
            }
        }
        Err(errors.join("; "))
    }

    async fn preferred_sources(&self, preset: Preset, cancel: &CancellationToken) -> Vec<SourceId> {
        let cached = self.inner.benchmark.read().await.clone();
        let fresh = cached.tested_at.is_some_and(|tested| {
            now_epoch_seconds().saturating_sub(tested) < BENCHMARK_CACHE_SECONDS
        });
        let benchmark = if fresh && cached.state == BenchmarkPhase::Complete {
            cached
        } else {
            let _ = self.run_benchmark(preset, cancel.clone()).await;
            self.inner.benchmark.read().await.clone()
        };
        let mut sources = Vec::new();
        if let Some(recommended) = benchmark.recommended {
            sources.push(recommended);
        }
        let mut measured: Vec<_> = benchmark
            .results
            .into_iter()
            .filter(|result| result.available)
            .collect();
        measured.sort_by_key(|result| std::cmp::Reverse(result.bytes_per_second.unwrap_or(0)));
        sources.extend(measured.into_iter().map(|result| result.source));
        sources.extend(SourceId::concrete());
        sources.dedup();
        sources
    }

    async fn download_from_source(
        &self,
        preset: Preset,
        source: SourceId,
        cancel: &CancellationToken,
    ) -> Result<Vec<RemoteFile>, String> {
        let files = self.fetch_manifest(preset, source).await?;
        let total: u64 = files.iter().map(|file| file.size).sum();
        let expected = preset.config().download_bytes;
        if total.abs_diff(expected) > 1024 * 1024 {
            return Err(format!(
                "unexpected model size: source reports {total} bytes, expected about {expected}"
            ));
        }
        let existing = self.current_downloaded_bytes(preset, &files).await;
        let remaining = total.saturating_sub(existing);
        let available = available_bytes(&self.inner.model_root);
        if available < remaining.saturating_add(DOWNLOAD_RESERVE_BYTES) {
            return Err(format!(
                "insufficient space: need {} bytes plus reserve, {} bytes available",
                remaining, available
            ));
        }
        self.update_model(preset, |status| {
            status.expected_bytes = total;
            status.downloaded_bytes = existing;
        })
        .await;
        let started = Instant::now();
        let mut transferred = 0_u64;
        for file in &files {
            if cancel.is_cancelled() {
                return Err("download paused".to_string());
            }
            self.download_file(
                preset,
                source,
                file,
                cancel,
                started,
                &mut transferred,
                total,
            )
            .await?;
        }
        Ok(files)
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_file(
        &self,
        preset: Preset,
        source: SourceId,
        file: &RemoteFile,
        cancel: &CancellationToken,
        started: Instant,
        transferred: &mut u64,
        total: u64,
    ) -> Result<(), String> {
        let final_path = self
            .inner
            .model_root
            .join(preset.config().model_dir_name)
            .join(&file.path);
        if tokio::fs::metadata(&final_path)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.size)
        {
            return Ok(());
        }
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        let part_path = self
            .download_dir(preset)
            .join(format!("{}.partial", file.path));
        if let Some(parent) = part_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut offset = tokio::fs::metadata(&part_path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if offset > file.size {
            tokio::fs::remove_file(&part_path)
                .await
                .map_err(|error| error.to_string())?;
            offset = 0;
        }
        if offset == file.size {
            if let Some(expected_hash) = &file.sha256 {
                let actual = sha256_file(part_path.clone()).await?;
                if !actual.eq_ignore_ascii_case(expected_hash) {
                    tokio::fs::remove_file(&part_path).await.ok();
                    return Err(format!("checksum mismatch for {}", file.path));
                }
            }
            tokio::fs::rename(&part_path, &final_path)
                .await
                .map_err(|error| format!("install {}: {error}", file.path))?;
            return Ok(());
        }
        let url = source_file_url(source, preset, &file.path)?;
        let mut request = self.inner.client.get(url);
        if offset > 0 {
            request = request.header(header::RANGE, format!("bytes={offset}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("download {}: {error}", file.path))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("download {} returned {status}", file.path));
        }
        let append = offset > 0 && status == StatusCode::PARTIAL_CONTENT;
        if append && !content_range_starts_at(response.headers(), offset) {
            return Err(format!(
                "download {} returned an invalid content range",
                file.path
            ));
        }
        if offset > 0 && !append {
            let discarded = offset;
            offset = 0;
            self.update_model(preset, |status| {
                status.downloaded_bytes = status.downloaded_bytes.saturating_sub(discarded);
            })
            .await;
        }
        let mut output = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&part_path)
            .await
            .map_err(|error| error.to_string())?;
        let mut stream = response.bytes_stream();
        let mut written = offset;
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => return Err("download paused".to_string()),
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.map_err(|error| format!("download {}: {error}", file.path))?;
            output
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
            written = written.saturating_add(chunk.len() as u64);
            *transferred = transferred.saturating_add(chunk.len() as u64);
            let elapsed = started.elapsed().as_secs_f64().max(0.25);
            self.update_model(preset, |status| {
                status.downloaded_bytes = status
                    .downloaded_bytes
                    .saturating_add(chunk.len() as u64)
                    .min(total);
                let speed = (*transferred as f64 / elapsed) as u64;
                status.bytes_per_second = speed;
                status.estimated_remaining_seconds = (speed > 0)
                    .then(|| total.saturating_sub(status.downloaded_bytes) / speed.max(1));
            })
            .await;
        }
        output.flush().await.map_err(|error| error.to_string())?;
        drop(output);
        if written != file.size {
            return Err(format!(
                "download {} incomplete: wrote {written}, expected {}",
                file.path, file.size
            ));
        }
        if let Some(expected_hash) = &file.sha256 {
            let actual = sha256_file(part_path.clone()).await?;
            if !actual.eq_ignore_ascii_case(expected_hash) {
                tokio::fs::remove_file(&part_path).await.ok();
                return Err(format!("checksum mismatch for {}", file.path));
            }
        }
        tokio::fs::rename(&part_path, &final_path)
            .await
            .map_err(|error| format!("install {}: {error}", file.path))?;
        Ok(())
    }

    async fn fetch_manifest(
        &self,
        preset: Preset,
        source: SourceId,
    ) -> Result<Vec<RemoteFile>, String> {
        match source {
            SourceId::Modelscope => self.fetch_modelscope_manifest(preset).await,
            SourceId::HfMirror | SourceId::Huggingface => {
                self.fetch_huggingface_manifest(preset, source).await
            }
            SourceId::Auto => Err("auto is not a concrete source".to_string()),
        }
    }

    async fn fetch_huggingface_manifest(
        &self,
        preset: Preset,
        source: SourceId,
    ) -> Result<Vec<RemoteFile>, String> {
        let cfg = preset.config();
        let base = match source {
            SourceId::HfMirror => "https://hf-mirror.com",
            _ => "https://huggingface.co",
        };
        let url = format!(
            "{base}/api/models/{}/revision/{}?blobs=true",
            cfg.huggingface_repo, cfg.huggingface_revision
        );
        let value: Value = self
            .inner
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("read source manifest: {error}"))?
            .error_for_status()
            .map_err(|error| format!("read source manifest: {error}"))?
            .json()
            .await
            .map_err(|error| format!("parse source manifest: {error}"))?;
        let siblings = value["siblings"]
            .as_array()
            .ok_or_else(|| "source manifest has no files".to_string())?;
        let mut files = Vec::new();
        for item in siblings {
            let Some(path) = item["rfilename"].as_str() else {
                continue;
            };
            if !is_runtime_file(path) {
                continue;
            }
            let size = item["size"]
                .as_u64()
                .or_else(|| item["lfs"]["size"].as_u64())
                .ok_or_else(|| format!("source did not report size for {path}"))?;
            files.push(RemoteFile {
                path: path.to_string(),
                size,
                sha256: item["lfs"]["sha256"].as_str().map(str::to_string),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    async fn fetch_modelscope_manifest(&self, preset: Preset) -> Result<Vec<RemoteFile>, String> {
        let repo = preset.config().modelscope_repo;
        let url = format!(
            "https://modelscope.cn/api/v1/models/{repo}/repo/files?Revision=master&Recursive=true"
        );
        let value: Value = self
            .inner
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("read ModelScope manifest: {error}"))?
            .error_for_status()
            .map_err(|error| format!("read ModelScope manifest: {error}"))?
            .json()
            .await
            .map_err(|error| format!("parse ModelScope manifest: {error}"))?;
        let entries = value["Data"]["Files"]
            .as_array()
            .ok_or_else(|| "ModelScope manifest has no files".to_string())?;
        let mut files = Vec::new();
        for item in entries {
            let Some(path) = item["Path"].as_str() else {
                continue;
            };
            if !is_runtime_file(path) {
                continue;
            }
            let size = item["Size"]
                .as_u64()
                .or_else(|| item["Size"].as_str().and_then(|value| value.parse().ok()))
                .ok_or_else(|| format!("ModelScope did not report size for {path}"))?;
            files.push(RemoteFile {
                path: path.to_string(),
                size,
                sha256: item["Sha256"].as_str().map(str::to_string),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    async fn run_benchmark(&self, preset: Preset, cancel: CancellationToken) -> Result<(), String> {
        *self.inner.benchmark.write().await = BenchmarkStatus {
            state: BenchmarkPhase::Running,
            results: Vec::new(),
            recommended: None,
            tested_at: None,
        };
        let futures = SourceId::concrete().into_iter().map(|source| {
            let manager = self.clone();
            let cancel = cancel.clone();
            async move { manager.benchmark_source(preset, source, cancel).await }
        });
        let results = futures::future::join_all(futures).await;
        if cancel.is_cancelled() {
            *self.inner.benchmark.write().await = BenchmarkStatus {
                state: BenchmarkPhase::Idle,
                results: Vec::new(),
                recommended: None,
                tested_at: None,
            };
            return Err("benchmark cancelled".to_string());
        }
        let recommended = recommended_source(&results);
        let state = if recommended.is_some() {
            BenchmarkPhase::Complete
        } else {
            BenchmarkPhase::Failed
        };
        *self.inner.benchmark.write().await = BenchmarkStatus {
            state,
            results,
            recommended,
            tested_at: Some(now_epoch_seconds()),
        };
        Ok(())
    }

    async fn benchmark_source(
        &self,
        preset: Preset,
        source: SourceId,
        cancel: CancellationToken,
    ) -> SourceBenchmark {
        let shard = if preset == Preset::SevenBFp8 {
            "model-00002-of-00002.safetensors"
        } else {
            "model-00000-of-00051.safetensors"
        };
        let url = match source_file_url(source, preset, shard) {
            Ok(url) => url,
            Err(error) => return benchmark_error(source, error),
        };
        let started = Instant::now();
        let response = tokio::select! {
            _ = cancel.cancelled() => return benchmark_error(source, "cancelled".to_string()),
            response = self.inner.client
                .get(url)
                .header(header::RANGE, format!("bytes={}-{}", BENCHMARK_OFFSET, BENCHMARK_OFFSET + BENCHMARK_BYTES - 1))
                .timeout(Duration::from_secs(20))
                .send() => response,
        };
        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => return benchmark_error(source, format!("HTTP {}", response.status())),
            Err(error) => return benchmark_error(source, error.to_string()),
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        let range_supported = response.status() == StatusCode::PARTIAL_CONTENT;
        let transfer_started = Instant::now();
        let mut bytes = 0_u64;
        let mut stream = response.bytes_stream();
        while bytes < BENCHMARK_BYTES {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return benchmark_error(source, "cancelled".to_string()),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            match chunk {
                Ok(chunk) => bytes = bytes.saturating_add(chunk.len() as u64),
                Err(error) => return benchmark_error(source, error.to_string()),
            }
        }
        let elapsed = transfer_started.elapsed().as_secs_f64().max(0.001);
        let complete = bytes >= BENCHMARK_BYTES;
        let speed = (bytes.min(BENCHMARK_BYTES) as f64 / elapsed) as u64;
        SourceBenchmark {
            source,
            label: source.label().to_string(),
            available: complete,
            latency_ms: Some(latency_ms),
            bytes_per_second: Some(speed),
            range_supported,
            error: (!complete).then(|| format!("only received {bytes} benchmark bytes")),
        }
    }

    async fn current_downloaded_bytes(&self, preset: Preset, files: &[RemoteFile]) -> u64 {
        let model_dir = self.inner.model_root.join(preset.config().model_dir_name);
        let download_dir = self.download_dir(preset);
        if files.is_empty() {
            return directory_size_async(model_dir).await
                + directory_size_async(download_dir).await;
        }
        let mut total = 0_u64;
        for file in files {
            let final_size = tokio::fs::metadata(model_dir.join(&file.path))
                .await
                .map(|metadata| metadata.len().min(file.size))
                .unwrap_or(0);
            if final_size == file.size {
                total = total.saturating_add(final_size);
            } else {
                total = total.saturating_add(
                    tokio::fs::metadata(download_dir.join(format!("{}.partial", file.path)))
                        .await
                        .map(|metadata| metadata.len().min(file.size))
                        .unwrap_or(0),
                );
            }
        }
        total
    }

    async fn incomplete_downloaded_bytes(&self, preset: Preset) -> u64 {
        let installed =
            directory_size_async(self.inner.model_root.join(preset.config().model_dir_name)).await;
        let partial = directory_size_async(self.download_dir(preset)).await;
        installed
            .saturating_add(partial)
            .min(preset.config().download_bytes)
    }

    async fn write_marker(
        &self,
        preset: Preset,
        source: SourceId,
        files: &[RemoteFile],
    ) -> Result<(), String> {
        let cfg = preset.config();
        let dir = self.inner.model_root.join(cfg.model_dir_name);
        let marker = InstalledMarker {
            preset,
            source,
            huggingface_revision: cfg.huggingface_revision,
            installed_at: now_epoch_seconds(),
            files,
        };
        let body = serde_json::to_vec_pretty(&marker).map_err(|error| error.to_string())?;
        let temporary = dir.join(".ferryman-model.json.tmp");
        let final_path = dir.join(".ferryman-model.json");
        tokio::fs::write(&temporary, body)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::rename(temporary, final_path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn update_model(&self, preset: Preset, update: impl FnOnce(&mut ModelStatus)) {
        let mut models = self.inner.models.write().await;
        let status = models
            .entry(preset)
            .or_insert_with(|| initial_status(preset, ModelPhase::Absent, 0));
        update(status);
        status.updated_at = now_epoch_seconds();
    }

    fn download_dir(&self, preset: Preset) -> PathBuf {
        self.inner
            .model_root
            .join(".downloads")
            .join(preset.as_str())
    }
}

fn source_file_url(source: SourceId, preset: Preset, path: &str) -> Result<Url, String> {
    let cfg = preset.config();
    match source {
        SourceId::Huggingface | SourceId::HfMirror => {
            let base = if source == SourceId::HfMirror {
                "https://hf-mirror.com"
            } else {
                "https://huggingface.co"
            };
            Url::parse(&format!(
                "{base}/{}/resolve/{}/{}",
                cfg.huggingface_repo, cfg.huggingface_revision, path
            ))
            .map_err(|error| error.to_string())
        }
        SourceId::Modelscope => {
            let mut url = Url::parse(&format!(
                "https://modelscope.cn/api/v1/models/{}/repo",
                cfg.modelscope_repo
            ))
            .map_err(|error| error.to_string())?;
            url.query_pairs_mut()
                .append_pair("Revision", "master")
                .append_pair("FilePath", path);
            Ok(url)
        }
        SourceId::Auto => Err("auto is not a concrete source".to_string()),
    }
}

fn content_range_starts_at(headers: &header::HeaderMap, offset: u64) -> bool {
    headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(&format!("bytes {offset}-")))
}

fn is_runtime_file(path: &str) -> bool {
    !path.contains('/')
        && (path.ends_with(".safetensors")
            || path.ends_with(".json")
            || path == "chat_template.jinja")
}

fn recommended_source(results: &[SourceBenchmark]) -> Option<SourceId> {
    let fastest = results
        .iter()
        .filter(|result| result.available)
        .max_by_key(|result| result.bytes_per_second.unwrap_or(0))?;
    let fastest_speed = fastest.bytes_per_second.unwrap_or(0);
    let modelscope = results
        .iter()
        .find(|result| result.source == SourceId::Modelscope && result.available);
    if modelscope.is_some_and(|result| {
        result.bytes_per_second.unwrap_or(0) >= fastest_speed.saturating_mul(85) / 100
    }) {
        Some(SourceId::Modelscope)
    } else {
        Some(fastest.source)
    }
}

fn benchmark_error(source: SourceId, error: String) -> SourceBenchmark {
    SourceBenchmark {
        source,
        label: source.label().to_string(),
        available: false,
        latency_ms: None,
        bytes_per_second: None,
        range_supported: false,
        error: Some(error),
    }
}

fn initial_status(preset: Preset, state: ModelPhase, downloaded_bytes: u64) -> ModelStatus {
    ModelStatus {
        preset,
        state,
        expected_bytes: preset.config().download_bytes,
        downloaded_bytes,
        bytes_per_second: 0,
        estimated_remaining_seconds: None,
        source: None,
        source_label: None,
        last_error: None,
        updated_at: now_epoch_seconds(),
    }
}

async fn validate_model_directory(path: PathBuf) -> Result<u64, String> {
    tokio::task::spawn_blocking(move || -> Result<u64, String> {
        for required in [
            "config.json",
            "tokenizer.json",
            "model.safetensors.index.json",
        ] {
            let metadata =
                fs::metadata(path.join(required)).map_err(|_| format!("missing {required}"))?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(format!("invalid {required}"));
            }
        }
        let index = fs::read(path.join("model.safetensors.index.json"))
            .map_err(|error| format!("read model index: {error}"))?;
        let value: Value = serde_json::from_slice(&index)
            .map_err(|error| format!("parse model index: {error}"))?;
        let weights = value["weight_map"]
            .as_object()
            .ok_or_else(|| "model index has no weight map".to_string())?;
        let mut shards: Vec<_> = weights.values().filter_map(Value::as_str).collect();
        shards.sort_unstable();
        shards.dedup();
        if shards.is_empty() {
            return Err("model index has no shards".to_string());
        }
        for shard in shards {
            let metadata = fs::metadata(path.join(shard))
                .map_err(|_| format!("missing model shard {shard}"))?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(format!("invalid model shard {shard}"));
            }
        }
        Ok(directory_size(&path))
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn sha256_file(path: PathBuf) -> Result<String, String> {
    let mut input = tokio::fs::File::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

async fn remove_path(path: &Path) -> Result<(), String> {
    let Ok(metadata) = tokio::fs::symlink_metadata(path).await else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        tokio::fs::remove_file(path)
            .await
            .map_err(|error| error.to_string())
    } else {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|error| error.to_string())
    }
}

fn available_bytes(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    let stats = unsafe { stats.assume_init() };
    stats.f_bavail.saturating_mul(stats.f_frsize)
}

async fn directory_size_async(path: PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || directory_size(&path))
        .await
        .unwrap_or(0)
}

fn directory_size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

fn directory_size_without(path: &Path, excluded: Option<&Path>) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| excluded != Some(entry.path().as_path()))
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modelscope_wins_near_ties() {
        let results = vec![
            SourceBenchmark {
                source: SourceId::Modelscope,
                label: "ModelScope".into(),
                available: true,
                latency_ms: Some(20),
                bytes_per_second: Some(90),
                range_supported: true,
                error: None,
            },
            SourceBenchmark {
                source: SourceId::HfMirror,
                label: "HF Mirror".into(),
                available: true,
                latency_ms: Some(10),
                bytes_per_second: Some(100),
                range_supported: true,
                error: None,
            },
        ];
        assert_eq!(recommended_source(&results), Some(SourceId::Modelscope));
    }

    #[tokio::test]
    async fn validates_complete_model_structure() {
        let root =
            std::env::temp_dir().join(format!("ferryman-model-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("config.json"), b"{}").unwrap();
        fs::write(root.join("tokenizer.json"), b"{}").unwrap();
        fs::write(root.join("model.safetensors"), b"weights").unwrap();
        fs::write(
            root.join("model.safetensors.index.json"),
            br#"{"weight_map":{"model.layer":"model.safetensors"}}"#,
        )
        .unwrap();
        assert!(validate_model_directory(root.clone()).await.is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
