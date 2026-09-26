//! Isolated process regressions: no Docker daemon, GPU, or production storage.
#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
#[test]
fn cli_failures_return_nonzero() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .args(["--no-cache", "--input"])
        .arg(root.path().join("missing.txt"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 failed"));
}
#[test]
fn unhealthy_cli_container_is_removed() {
    let root = tempfile::tempdir().unwrap();
    executable(&root.path().join("docker"), "#!/bin/sh\ncase \"$1\" in\nrun) touch \"$FIXTURE_ROOT/running\";;\nrm) rm -f \"$FIXTURE_ROOT/running\";;\ninspect) echo true;;\nesac\n");
    fs::write(root.path().join("source.txt"), "Hello").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("FIXTURE_ROOT", root.path())
        .args(["--serve", "--health-timeout", "0", "--input"])
        .arg(root.path().join("source.txt"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!root.path().join("running").exists());
}
struct Agent {
    child: Child,
    root: tempfile::TempDir,
    origin: String,
    client: reqwest::Client,
}
impl Drop for Agent {
    fn drop(&mut self) {
        for file in ["model.pid", "help.pid"] {
            if let Ok(pid) = fs::read_to_string(self.root.path().join(file)) {
                if let Ok(pid) = pid.trim().parse::<i32>() {
                    if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                        let _ =
                            rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Agent {
    async fn start(mode: &str, bytes: u64, free: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let models = r.join("models/Hy-MT2-7B-FP8");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("config.json"), r#"{"model_type":"fixture"}"#).unwrap();
        fs::write(models.join("tokenizer.json"), r#"{"model":{}}"#).unwrap();
        fs::write(
            models.join("model.safetensors.index.json"),
            r#"{"weight_map":{"weight":"weights.safetensors"}}"#,
        )
        .unwrap();
        let header = format!(
            r#"{{"weight":{{"dtype":"U8","shape":[{bytes}],"data_offsets":[0,{bytes}]}}}}"#
        );
        use std::io::Write;
        let mut file = fs::File::create(models.join("weights.safetensors")).unwrap();
        file.write_all(&(header.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(header.as_bytes()).unwrap();
        file.set_len(bytes + 8 + header.len() as u64).unwrap();
        fs::write(r.join("free"), free.to_string()).unwrap();
        executable(
            &r.join("python3"),
            "#!/bin/sh\nprintf '17179869184 '; cat \"$FIXTURE_ROOT/free\"\n",
        );
        executable(
            &r.join("vllm"),
            r#"#!/bin/sh
case "$2" in
-h|--help=CacheConfig) echo $$ > "$FIXTURE_ROOT/help.pid"; exec sleep 120;;
esac
echo $$ > "$FIXTURE_ROOT/model.pid"
if [ "$FIXTURE_MODE" = orphan ]; then
  sleep 120 & echo $! > "$FIXTURE_ROOT/worker.pid"
  exit 7
fi
exec sleep 120
"#,
        );
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferryman-agent"));
        command
            .env(
                "PATH",
                format!("{}:{}", r.display(), std::env::var("PATH").unwrap()),
            )
            .env("FIXTURE_ROOT", r)
            .env("FIXTURE_MODE", mode)
            .env("FERRYMAN_AGENT_LISTEN", address.to_string())
            .env("FERRYMAN_AGENT_TOKEN", "isolated-regression-token")
            .env("FERRYMAN_MODEL_ROOT", r.join("models"))
            .env("FERRYMAN_CACHE_ROOT", r.join("cache"))
            .env("FERRYMAN_VLLM_BIN", r.join("vllm"))
            .env("FERRYMAN_VLLM_LD_PRELOAD", "")
            .env("FERRYMAN_START_TIMEOUT_SECONDS", "30")
            .env("FERRYMAN_VLLM_ENDPOINT", "http://127.0.0.1:1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if mode == "help" {
            command.env_remove("FERRYMAN_VLLM_KV_CACHE_FLAG");
        } else {
            command.env("FERRYMAN_VLLM_KV_CACHE_FLAG", "0");
        }
        let child = command.spawn().unwrap();
        let agent = Self {
            child,
            root,
            origin: format!("http://{address}"),
            client: reqwest::Client::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if agent.request("/healthz").send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        agent
    }
    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.origin))
            .bearer_auth("isolated-regression-token")
    }
    fn post(&self, path: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.origin))
            .bearer_auth("isolated-regression-token")
            .json(&body)
    }
    fn acquire(&self) -> reqwest::RequestBuilder {
        self.post(
            "/runtime/acquire",
            serde_json::json!({"preset":"7b-fp8","lease_id":"test"}),
        )
    }
    async fn stop(&self) {
        assert!(self
            .post("/runtime/stop", serde_json::json!({"force":true}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
}
#[tokio::test]
async fn gpu_memory_is_refreshed_after_failed_start() {
    let agent = Agent::start("memory", 8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024).await;
    assert_eq!(agent.acquire().send().await.unwrap().status(), 409);
    fs::write(
        agent.root.path().join("free"),
        (16u64 * 1024 * 1024 * 1024).to_string(),
    )
    .unwrap();
    let response = agent.acquire().send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 202, "{body}");
    agent.stop().await;
}
fn running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| !stat.contains(") Z"))
}
#[tokio::test]
async fn exited_vllm_parent_does_not_leave_workers() {
    let agent = Agent::start("orphan", 4, 16 * 1024 * 1024 * 1024).await;
    assert_eq!(agent.acquire().send().await.unwrap().status(), 202);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = agent
                .request("/runtime")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["state"] == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    agent.stop().await;
    let pid = fs::read_to_string(agent.root.path().join("worker.pid")).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while running(&pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn hung_capability_probe_has_a_deadline_and_releases_control() {
    let agent = Agent::start("help", 4, 16 * 1024 * 1024 * 1024).await;
    let acquire = tokio::spawn(agent.acquire().send());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !agent.root.path().join("help.pid").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(18), agent.stop())
        .await
        .unwrap();
    acquire.await.unwrap().unwrap();
    let pid = fs::read_to_string(agent.root.path().join("help.pid")).unwrap();
    assert!(!running(&pid));
    let status: serde_json::Value = agent
        .request("/runtime")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "stopped");
}
