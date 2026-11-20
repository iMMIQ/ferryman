//! Isolated Web process regressions.
#![cfg(unix)]
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ProcessGuard(Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl std::ops::Deref for ProcessGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for ProcessGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

#[tokio::test]
async fn web_translates_with_external_models_and_official_prompt_adapters() {
    use axum::{routing::post, Json, Router};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("web");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_origin = format!("http://{}", listener.local_addr().unwrap());
    let model_app = Router::new().route("/v1/chat/completions", post(|headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
        assert_eq!(headers.get("authorization").unwrap(), "Bearer regression-model-key");
        assert!(headers.get("x-hc-user-ticket").is_none());
        assert!(body.get("top_k").is_none());
        let generic = body["model"] == "my-arbitrary-chat-v2";
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), if generic { 2 } else { 1 });
        let prompt = messages.last().unwrap()["content"].as_str().unwrap();
        if !generic {
            assert!(prompt.starts_with("请将以下文本翻译成中文"));
            assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        }
        let source = prompt.split("<c1>").nth(1).unwrap().split("</c1>").next().unwrap();
        Json(serde_json::json!({"choices":[{"message":{"content":format!("<c1>译文:{source}</c1>")},"finish_reason":"stop"}]}))
    }));
    let model_server = tokio::spawn(async move {
        axum::serve(listener, model_app).await.unwrap();
    });
    fs::create_dir_all(root.join("documents")).unwrap();
    fs::create_dir_all(root.join("remote")).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let _web = ProcessGuard(
        Command::new(env!("CARGO_BIN_EXE_ferryman-web"))
            .env("FERRYMAN_WEB_LISTEN", address.to_string())
            .env("FERRYMAN_DATA_DIR", root.join("data"))
            .env("FERRYMAN_USER_DOCUMENTS_DIR", root.join("documents"))
            .env("FERRYMAN_REMOTE_FS_DIR", root.join("remote"))
            .env("FERRYMAN_ALLOW_LOCAL_USER", "true")
            .env("FERRYMAN_MODEL_DISCOVERY_URL", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let origin = format!("http://{address}");
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client
                .get(format!("{origin}/api/config"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for (model, adapter) in [
        ("Index-Translate-2B", "index_translate"),
        ("IndexTeam/Index-Translate-9B", "index_translate"),
        ("Index-Translate-35B-A3B", "index_translate"),
        ("my-arbitrary-chat-v2", "generic"),
    ] {
        let connection: serde_json::Value = client.post(format!("{origin}/api/models"))
            .json(&serde_json::json!({"name":model,"base_url":format!("{model_origin}/v1"),"model_id":model,
                "api_key":"regression-model-key","adapter":adapter,"context_length":2048,"max_output_tokens":768,"concurrency":2}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let selection = connection["target"]["id"].as_str().unwrap();
        let boundary = "ferryman-quality-regression";
        let mut body = String::new();
        for (name, value) in [
            ("model_id", selection),
            ("target", "中文"),
            ("mode", "replace"),
            ("batch_size", "1"),
        ] {
            body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
        }
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"regression.txt\"\r\nContent-Type: text/plain\r\n\r\nFirst paragraph.\nSecond paragraph.\n\r\n--{boundary}--\r\n"));
        let response = client
            .post(format!("{origin}/api/jobs"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let created: serde_json::Value = response.json().await.unwrap();
        let id = created["id"].as_str().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let job: serde_json::Value = client
                    .get(format!("{origin}/api/jobs/{id}"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                match job["status"].as_str() {
                    Some("completed") => break job,
                    Some("failed" | "cancelled") => panic!("{model}: {job}"),
                    _ => tokio::time::sleep(Duration::from_millis(30)).await,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result["translated"], 2, "{model}: {result}");
        assert_eq!(result["failed_segments"], 0, "{model}: {result}");
        let text = client
            .get(format!("{origin}/api/jobs/{id}/result"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            text.find("译文:First paragraph.").unwrap()
                < text.find("译文:Second paragraph.").unwrap()
        );
    }
    model_server.abort();
}
