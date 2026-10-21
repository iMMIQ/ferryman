//! Launch and tear down the orinfer Docker container that serves the model.
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
    pub host_model_dir: String,
    pub host_cache_dir: String,
    pub container_model: String,
    pub model: String,
    pub host_port: u16,
    pub container_name: String,
    pub concurrency: usize,
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
        // Execution packages are installed in the image; only runtime caches are writable.
        let cache_mount = format!("{}:/root/.cache", spec.host_cache_dir);

        let args = vec![
            "run".to_string(),
            "-d".into(),
            "--runtime=nvidia".into(),
            "--name".into(),
            spec.container_name.clone(),
            "-v".into(),
            mount,
            "-v".into(),
            cache_mount,
            "-e".into(),
            "ORINFER_EXECUTION_CACHE=/opt/orinfer/packages".into(),
            "-p".into(),
            format!("{}:8088", spec.host_port),
            "--entrypoint".into(),
            "/opt/orinfer/bin/orinfer".into(),
            spec.image.clone(),
            "serve".into(),
            spec.container_model.clone(),
            "--model".into(),
            spec.model.clone(),
            "--listen".into(),
            "0.0.0.0:8088".into(),
            "--max-active-requests".into(),
            spec.concurrency.to_string(),
            "--prefix-cache-mib".into(),
            "1024".into(),
        ];

        eprintln!(
            "starting orinfer container '{}' (image {}, model {})…",
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
        let guard = Self {
            name: spec.container_name.clone(),
            endpoint: endpoint.clone(),
            active: true,
        };
        if !wait_health(&endpoint, &spec.container_name, spec.health_timeout).await {
            // Surface the container logs so the failure is diagnosable.
            let _ = Command::new("docker")
                .args(["logs", &spec.container_name])
                .status()
                .await;
            bail!(
                "orinfer container '{}' did not become healthy within {}s",
                spec.container_name,
                spec.health_timeout
            );
        }
        eprintln!("orinfer healthy at {}", endpoint);

        Ok(guard)
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
