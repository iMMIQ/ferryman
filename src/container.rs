//! Launch and tear down the vLLM Docker container that serves the model.
//!
//! [`ContainerGuard::launch`] starts the container and blocks until its
//! `/health` endpoint is ready. When the guard is dropped (at the end of
//! `main`, including on error via `?`) the container is removed, so the model
//! deployment is always shut down after translation.

use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub struct ServeSpec {
    pub image: String,
    /// Host directory containing the model files.
    pub host_model_dir: String,
    /// Host directory persisted as the container's /root/.cache so the vLLM v1
    /// / FlashInfer / Triton JIT kernels compiled on first launch are reused on
    /// subsequent launches (avoids ~3-5 min of recompilation each cold start).
    pub host_cache_dir: String,
    /// Path inside the container (also used as the served model id).
    pub container_model: String,
    pub host_port: u16,
    pub container_name: String,
    /// e.g. Some("awq_marlin") for AWQ; None lets vLLM auto-detect (FP8).
    pub quantization: Option<String>,
    pub dtype: String,
    /// KV cache dtype passed as `--kv-cache-dtype` (e.g. "fp8", "auto").
    pub kv_cache_dtype: String,
    pub gpu_memory_utilization: f32,
    pub max_model_len: u32,
    /// vLLM admission cap (`--max-num-seqs`); None = vLLM default (256).
    /// 512 unlocks the 30B's throughput ceiling (short blocks).
    pub max_num_seqs: Option<u32>,
    /// If true, pass `--enforce-eager` (no torch.compile/CUDA graphs). The 7B
    /// preset wants eager; the 30B preset wants graphs (faster, ~1 GiB capture).
    pub enforce_eager: bool,
    /// Seconds to wait for the server to become healthy.
    pub health_timeout: u64,
}

pub struct ContainerGuard {
    name: String,
    endpoint: String,
    active: bool,
}

impl ContainerGuard {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Start the container and wait until it is healthy. Returns a guard whose
    /// `Drop` removes the container.
    pub async fn launch(spec: &ServeSpec) -> Result<Self> {
        // Remove any stale container of the same name.
        let _ = Command::new("docker")
            .args(["rm", "-f", &spec.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        let mount = format!("{}:{}:ro", spec.host_model_dir, spec.container_model);
        // Persist the JIT/compile caches (FlashInfer, Triton, vLLM, inductor) so
        // the ~60s+ FlashInfer fp8/sm87 kernel compile is done once, not per run.
        let cache_mount = format!("{}:/root/.cache", spec.host_cache_dir);

        let mut args: Vec<String> = vec![
            "run".into(), "-d".into(), "--runtime=nvidia".into(),
            "--name".into(), spec.container_name.clone(),
            "-v".into(), mount,
            "-v".into(), cache_mount,
            "-p".into(), format!("{}:8000", spec.host_port),
            "--shm-size".into(), "512m".into(),
            "-e".into(), "VLLM_CONFIGURE_LOGGING=0".into(),
            "-e".into(), "VLLM_DO_NOT_TRACK=1".into(),
            "-e".into(), "VLLM_NO_USAGE_STATS=1".into(),
            // Route Triton + Inductor caches under the persisted /root/.cache too.
            "-e".into(), "TRITON_CACHE_DIR=/root/.cache/triton".into(),
            "-e".into(), "TORCHINDUCTOR_CACHE_DIR=/root/.cache/torchinductor".into(),
            "--entrypoint".into(), "vllm".into(),
            spec.image.clone(),
            "serve".into(), spec.container_model.clone(),
            "--host".into(), "0.0.0.0".into(),
            "--port".into(), "8000".into(),
            "--dtype".into(), spec.dtype.clone(),
            "--max-model-len".into(), spec.max_model_len.to_string(),
            "--gpu-memory-utilization".into(), spec.gpu_memory_utilization.to_string(),
            "--kv-cache-dtype".into(), spec.kv_cache_dtype.clone(),
            "--enable-prefix-caching".into(),
        ];
        // CUDA graphs are FASTER for the 30B-FP8 on this Jetson (measured 2.9x
        // single-stream, +9% @c256, peak 1222 tok/s) and cost only ~1 GiB to
        // capture. The 7B/quantized path stays eager. The preset decides; omitting
        // --enforce-eager enables torch.compile + cudagraph.
        if spec.enforce_eager {
            args.push("--enforce-eager".into());
        }
        if let Some(mns) = spec.max_num_seqs {
            args.push("--max-num-seqs".into());
            args.push(mns.to_string());
        }
        if let Some(q) = &spec.quantization {
            args.push("--quantization".into());
            args.push(q.clone());
        }

        eprintln!(
            "starting vLLM container '{}' (image {}, model {})…",
            spec.container_name, spec.image, spec.container_model
        );
        let out = Command::new("docker")
            .args(&args)
            .output()
            .await
            .context("failed to invoke docker")?;
        if !out.status.success() {
            bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let endpoint = format!("http://127.0.0.1:{}", spec.host_port);
        if !wait_health(&endpoint, &spec.container_name, spec.health_timeout).await {
            // Surface the container logs so the failure is diagnosable.
            let _ = Command::new("docker")
                .args(["logs", &spec.container_name])
                .status()
                .await;
            bail!(
                "vLLM container '{}' did not become healthy within {}s",
                spec.container_name, spec.health_timeout
            );
        }
        eprintln!("vLLM healthy at {}", endpoint);

        Ok(Self {
            name: spec.container_name.clone(),
            endpoint,
            active: true,
        })
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if self.active {
            eprintln!("stopping container '{}'…", self.name);
            // Drop can't be async, so this stays a blocking std call — the
            // container is detached (-d) and `rm -f` returns within a second.
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn wait_health(endpoint: &str, name: &str, timeout: u64) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        if !is_running(name).await {
            eprintln!("container '{}' exited before becoming healthy", name);
            return false;
        }
        if let Ok(resp) = client.get(format!("{}/health", endpoint)).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

async fn is_running(name: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}
