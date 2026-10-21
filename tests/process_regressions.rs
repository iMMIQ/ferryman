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
        .args([
            "--serve",
            "--host-port",
            "0",
            "--health-timeout",
            "0",
            "--input",
        ])
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
        for file in ["model.pid"] {
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
    async fn batch_lease(&self) {
        assert_eq!(
            self.post(
                "/runtime/acquire",
                serde_json::json!({
                    "preset":"index-translate-2b", "lease_id":"test", "mode":"batch"
                })
            )
            .send()
            .await
            .unwrap()
            .status(),
            202
        );
    }

    fn jsonl_engine(&self) -> ferryman::engine::Engine {
        let client = ferryman::engine::build_translation_client(
            Duration::from_secs(10),
            Some("isolated-regression-token"),
        )
        .unwrap();
        ferryman::engine::Engine::new(
            client.clone(),
            self.origin.clone(),
            "index-translate-2b".into(),
            "中文".into(),
            32,
            None,
        )
        .with_checkpoint(ferryman::cache::Cache::open(Some(
            self.root.path().join("checkpoint"),
        )))
        .with_jsonl(ferryman::jsonl::JsonlBackend {
            client,
            endpoint: self.origin.clone(),
            preset: ferryman::preset::Preset::Index2B,
            lease_id: "test".into(),
        })
    }

    async fn start(mode: &str) -> Self {
        Self::start_with_endpoint(mode, "http://127.0.0.1:1").await
    }
    async fn start_with_endpoint(mode: &str, endpoint: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let models = r.join("models/Index-Translate-2B-Orinfer-E8P-Q4A8");
        fs::create_dir_all(models.join("cache/weights")).unwrap();
        fs::write(models.join("cache/model.json"), serde_json::to_vec(&serde_json::json!({"execution_package": ferryman::preset::Preset::Index2B.config().execution_package})).unwrap()).unwrap();
        fs::write(models.join("config.json"), r#"{"model_type":"fixture"}"#).unwrap();
        fs::write(models.join("tokenizer.json"), r#"{"model":{}}"#).unwrap();
        fs::write(
            models.join("cache/weights/model.safetensors.index.json"),
            r#"{"weight_map":{"weight":"weights.safetensors"}}"#,
        )
        .unwrap();
        let bytes = 4;
        let header = format!(
            r#"{{"weight":{{"dtype":"U8","shape":[{bytes}],"data_offsets":[0,{bytes}]}}}}"#
        );
        use std::io::Write;
        let mut file = fs::File::create(models.join("cache/weights/weights.safetensors")).unwrap();
        file.write_all(&(header.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(header.as_bytes()).unwrap();
        file.set_len(bytes + 8 + header.len() as u64).unwrap();
        executable(
            &r.join("orinfer"),
            r#"#!/bin/sh
printf '%s\n' "$@" > "$FIXTURE_ROOT/args"
echo $$ > "$FIXTURE_ROOT/model.pid"
if [ "$FIXTURE_MODE" = jsonl ]; then
  exec python3 "$FIXTURE_ORINFER" "$@"
fi
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
            .env(
                "FIXTURE_ORINFER",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_orinfer.py"),
            )
            .env("FERRYMAN_AGENT_LISTEN", address.to_string())
            .env("FERRYMAN_AGENT_TOKEN", "isolated-regression-token")
            .env("FERRYMAN_MODEL_ROOT", r.join("models"))
            .env("FERRYMAN_CACHE_ROOT", r.join("cache"))
            .env("FERRYMAN_ORINFER_BIN", r.join("orinfer"))
            .env("FERRYMAN_START_TIMEOUT_SECONDS", "30")
            .env("FERRYMAN_ORINFER_ENDPOINT", endpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let agent = Self {
            child,
            root,
            origin: format!("http://{address}"),
            client: reqwest::Client::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(response) = agent.request("/models").send().await {
                    if let Ok(catalog) = response.json::<serde_json::Value>().await {
                        if catalog["models"][0]["state"] == "ready" {
                            break;
                        }
                    }
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
            serde_json::json!({"preset":"index-translate-2b","lease_id":"test"}),
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
async fn jsonl_preserves_alignment_checkpoints_and_recovers_truncated_batches() {
    use ferryman::engine::Unit;
    use tokio_util::sync::CancellationToken;
    let agent = Agent::start("jsonl").await;
    agent.batch_lease().await;
    assert!(
        !agent.root.path().join("model.pid").exists(),
        "batch lease must not load an HTTP model"
    );
    let engine = agent.jsonl_engine();
    let units = vec![
        Unit::Single {
            file: 0,
            id: 0,
            text: "First paragraph.".into(),
        },
        Unit::Single {
            file: 0,
            id: 1,
            text: "Second paragraph.".into(),
        },
        Unit::Batch {
            file: 0,
            ids: vec![2, 3],
            cues: vec!["<truncate>Third.".into(), "Fourth.".into()],
            context: vec!["Read-only context".into()],
        },
    ];
    let mut results = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.exec_jsonl_units(units.clone(), CancellationToken::new(), |done| {
            results.extend(done.pairs)
        }),
    )
    .await
    .unwrap();
    results.sort();
    assert_eq!(
        results,
        vec![
            (0, "译文:First paragraph.".into()),
            (1, "译文:Second paragraph.".into()),
            (2, "译文:<truncate>Third.".into()),
            (3, "译文:Fourth.".into())
        ]
    );
    let runs = fs::read_to_string(agent.root.path().join("runs.jsonl")).unwrap();
    let runs: Vec<serde_json::Value> = runs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        runs[0]["requests"].as_array().unwrap().len(),
        3,
        "document requests should share one input file"
    );
    assert!(
        runs.len() >= 2,
        "truncated rows must generate a new recovery round"
    );
    let invocations = runs.len();
    engine
        .exec_jsonl_units(units, CancellationToken::new(), |_| {})
        .await;
    assert_eq!(
        fs::read_to_string(agent.root.path().join("runs.jsonl"))
            .unwrap()
            .lines()
            .count(),
        invocations,
        "checkpoint hits must bypass inference"
    );
    let status: serde_json::Value = agent
        .request("/runtime")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status["pid"].is_null());
    assert_eq!(status["active_requests"], 0);
    let batches = agent.root.path().join("cache/batches");
    assert_eq!(
        fs::read_dir(batches).unwrap().count(),
        0,
        "spooled document contents must be removed"
    );
}

#[tokio::test]
async fn cancelling_jsonl_reaps_the_model_and_preserves_completed_units() {
    use ferryman::engine::Unit;
    use tokio_util::sync::CancellationToken;
    let agent = Agent::start("jsonl").await;
    agent.batch_lease().await;
    let engine = agent.jsonl_engine();
    let cancel = CancellationToken::new();
    let mut results = Vec::new();
    // The fixture completes in reverse order, then blocks on the first row.
    tokio::time::timeout(
        Duration::from_secs(10),
        engine.exec_jsonl_units(
            vec![
                Unit::Single {
                    file: 0,
                    id: 0,
                    text: "<delay>pending".into(),
                },
                Unit::Single {
                    file: 0,
                    id: 1,
                    text: "Completed.".into(),
                },
            ],
            cancel.clone(),
            |done| {
                results.extend(done.pairs);
                cancel.cancel();
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(results, vec![(1, "译文:Completed.".into())]);
    let status: serde_json::Value = agent
        .request("/runtime")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status["pid"].is_null());
    assert_eq!(status["active_requests"], 0);
    assert_eq!(
        fs::read_dir(agent.root.path().join("cache/batches"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn jsonl_document_pipeline_writes_each_file_in_source_order() {
    use ferryman::batch::{run_batch_controlled, BatchOpts};
    use ferryman::format::OutputMode;
    use std::sync::Arc;
    let agent = Agent::start("jsonl").await;
    agent.batch_lease().await;
    let first = agent.root.path().join("first.txt");
    let second = agent.root.path().join("second.txt");
    fs::write(&first, "Alpha.\nBeta.\n").unwrap();
    fs::write(&second, "Gamma.\nDelta.\n").unwrap();
    let engine = agent.jsonl_engine();
    let summary = tokio::time::timeout(
        Duration::from_secs(10),
        run_batch_controlled(
            &engine,
            vec![first, second],
            BatchOpts {
                mode: OutputMode::Replace,
                in_place: false,
                output: None,
                batch_size: 1,
                context: 1,
                limit: None,
                prompt_char_budget: 0,
            },
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(summary.ok_files, 2);
    assert_eq!(summary.translated, 4);
    assert_eq!(summary.failed, 0);
    assert!(summary.failed_files.is_empty());
    for (file, left, right) in [
        ("first.translated.txt", "Alpha.", "Beta."),
        ("second.translated.txt", "Gamma.", "Delta."),
    ] {
        let output = fs::read_to_string(agent.root.path().join(file)).unwrap();
        assert!(
            output.find(&format!("译文:{left}")).unwrap()
                < output.find(&format!("译文:{right}")).unwrap()
        );
    }
}

#[tokio::test]
async fn jsonl_submit_is_idempotent_and_requires_a_batch_lease() {
    let agent = Agent::start("jsonl").await;
    let path = format!("/batch/jobs/{}", uuid::Uuid::new_v4());
    let body = serde_json::json!({"preset":"index-translate-2b", "lease_id":"test", "requests":[{
        "custom_id":"one", "body":{"model":"index-translate-2b", "enable_thinking":false, "messages":[{"role":"user","content":"<delay>"}]}
    }]});
    assert_eq!(
        agent
            .post(&path, body.clone())
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    agent.batch_lease().await;
    assert_eq!(
        agent
            .post(&path, body.clone())
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(
        agent
            .post(&path, body.clone())
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    let mut changed = body.clone();
    changed["requests"][0]["body"]["messages"][0]["content"] = "different".into();
    assert_eq!(
        agent.post(&path, changed).send().await.unwrap().status(),
        400
    );
    // A manual online load must not replace an active batch reservation.
    assert_eq!(agent.acquire().send().await.unwrap().status(), 409);
    let response = agent
        .client
        .delete(format!("{}{path}", agent.origin))
        .bearer_auth("isolated-regression-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        agent.post(&path, body).send().await.unwrap().status(),
        400,
        "a delayed submit must not resurrect a cancelled job"
    );
}

#[tokio::test]
async fn jsonl_agent_restart_resumes_committed_rows_without_repeating_them() {
    let mut agent = Agent::start("jsonl").await;
    agent.batch_lease().await;
    let id = uuid::Uuid::new_v4();
    let path = format!("/batch/jobs/{id}");
    let body = serde_json::json!({"preset":"index-translate-2b", "lease_id":"test", "requests":[
        {"custom_id":"pending", "body":{"model":"index-translate-2b", "enable_thinking":false,"messages":[{"role":"user","content":"<resume-delay>"}]}},
        {"custom_id":"done", "body":{"model":"index-translate-2b", "enable_thinking":false,"messages":[{"role":"user","content":"Complete."}]}}
    ]});
    assert_eq!(
        agent
            .post(&path, body.clone())
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page: serde_json::Value = agent
                .request(&path)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if page["outputs"]
                .as_array()
                .is_some_and(|outputs| !outputs.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Recreate the Agent with its persistent cache, as an AI Pod restart does.
    let pid = fs::read_to_string(agent.root.path().join("model.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    agent.child.kill().unwrap();
    agent.child.wait().unwrap();
    rustix::process::kill_process_group(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    let root = agent.root.path();
    agent.child = Command::new(env!("CARGO_BIN_EXE_ferryman-agent"))
        .env(
            "FERRYMAN_AGENT_LISTEN",
            agent.origin.trim_start_matches("http://"),
        )
        .env("FERRYMAN_AGENT_TOKEN", "isolated-regression-token")
        .env("FERRYMAN_MODEL_ROOT", root.join("models"))
        .env("FERRYMAN_CACHE_ROOT", root.join("cache"))
        .env("FERRYMAN_ORINFER_BIN", root.join("orinfer"))
        .env("FIXTURE_ROOT", root)
        .env("FIXTURE_MODE", "jsonl")
        .env(
            "FIXTURE_ORINFER",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_orinfer.py"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = agent.post("/runtime/acquire", serde_json::json!({"preset":"index-translate-2b", "lease_id":"test", "mode":"batch"})).send().await {
                if response.status() == 202 { break; }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(agent.post(&path, body).send().await.unwrap().status(), 202);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page: serde_json::Value = agent
                .request(&path)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if page["state"] == "completed" {
                assert_eq!(
                    page["outputs"].as_array().unwrap().len(),
                    2,
                    "resume must append only the unfinished row"
                );
                break;
            }
            assert_ne!(page["state"], "failed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let runs = fs::read_to_string(root.join("runs.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(runs.lines().last().unwrap()).unwrap();
    assert!(last["args"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg == "--resume"));
}
#[tokio::test]
async fn launch_uses_orinfer_model_alias_and_endpoint() {
    let agent = Agent::start("launch").await;
    assert_eq!(agent.acquire().send().await.unwrap().status(), 202);
    let args = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(args) = fs::read_to_string(agent.root.path().join("args")) {
                break args;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(args.contains("--model\nindex-translate-2b"), "{args}");
    assert!(args.contains("--listen\n127.0.0.1:1"), "{args}");
    assert!(!args.contains("--gpu-memory-utilization"), "{args}");
    agent.stop().await;
}

#[tokio::test]
async fn inference_rejects_public_listeners() {
    let agent = Agent::start_with_endpoint("private", "http://0.0.0.0:18088").await;
    let response = agent.acquire().send().await.unwrap();
    assert_eq!(response.status(), 409);
    assert!(response.text().await.unwrap().contains("private loopback"));
    assert!(!agent.root.path().join("model.pid").exists());
}

fn running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| !stat.contains(") Z"))
}
#[tokio::test]
async fn shutdown_stops_orinfer_before_draining_streams() {
    use axum::{
        body::Body,
        routing::{get, post},
        Router,
    };
    use std::sync::{Arc, Mutex};

    // Keep the mock generation open for as long as the orinfer process is alive.
    // Stopping orinfer closes the upstream stream, as it does in production.
    let model_pid = Arc::new(Mutex::new(String::new()));
    let stream_pid = model_pid.clone();
    let upstream = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/v1/chat/completions",
            post(move || {
                let model_pid = stream_pid.clone();
                async move {
                    Body::from_stream(futures::stream::unfold(model_pid, |model_pid| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let alive = running(&model_pid.lock().unwrap());
                        alive.then_some((Ok::<_, std::io::Error>("data: token\n\n"), model_pid))
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let mut agent = Agent::start_with_endpoint("shutdown", &endpoint).await;
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
            if status["state"] == "ready" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(agent.root.path().join("model.pid")).unwrap();
    *model_pid.lock().unwrap() = pid.clone();
    let mut response = agent
        .post("/v1/chat/completions", serde_json::json!({"stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.chunk().await.unwrap().is_some());
    let agent_pid = rustix::process::Pid::from_raw(agent.child.id() as i32).unwrap();
    rustix::process::kill_process(agent_pid, rustix::process::Signal::TERM).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = agent.child.try_wait().unwrap() {
                break status;
            }
            // Continue reading so downstream backpressure cannot stall shutdown.
            tokio::select! {
                _ = response.chunk() => {},
                _ = tokio::time::sleep(Duration::from_millis(20)) => {},
            }
        }
    })
    .await
    .expect("SIGTERM must stop orinfer and close ongoing generation streams");
    assert!(status.success(), "agent must exit cleanly: {status}");
    assert!(!running(&pid));
    upstream_task.abort();
}
#[tokio::test]
async fn exited_orinfer_parent_does_not_leave_workers() {
    let agent = Agent::start("orphan").await;
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
