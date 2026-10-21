//! Wire contract shared by Web, the AI Pod Agent and generated frontend types.
use crate::preset::Preset;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LEASE_TTL_SECONDS: u64 = 120;
fn default_lease_ttl() -> u64 {
    LEASE_TTL_SECONDS
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(ts_rs::TS)]
pub enum RuntimePhase {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(ts_rs::TS)]
pub enum StartupStage {
    StartingProcess,
    LoadingWeights,
    CompilingKernels,
    CapturingGraphs,
    StartingServer,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, ts_rs::TS)]
pub struct RuntimeStatus {
    pub state: RuntimePhase,
    pub execution_mode: ExecutionMode,
    pub preset: Option<Preset>,
    pub pid: Option<u32>,
    pub active_requests: usize,
    pub leases: usize,
    pub idle_timeout_seconds: u64,
    pub updated_at: u64,
    pub last_error: Option<String>,
    pub startup_stage: Option<StartupStage>,
    pub startup_progress: u8,
    pub startup_elapsed_seconds: Option<u64>,
    pub estimated_remaining_seconds: Option<u64>,
    pub recent_logs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Serve,
    Batch,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AcquireRequest {
    pub preset: Preset,
    pub lease_id: String,
    #[serde(default = "default_lease_ttl")]
    pub ttl_seconds: u64,
    #[serde(default)]
    pub mode: ExecutionMode,
}

impl AcquireRequest {
    pub fn new(preset: Preset, lease_id: impl Into<String>, mode: ExecutionMode) -> Self {
        Self {
            preset,
            lease_id: lease_id.into(),
            mode,
            ttl_seconds: LEASE_TTL_SECONDS,
        }
    }
}

pub const MAX_BATCH_REQUESTS: usize = 2048;
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BATCH_LINE_BYTES: usize = 32 * 1024 * 1024;

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
    pub outputs: Vec<BatchOutput>,
    pub error: Option<String>,
}

/// Native Orinfer output. Keep the Chat body intact (usage, model and choices).
#[derive(Debug, Deserialize, Serialize)]
pub struct BatchResponse {
    pub status_code: u16,
    pub body: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BatchError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BatchReply {
    pub response: Option<BatchResponse>,
    pub error: Option<BatchError>,
}

impl BatchReply {
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            response: None,
            error: Some(BatchError {
                code: "request_failed".into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BatchOutput {
    pub custom_id: String,
    #[serde(flatten)]
    pub reply: BatchReply,
}

#[derive(Deserialize, Serialize)]
pub struct PairChallenge {
    pub nonce: String,
}

#[derive(Deserialize, Serialize)]
pub struct PairRequest {
    pub nonce: String,
    pub token: String,
    pub proof: String,
}
