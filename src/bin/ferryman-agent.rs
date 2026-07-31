use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ferryman::preset::Preset;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::path::Path as FsPath;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    controller: Controller,
    client: reqwest::Client,
    token: Arc<str>,
    vllm_endpoint: Arc<str>,
}

struct ActiveRequestGuard {
    active_requests: Arc<AtomicUsize>,
}

impl ActiveRequestGuard {
    fn new(active_requests: Arc<AtomicUsize>) -> Self {
        active_requests.fetch_add(1, Ordering::AcqRel);
        Self { active_requests }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
struct Controller {
    commands: mpsc::Sender<RuntimeCommand>,
    status: watch::Receiver<RuntimeStatus>,
    active_requests: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimePhase {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StartupStage {
    StartingProcess,
    LoadingWeights,
    CompilingKernels,
    CapturingGraphs,
    StartingServer,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeStatus {
    state: RuntimePhase,
    preset: Option<Preset>,
    pid: Option<u32>,
    active_requests: usize,
    leases: usize,
    idle_timeout_seconds: u64,
    updated_at: u64,
    last_error: Option<String>,
    startup_stage: Option<StartupStage>,
    startup_progress: u8,
    startup_elapsed_seconds: Option<u64>,
    estimated_remaining_seconds: Option<u64>,
    recent_logs: Vec<String>,
}

#[derive(Deserialize)]
struct AcquireRequest {
    preset: Preset,
    lease_id: String,
    #[serde(default = "default_lease_ttl")]
    ttl_seconds: u64,
}

#[derive(Deserialize)]
struct StopRequest {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

struct Lease {
    preset: Preset,
    expires_at: Instant,
}

enum RuntimeCommand {
    Acquire {
        request: AcquireRequest,
        reply: oneshot::Sender<Result<RuntimeStatus, String>>,
    },
    Release {
        lease_id: String,
        reply: oneshot::Sender<RuntimeStatus>,
    },
    Stop {
        force: bool,
        reply: oneshot::Sender<Result<RuntimeStatus, String>>,
    },
    Touch,
    VllmLog {
        pid: u32,
        line: String,
    },
}

struct RuntimeManager {
    commands: mpsc::Sender<RuntimeCommand>,
    receiver: mpsc::Receiver<RuntimeCommand>,
    status_tx: watch::Sender<RuntimeStatus>,
    active_requests: Arc<AtomicUsize>,
    phase: RuntimePhase,
    preset: Option<Preset>,
    child: Option<Child>,
    leases: HashMap<String, Lease>,
    last_activity: Instant,
    last_health_check: Instant,
    started_at: Option<Instant>,
    idle_timeout: Duration,
    start_timeout: Duration,
    vllm_bin: String,
    model_root: String,
    vllm_endpoint: String,
    client: reqwest::Client,
    last_error: Option<String>,
    startup_stage: Option<StartupStage>,
    startup_progress: u8,
    startup_elapsed_seconds: Option<u64>,
    recent_logs: VecDeque<String>,
}

const MAX_RECENT_LOGS: usize = 40;
const MAX_LOG_LINE_CHARS: usize = 600;
const EXPECTED_STARTUP_SECONDS: u64 = 360;

fn default_lease_ttl() -> u64 {
    120
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Controller {
    fn spawn(
        idle_timeout: Duration,
        start_timeout: Duration,
        vllm_bin: String,
        model_root: String,
        vllm_endpoint: String,
        client: reqwest::Client,
    ) -> Self {
        let active_requests = Arc::new(AtomicUsize::new(0));
        let initial = RuntimeStatus {
            state: RuntimePhase::Stopped,
            preset: None,
            pid: None,
            active_requests: 0,
            leases: 0,
            idle_timeout_seconds: idle_timeout.as_secs(),
            updated_at: now_epoch_seconds(),
            last_error: None,
            startup_stage: None,
            startup_progress: 0,
            startup_elapsed_seconds: None,
            estimated_remaining_seconds: None,
            recent_logs: Vec::new(),
        };
        let (status_tx, status) = watch::channel(initial);
        let (commands, receiver) = mpsc::channel(64);
        let manager = RuntimeManager {
            commands: commands.clone(),
            receiver,
            status_tx,
            active_requests: active_requests.clone(),
            phase: RuntimePhase::Stopped,
            preset: None,
            child: None,
            leases: HashMap::new(),
            last_activity: Instant::now(),
            last_health_check: Instant::now() - Duration::from_secs(10),
            started_at: None,
            idle_timeout,
            start_timeout,
            vllm_bin,
            model_root,
            vllm_endpoint,
            client,
            last_error: None,
            startup_stage: None,
            startup_progress: 0,
            startup_elapsed_seconds: None,
            recent_logs: VecDeque::with_capacity(MAX_RECENT_LOGS),
        };
        tokio::spawn(manager.run());
        Self {
            commands,
            status,
            active_requests,
        }
    }

    fn snapshot(&self) -> RuntimeStatus {
        let mut status = self.status.borrow().clone();
        status.active_requests = self.active_requests.load(Ordering::Relaxed);
        status
    }

    async fn acquire(&self, request: AcquireRequest) -> Result<RuntimeStatus, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Acquire { request, reply })
            .await
            .map_err(|_| "runtime manager is unavailable".to_string())?;
        result
            .await
            .map_err(|_| "runtime manager stopped before replying".to_string())?
    }

    async fn release(&self, lease_id: String) -> Result<RuntimeStatus, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Release { lease_id, reply })
            .await
            .map_err(|_| "runtime manager is unavailable".to_string())?;
        result
            .await
            .map_err(|_| "runtime manager stopped before replying".to_string())
    }

    async fn stop(&self, force: bool) -> Result<RuntimeStatus, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Stop { force, reply })
            .await
            .map_err(|_| "runtime manager is unavailable".to_string())?;
        result
            .await
            .map_err(|_| "runtime manager stopped before replying".to_string())?
    }

    async fn touch(&self) {
        let _ = self.commands.send(RuntimeCommand::Touch).await;
    }
}

impl RuntimeManager {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = tick.tick() => self.on_tick().await,
                command = self.receiver.recv() => {
                    let Some(command) = command else { break; };
                    self.handle(command).await;
                }
            }
        }
        let _ = self.stop_child(true).await;
    }

    async fn handle(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::Acquire { request, reply } => {
                let result = self.acquire(request).await;
                let _ = reply.send(result);
            }
            RuntimeCommand::Release { lease_id, reply } => {
                self.leases.remove(&lease_id);
                self.last_activity = Instant::now();
                let status = self.publish();
                let _ = reply.send(status);
            }
            RuntimeCommand::Stop { force, reply } => {
                let result = if !force
                    && (self.active_requests.load(Ordering::Acquire) > 0 || !self.leases.is_empty())
                {
                    Err("model has active requests or leases".to_string())
                } else {
                    self.leases.clear();
                    self.stop_child(force).await.map(|_| self.publish())
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Touch => {
                self.last_activity = Instant::now();
                self.publish();
            }
            RuntimeCommand::VllmLog { pid, line } => {
                if self.child.as_ref().and_then(Child::id) != Some(pid) {
                    return;
                }
                self.push_log(line);
                self.publish();
            }
        }
    }

    async fn acquire(&mut self, request: AcquireRequest) -> Result<RuntimeStatus, String> {
        if request.lease_id.is_empty() || request.lease_id.len() > 128 {
            return Err("lease_id must contain 1 to 128 characters".to_string());
        }
        self.prune_leases();
        let ttl = Duration::from_secs(request.ttl_seconds.clamp(30, 3600));

        if let Some(current) = self.preset {
            if current != request.preset {
                let other_leases = self
                    .leases
                    .iter()
                    .any(|(id, lease)| id != &request.lease_id && lease.preset == current);
                if other_leases || self.active_requests.load(Ordering::Acquire) > 0 {
                    return Err(format!(
                        "{} is active; release its leases before switching to {}",
                        current, request.preset
                    ));
                }
                self.leases.remove(&request.lease_id);
                self.stop_child(false).await?;
            }
        }

        self.leases.insert(
            request.lease_id,
            Lease {
                preset: request.preset,
                expires_at: Instant::now() + ttl,
            },
        );
        self.last_activity = Instant::now();

        if self.child.is_none() {
            if let Err(error) = self.start_child(request.preset).await {
                self.leases
                    .retain(|_, lease| lease.preset != request.preset);
                return Err(error);
            }
        }
        Ok(self.publish())
    }

    async fn start_child(&mut self, preset: Preset) -> Result<(), String> {
        let cfg = preset.config();
        let model_path = format!(
            "{}/{}",
            self.model_root.trim_end_matches('/'),
            cfg.model_dir_name
        );
        if !FsPath::new(&model_path).exists() {
            self.phase = RuntimePhase::Failed;
            self.preset = Some(preset);
            self.startup_stage = Some(StartupStage::Failed);
            self.startup_progress = 0;
            self.last_error = Some(format!("model directory not found: {model_path}"));
            self.recent_logs.clear();
            self.recent_logs
                .push_back(self.last_error.clone().unwrap_or_default());
            self.publish();
            return Err(self.last_error.clone().unwrap_or_default());
        }

        let mut command = Command::new(&self.vllm_bin);
        command
            .arg("serve")
            .arg(&model_path)
            .args(["--served-model-name", preset.api_model()])
            .args(["--host", "127.0.0.1", "--port", "8001"])
            .args(["--dtype", cfg.dtype])
            .args(["--kv-cache-dtype", cfg.kv_cache_dtype])
            .args([
                "--gpu-memory-utilization",
                &cfg.gpu_memory_utilization.to_string(),
            ])
            .args(["--max-model-len", &cfg.max_model_len.to_string()])
            .arg("--enable-prefix-caching")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(max_num_seqs) = cfg.max_num_seqs {
            command.args(["--max-num-seqs", &max_num_seqs.to_string()]);
        }
        if cfg.enforce_eager {
            command.arg("--enforce-eager");
        }
        command.env(
            "LD_PRELOAD",
            "/usr/lib/aarch64-linux-gnu/nvidia/libcuda.so.1",
        );
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            let message = format!("start vLLM: {error}");
            self.phase = RuntimePhase::Failed;
            self.preset = Some(preset);
            self.startup_stage = Some(StartupStage::Failed);
            self.last_error = Some(message.clone());
            self.publish();
            message
        })?;
        let pid = child.id().unwrap_or_default();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pipe_logs(stdout, pid, "stdout", self.commands.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pipe_logs(stderr, pid, "stderr", self.commands.clone()));
        }

        info!(%preset, pid, "vLLM process started");
        self.child = Some(child);
        self.phase = RuntimePhase::Starting;
        self.preset = Some(preset);
        self.last_error = None;
        self.startup_stage = Some(StartupStage::StartingProcess);
        self.startup_progress = 5;
        self.startup_elapsed_seconds = None;
        self.recent_logs.clear();
        self.last_health_check = Instant::now() - Duration::from_secs(10);
        self.started_at = Some(Instant::now());
        self.publish();
        Ok(())
    }

    async fn stop_child(&mut self, force: bool) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            self.phase = RuntimePhase::Stopped;
            self.preset = None;
            self.last_error = None;
            self.publish();
            return Ok(());
        };
        self.phase = RuntimePhase::Stopping;
        self.publish();
        let pid = child.id().unwrap_or_default();
        if pid != 0 {
            let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
            unsafe {
                libc::kill(-(pid as i32), signal);
            }
        }
        if tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .is_err()
        {
            if pid != 0 {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.wait().await;
        }
        info!(pid, "vLLM process stopped");
        self.phase = RuntimePhase::Stopped;
        self.preset = None;
        self.last_error = None;
        self.started_at = None;
        self.startup_stage = None;
        self.startup_progress = 0;
        self.startup_elapsed_seconds = None;
        self.recent_logs.clear();
        self.last_activity = Instant::now();
        self.publish();
        Ok(())
    }

    async fn on_tick(&mut self) {
        self.prune_leases();
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.child = None;
                    self.phase = RuntimePhase::Failed;
                    self.last_error = Some(format!("vLLM exited with {status}"));
                    self.startup_elapsed_seconds =
                        self.started_at.map(|started| started.elapsed().as_secs());
                    self.started_at = None;
                    self.startup_stage = Some(StartupStage::Failed);
                    self.leases.clear();
                    self.publish();
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, "could not poll vLLM process");
                }
            }
        }

        if self.phase == RuntimePhase::Starting
            && self.last_health_check.elapsed() >= Duration::from_secs(2)
        {
            self.last_health_check = Instant::now();
            let health = self
                .client
                .get(format!("{}/health", self.vllm_endpoint))
                .timeout(Duration::from_secs(2))
                .send()
                .await;
            if health.is_ok_and(|response| response.status().is_success()) {
                self.phase = RuntimePhase::Ready;
                self.last_error = None;
                self.startup_elapsed_seconds =
                    self.started_at.map(|started| started.elapsed().as_secs());
                self.started_at = None;
                self.startup_stage = Some(StartupStage::Ready);
                self.startup_progress = 100;
                info!(preset = ?self.preset, "vLLM is ready");
                self.publish();
            }
        }

        if self.phase == RuntimePhase::Starting
            && self
                .started_at
                .is_some_and(|started| started.elapsed() >= self.start_timeout)
        {
            let preset = self.preset;
            let recent_logs = self.recent_logs.clone();
            let startup_progress = self.startup_progress;
            let startup_elapsed_seconds =
                self.started_at.map(|started| started.elapsed().as_secs());
            let _ = self.stop_child(true).await;
            self.phase = RuntimePhase::Failed;
            self.preset = preset;
            self.leases.clear();
            self.startup_stage = Some(StartupStage::Failed);
            self.startup_progress = startup_progress;
            self.startup_elapsed_seconds = startup_elapsed_seconds;
            self.recent_logs = recent_logs;
            self.last_error = Some(format!(
                "vLLM did not become ready within {} seconds",
                self.start_timeout.as_secs()
            ));
            self.publish();
            return;
        }

        let idle = self.leases.is_empty()
            && self.active_requests.load(Ordering::Acquire) == 0
            && self.last_activity.elapsed() >= self.idle_timeout;
        if idle && matches!(self.phase, RuntimePhase::Starting | RuntimePhase::Ready) {
            info!("idle timeout reached; unloading model");
            let _ = self.stop_child(false).await;
        } else {
            self.publish();
        }
    }

    fn prune_leases(&mut self) {
        let now = Instant::now();
        self.leases.retain(|_, lease| lease.expires_at > now);
    }

    fn push_log(&mut self, line: String) {
        let line = sanitize_log_line(&line);
        if line.is_empty() {
            return;
        }
        if self.recent_logs.len() == MAX_RECENT_LOGS {
            self.recent_logs.pop_front();
        }
        self.recent_logs.push_back(line.clone());
        info!(target = "vllm", "{line}");

        if self.phase == RuntimePhase::Starting {
            if let Some((stage, progress)) = parse_startup_signal(&line) {
                if progress >= self.startup_progress {
                    self.startup_stage = Some(stage);
                    self.startup_progress = progress;
                }
            }
        }
    }

    fn publish(&self) -> RuntimeStatus {
        let startup_elapsed_seconds = self
            .started_at
            .map(|started| started.elapsed().as_secs())
            .or(self.startup_elapsed_seconds);
        let estimated_remaining_seconds = if self.phase == RuntimePhase::Starting {
            startup_elapsed_seconds.map(|elapsed| {
                let scheduled = EXPECTED_STARTUP_SECONDS.saturating_sub(elapsed);
                if self.startup_progress <= 5 {
                    scheduled
                } else {
                    let observed = elapsed.saturating_mul(u64::from(100 - self.startup_progress))
                        / u64::from(self.startup_progress);
                    ((scheduled + observed) / 2)
                        .min(self.start_timeout.as_secs().saturating_sub(elapsed))
                }
            })
        } else {
            None
        };
        let status = RuntimeStatus {
            state: self.phase,
            preset: self.preset,
            pid: self.child.as_ref().and_then(Child::id),
            active_requests: self.active_requests.load(Ordering::Relaxed),
            leases: self.leases.len(),
            idle_timeout_seconds: self.idle_timeout.as_secs(),
            updated_at: now_epoch_seconds(),
            last_error: self.last_error.clone(),
            startup_stage: self.startup_stage,
            startup_progress: self.startup_progress,
            startup_elapsed_seconds,
            estimated_remaining_seconds,
            recent_logs: self.recent_logs.iter().cloned().collect(),
        };
        self.status_tx.send_replace(status.clone());
        status
    }
}

async fn pipe_logs<R>(
    mut reader: R,
    pid: u32,
    stream: &'static str,
    commands: mpsc::Sender<RuntimeCommand>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 2048];
    let mut pending = Vec::new();
    loop {
        let count = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                warn!(%error, stream, "could not read vLLM log stream");
                break;
            }
        };
        for byte in &chunk[..count] {
            if matches!(byte, b'\n' | b'\r') {
                if !pending.is_empty() {
                    let line = String::from_utf8_lossy(&pending).into_owned();
                    pending.clear();
                    if commands
                        .send(RuntimeCommand::VllmLog { pid, line })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            } else {
                pending.push(*byte);
                if pending.len() >= 8192 {
                    let line = String::from_utf8_lossy(&pending).into_owned();
                    pending.clear();
                    if commands
                        .send(RuntimeCommand::VllmLog { pid, line })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending).into_owned();
        let _ = commands.send(RuntimeCommand::VllmLog { pid, line }).await;
    }
}

fn sanitize_log_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len().min(MAX_LOG_LINE_CHARS));
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next() == Some('[') {
                for code in chars.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
            continue;
        }
        if !ch.is_control() || ch == '\t' {
            result.push(ch);
            if result.chars().count() >= MAX_LOG_LINE_CHARS {
                result.push_str("...");
                break;
            }
        }
    }
    result.trim().to_string()
}

fn parse_startup_signal(line: &str) -> Option<(StartupStage, u8)> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("loading safetensors checkpoint shards") {
        let percent = extract_percent(&lower).unwrap_or(0);
        let progress = 10 + (u16::from(percent) * 35 / 100) as u8;
        return Some((StartupStage::LoadingWeights, progress));
    }
    if lower.contains("loading model weights took")
        || lower.contains("model loading took")
        || lower.contains("weights loaded")
    {
        return Some((StartupStage::LoadingWeights, 48));
    }
    if lower.contains("cuda graph") || lower.contains("cudagraph") {
        let percent = extract_percent(&lower).unwrap_or(5);
        let progress = 65 + (u16::from(percent) * 30 / 100) as u8;
        return Some((StartupStage::CapturingGraphs, progress));
    }
    if [
        "compil",
        "triton",
        "flashinfer",
        "marlin",
        "torchinductor",
        "ninja",
        "nvcc",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return Some((StartupStage::CompilingKernels, 52));
    }
    if lower.contains("application startup complete")
        || lower.contains("uvicorn running")
        || lower.contains("starting vllm api server")
        || lower.contains("available routes")
    {
        return Some((StartupStage::StartingServer, 98));
    }
    None
}

fn extract_percent(line: &str) -> Option<u8> {
    let percent = line.find('%')?;
    let digits_reversed: String = line[..percent]
        .chars()
        .rev()
        .skip_while(|ch| ch.is_ascii_whitespace())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits_reversed.is_empty() {
        return None;
    }
    let digits: String = digits_reversed.chars().rev().collect();
    digits.parse::<u8>().ok().map(|value| value.min(100))
}

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied == Some(expected) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "missing or invalid bearer token".to_string(),
        }),
    )
        .into_response()
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn runtime_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RuntimeStatus>, Response> {
    authorize(&headers, &state.token).map_err(|_| unauthorized_response())?;
    Ok(Json(state.controller.snapshot()))
}

async fn acquire_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AcquireRequest>,
) -> Response {
    if authorize(&headers, &state.token).is_err() {
        return unauthorized_response();
    }
    match state.controller.acquire(request).await {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(ErrorBody { error })).into_response(),
    }
}

async fn release_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lease_id): Path<String>,
) -> Response {
    if authorize(&headers, &state.token).is_err() {
        return unauthorized_response();
    }
    match state.controller.release(lease_id).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, Json(ErrorBody { error })).into_response(),
    }
}

async fn stop_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StopRequest>,
) -> Response {
    if authorize(&headers, &state.token).is_err() {
        return unauthorized_response();
    }
    match state.controller.stop(request.force).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(ErrorBody { error })).into_response(),
    }
}

async fn proxy_chat(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if authorize(&headers, &state.token).is_err() {
        return unauthorized_response();
    }
    if state.controller.snapshot().state != RuntimePhase::Ready {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "translation model is not ready".to_string(),
            }),
        )
            .into_response();
    }

    let _request_guard = ActiveRequestGuard::new(state.controller.active_requests.clone());
    state.controller.touch().await;
    let response = state
        .client
        .post(format!("{}/v1/chat/completions", state.vllm_endpoint))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;

    let response = match response {
        Ok(response) => {
            let status = response.status();
            let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
            match response.bytes().await {
                Ok(bytes) => {
                    let mut builder = Response::builder().status(status);
                    if let Some(content_type) = content_type {
                        builder = builder.header(header::CONTENT_TYPE, content_type);
                    }
                    builder.body(Body::from(bytes)).unwrap_or_else(|error| {
                        error!(%error, "failed to build proxy response");
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    })
                }
                Err(error) => (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorBody {
                        error: format!("read vLLM response: {error}"),
                    }),
                )
                    .into_response(),
            }
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorBody {
                error: format!("vLLM request failed: {error}"),
            }),
        )
            .into_response(),
    };
    state.controller.touch().await;
    response
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let listen = env::var("FERRYMAN_AGENT_LISTEN").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let token = env::var("FERRYMAN_AGENT_TOKEN")
        .map_err(|_| anyhow::anyhow!("FERRYMAN_AGENT_TOKEN must be set"))?;
    if token.len() < 16 {
        anyhow::bail!("FERRYMAN_AGENT_TOKEN must be at least 16 characters");
    }
    let idle_timeout = env::var("FERRYMAN_IDLE_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600)
        .max(30);
    let start_timeout = env::var("FERRYMAN_START_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600)
        .max(30);
    let vllm_bin = env::var("FERRYMAN_VLLM_BIN").unwrap_or_else(|_| "vllm".into());
    let model_root = env::var("FERRYMAN_MODEL_ROOT").unwrap_or_else(|_| "/models".into());
    let vllm_endpoint =
        env::var("FERRYMAN_VLLM_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8001".into());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(190))
        .build()?;
    let controller = Controller::spawn(
        Duration::from_secs(idle_timeout),
        Duration::from_secs(start_timeout),
        vllm_bin,
        model_root,
        vllm_endpoint.clone(),
        client.clone(),
    );
    let state = AppState {
        controller: controller.clone(),
        client,
        token: Arc::from(token),
        vllm_endpoint: Arc::from(vllm_endpoint),
    };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/runtime", get(runtime_status))
        .route("/runtime/acquire", post(acquire_runtime))
        .route("/runtime/leases/{lease_id}", delete(release_runtime))
        .route("/runtime/stop", post(stop_runtime))
        .route("/v1/chat/completions", post(proxy_chat))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!(%listen, "Ferryman AI Pod agent listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Err(error) = controller.stop(true).await {
        warn!(%error, "could not stop vLLM during shutdown");
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_weight_loading_progress() {
        assert_eq!(
            parse_startup_signal("Loading safetensors checkpoint shards: 60%|###"),
            Some((StartupStage::LoadingWeights, 31))
        );
    }

    #[test]
    fn parses_cuda_graph_progress_with_spacing() {
        assert_eq!(
            parse_startup_signal("Capturing CUDA graphs (PIECEWISE): 50 %"),
            Some((StartupStage::CapturingGraphs, 80))
        );
    }

    #[test]
    fn recognizes_compilation_and_server_start() {
        assert_eq!(
            parse_startup_signal("Using cache directory for torchinductor"),
            Some((StartupStage::CompilingKernels, 52))
        );
        assert_eq!(
            parse_startup_signal("INFO: Application startup complete."),
            Some((StartupStage::StartingServer, 98))
        );
    }

    #[test]
    fn strips_ansi_and_control_characters() {
        assert_eq!(sanitize_log_line("\u{1b}[32mready\u{1b}[0m\u{7}"), "ready");
    }
}
