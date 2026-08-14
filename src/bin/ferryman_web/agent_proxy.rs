//! Proxy handlers for the AI Pod agent plus model lease acquisition helpers.

use super::{agent_response, json_error, user_owner, AppState, Config, MAX_MODEL_START_ATTEMPTS};
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferryman::preset::Preset;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Deserialize)]
pub(super) struct RuntimeStartRequest {
    preset: Preset,
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

pub(super) async fn runtime_status(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/runtime", None).await {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

pub(super) async fn model_catalog(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/models", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

pub(super) async fn model_storage(State(state): State<AppState>) -> Response {
    match agent_json(&state.config, reqwest::Method::GET, "/storage", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

pub(super) async fn start_model_download(
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

pub(super) async fn pause_model_download(
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

pub(super) async fn delete_model(
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

pub(super) async fn start_source_benchmark(
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

pub(super) async fn cancel_source_benchmark(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
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

pub(super) async fn clear_runtime_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if user_owner(&headers, state.config.allow_local_user).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "missing SAFE_UID header");
    }
    match agent_json(&state.config, reqwest::Method::DELETE, "/cache", None).await {
        Ok((status, value)) => agent_response(status, value),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

pub(super) async fn runtime_start(
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

pub(super) async fn runtime_stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

pub(crate) async fn acquire_agent(config: &Config, preset: Preset, lease_id: &str) -> Result<()> {
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

pub(crate) async fn acquire_agent_with_retry(
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

pub(crate) async fn release_agent(config: &Config, lease_id: &str) {
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

pub(crate) async fn wait_for_agent(
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use std::path::PathBuf;
    use std::sync::Arc;

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
