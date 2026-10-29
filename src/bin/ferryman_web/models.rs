//! User-scoped model discovery and custom OpenAI-compatible APIs on the MicroServer.
use super::{json_error, AppState, UserIdentity};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use ferryman::model_api::{normalize_base_url, ModelTarget, PromptAdapter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

#[cfg(feature = "lazycat")]
#[path = "model_discovery.rs"]
mod discovery;
#[cfg(feature = "lazycat")]
pub(super) use discovery::discover;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct StoredModel {
    pub info: ModelInfo,
    // This field never crosses the browser API boundary.
    pub api_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ts_rs::TS)]
pub(crate) struct ModelInfo {
    pub target: ModelTarget,
    pub source: String,
    pub category: String,
    pub deployment_status: String,
    pub runtime_status: String,
    pub available: bool,
    pub has_api_key: bool,
    pub devices: Vec<String>,
    // Only a value actually reported by the service; never a client default.
    #[serde(default)]
    pub server_max_active: Option<u32>,
}

#[derive(Serialize, ts_rs::TS)]
pub(crate) struct ModelsResponse {
    pub models: Vec<ModelInfo>,
    pub discovery_enabled: bool,
    pub partial: bool,
    pub warnings: Vec<String>,
}

#[derive(Deserialize, ts_rs::TS)]
pub(crate) struct CustomModelRequest {
    pub id: Option<String>,
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    // Omitted/null preserves a saved key; an empty string clears it.
    pub api_key: Option<String>,
    pub adapter: PromptAdapter,
    pub context_length: u32,
    pub max_output_tokens: u32,
    pub concurrency: usize,
}

pub(super) async fn list(State(state): State<AppState>, identity: UserIdentity) -> Response {
    match state.store.models(identity.owner).await {
        Ok(models) => Json(ModelsResponse {
            models: sorted(models),
            discovery_enabled: cfg!(feature = "lazycat") && !state.config.discovery_url.is_empty(),
            partial: false,
            warnings: vec![],
        })
        .into_response(),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取模型配置"),
    }
}

fn sorted(models: Vec<StoredModel>) -> Vec<ModelInfo> {
    let mut result: Vec<_> = models
        .into_iter()
        .filter(model_supported)
        .map(|model| model.info)
        .collect();
    result.sort_by_key(|model| {
        (
            !model.available,
            model.category != "translation",
            model.target.name.clone(),
        )
    });
    result
}

fn model_supported(model: &StoredModel) -> bool {
    cfg!(feature = "lazycat") || model.info.source == "custom"
}

pub(super) async fn save(
    State(state): State<AppState>,
    identity: UserIdentity,
    Json(request): Json<CustomModelRequest>,
) -> Response {
    let id = request
        .id
        .unwrap_or_else(|| format!("custom:{}", Uuid::new_v4()));
    if !id.starts_with("custom:") || id.len() > 128 {
        return json_error(StatusCode::BAD_REQUEST, "无效的自定义模型 ID");
    }
    let mut target = ModelTarget {
        id,
        name: request.name,
        base_url: request.base_url,
        model_id: request.model_id,
        adapter: request.adapter,
        context_length: request.context_length,
        max_output_tokens: request.max_output_tokens,
        concurrency: request.concurrency,
    };
    if let Err(error) = target.validate() {
        return json_error(StatusCode::BAD_REQUEST, error.to_string());
    }
    let existing = match state.store.models(identity.owner.clone()).await {
        Ok(models) => models
            .into_iter()
            .find(|model| model.info.target.id == target.id),
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取模型配置"),
    };
    // A changed destination must never inherit another destination's secret.
    let api_key = request.api_key.unwrap_or_else(|| {
        existing
            .filter(|model| model.info.target.base_url == target.base_url)
            .map(|model| model.api_key)
            .unwrap_or_default()
    });
    if api_key.len() > 8192
        || reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}")).is_err()
    {
        return json_error(StatusCode::BAD_REQUEST, "无效的 API Key");
    }
    let model = StoredModel {
        info: ModelInfo {
            target,
            source: "custom".into(),
            category: "chat".into(),
            deployment_status: "custom".into(),
            runtime_status: "unknown".into(),
            available: true,
            has_api_key: !api_key.is_empty(),
            devices: vec![],
            server_max_active: None,
        },
        api_key,
    };
    match state
        .store
        .save_models(identity.owner, vec![model.clone()], false, false)
        .await
    {
        Ok(()) => Json(model.info).into_response(),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法保存模型配置"),
    }
}

pub(super) async fn delete(
    State(state): State<AppState>,
    identity: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    if !id.starts_with("custom:") {
        return json_error(StatusCode::BAD_REQUEST, "仅可移除自定义 API 配置");
    }
    match state.store.delete_model(identity.owner, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法删除模型配置"),
    }
}

pub(crate) async fn selected(state: &AppState, owner: &str, id: &str) -> Result<ModelTarget> {
    let model = state
        .store
        .models(owner.to_string())
        .await?
        .into_iter()
        .find(|model| model_supported(model) && model.info.target.id == id)
        .context("所选模型不存在，请刷新模型列表")?;
    if !model.info.available {
        bail!("所选模型当前不可用，请选择其他模型");
    }
    Ok(model.info.target)
}

pub(crate) async fn credential(
    state: &AppState,
    owner: &str,
    target: &ModelTarget,
) -> Result<String> {
    let model = state
        .store
        .models(owner.to_string())
        .await?
        .into_iter()
        .find(|model| model_supported(model) && model.info.target.id == target.id)
        .context("模型配置已移除，请重新选择模型并提交任务")?;
    if model.info.target.base_url != target.base_url {
        bail!("模型 API 地址已更改，请重新提交任务");
    }
    Ok(model.api_key)
}

#[derive(Deserialize, ts_rs::TS)]
pub(crate) struct ProbeRequest {
    pub base_url: String,
    pub api_key: Option<String>,
    pub id: Option<String>,
}
#[derive(Serialize, ts_rs::TS)]
pub(crate) struct ProbeResponse {
    pub models: Vec<String>,
    pub context_length: Option<u32>,
}

pub(super) async fn probe(
    State(state): State<AppState>,
    identity: UserIdentity,
    Json(request): Json<ProbeRequest>,
) -> Response {
    let result = async {
        let base_url = normalize_base_url(&request.base_url)?;
        let explicit_key = request.api_key;
        let mut key = explicit_key.clone().unwrap_or_default();
        if explicit_key.is_none() {
            if let Some(id) = request.id {
                if let Some(saved) =
                    state
                        .store
                        .models(identity.owner)
                        .await?
                        .into_iter()
                        .find(|model| {
                            model_supported(model)
                                && model.info.target.id == id
                                && model.info.target.base_url == base_url
                        })
                {
                    key = saved.api_key;
                }
            }
        }
        let response = authorized(state.config.client.get(format!("{base_url}/models")), &key)
            .send()
            .await
            .context("无法连接模型 API")?;
        if !response.status().is_success() {
            bail!("获取模型列表失败：HTTP {}", response.status());
        }
        let value = read_json(response).await?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .context("API 未返回有效的模型列表")?
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let target = ModelTarget {
            id: String::new(),
            name: String::new(),
            base_url,
            model_id: String::new(),
            adapter: PromptAdapter::Generic,
            context_length: 2048,
            max_output_tokens: 768,
            concurrency: 4,
        };
        let context_length = health_metadata(&state.config.client, &target, &key)
            .await
            .ok()
            .and_then(|value| value.get("max_context").and_then(Value::as_u64))
            .filter(|n| (512..=2_000_000).contains(n))
            .map(|n| n as u32);
        Ok::<_, anyhow::Error>(ProbeResponse {
            models,
            context_length,
        })
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

fn authorized(request: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    if key.is_empty() {
        request
    } else {
        request.bearer_auth(key)
    }
}

async fn health_metadata(
    client: &reqwest::Client,
    target: &ModelTarget,
    key: &str,
) -> Result<Value> {
    let url = reqwest::Url::parse(&target.base_url)?.join("/health")?;
    let response = authorized(client.get(url).timeout(Duration::from_secs(3)), key)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("health unavailable");
    }
    read_json(response).await
}

async fn read_json(mut response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            bail!("模型服务响应超过 2 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        bail!("模型服务返回空响应（HTTP {status}），请检查 API 地址与网关域名路由");
    }
    if html {
        bail!("模型服务返回网页而非 JSON（HTTP {status}），请检查登录授权或 API 地址");
    }
    serde_json::from_slice(&bytes).context("模型服务未返回有效的 JSON 数据")
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "lazycat")]
    use super::discovery::{deployment_model, discover_models, discovery_request, Deployment};
    use super::*;
    #[cfg(feature = "lazycat")]
    use axum::http::HeaderMap;
    #[cfg(feature = "lazycat")]
    #[test]
    fn unknown_runtime_and_null_accessibility_do_not_hide_translation_model() {
        let deployment: Deployment = serde_json::from_value(serde_json::json!({
            "key":"index:one", "name":"Index", "modelId":"Index-Translate-2B", "category":"translation",
            "deploymentStatus":"succeeded", "runtimeStatus":"unknown", "accessible":null, "apiAccessible":true,
            "devices":[{"name":"pod", "online":true}],
            "api":{"baseUrl":"http://192.168.2.91:18156/v1", "apiKey":"secret"}
        })).unwrap();
        let model = deployment_model(deployment).unwrap().unwrap();
        assert!(model.info.available);
        assert_eq!(model.info.target.adapter, PromptAdapter::IndexTranslate);
        assert!(!serde_json::to_string(&model.info)
            .unwrap()
            .contains("secret"));
    }
    fn identity(owner: &str) -> UserIdentity {
        UserIdentity {
            uid: owner.into(),
            owner: owner.into(),
        }
    }
    fn custom(base: &str, key: Option<&str>) -> CustomModelRequest {
        CustomModelRequest {
            id: Some("custom:test".into()),
            name: "Test chat".into(),
            base_url: base.into(),
            model_id: "test-chat".into(),
            api_key: key.map(str::to_string),
            adapter: PromptAdapter::Generic,
            context_length: 2048,
            max_output_tokens: 768,
            concurrency: 2,
        }
    }
    async fn response_json(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn keys_are_private_user_scoped_and_never_inherited_by_changed_destinations() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::regression_tests::state_at(root.path()).await;
        let response = save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://one.test/v1", Some("private-key"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let info = response_json(response).await;
        assert_eq!(info["has_api_key"], true);
        assert!(!info.to_string().contains("private-key"));
        let mut legacy = info.clone();
        legacy.as_object_mut().unwrap().remove("server_max_active");
        assert!(serde_json::from_value::<ModelInfo>(legacy)
            .unwrap()
            .server_max_active
            .is_none());
        let target = selected(&state, "alice", "custom:test").await.unwrap();
        assert_eq!(
            credential(&state, "alice", &target).await.unwrap(),
            "private-key"
        );
        assert!(selected(&state, "bob", "custom:test").await.is_err());
        assert!(state.store.models("bob".into()).await.unwrap().is_empty());
        save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://one.test/v1", None)),
        )
        .await;
        assert_eq!(
            credential(&state, "alice", &target).await.unwrap(),
            "private-key"
        );
        save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://two.test/v1", None)),
        )
        .await;
        assert!(credential(&state, "alice", &target).await.is_err());
        assert!(state.store.models("alice".into()).await.unwrap()[0]
            .api_key
            .is_empty());
    }
    #[cfg(not(feature = "lazycat"))]
    #[tokio::test]
    async fn standalone_ignores_discovery_configuration_and_only_uses_custom_models() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::regression_tests::state_at(root.path()).await;
        std::sync::Arc::get_mut(&mut state.config)
            .unwrap()
            .discovery_url = "https://discovery.test/api/v1/model-deployments".into();
        tokio::fs::create_dir_all(&state.config.data_dir)
            .await
            .unwrap();
        tokio::fs::write(
            state.config.data_dir.join("discovery-api-auth.json"),
            b"invalid-debug-config",
        )
        .await
        .unwrap();
        save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://one.test/v1", Some("key"))),
        )
        .await;
        let mut discovered = state.store.models("alice".into()).await.unwrap()[0].clone();
        discovered.info.source = "discovered".into();
        discovered.info.target.id = "discovered:old".into();
        let target = discovered.info.target.clone();
        state
            .store
            .save_models("alice".into(), vec![discovered], true, true)
            .await
            .unwrap();
        let public = response_json(list(State(state.clone()), identity("alice")).await).await;
        assert_eq!(public["discovery_enabled"], false);
        assert_eq!(public["models"].as_array().unwrap().len(), 1);
        assert_eq!(public["models"][0]["source"], "custom");
        assert!(selected(&state, "alice", &target.id).await.is_err());
        assert!(credential(&state, "alice", &target).await.is_err());
        let custom = selected(&state, "alice", "custom:test").await.unwrap();
        assert_eq!(credential(&state, "alice", &custom).await.unwrap(), "key");
    }
    #[tokio::test]
    async fn partial_discovery_preserves_other_records_and_complete_discovery_preserves_custom_apis(
    ) {
        let root = tempfile::tempdir().unwrap();
        let state = crate::regression_tests::state_at(root.path()).await;
        save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://one.test/v1", Some("key"))),
        )
        .await;
        let mut discovered = state.store.models("alice".into()).await.unwrap()[0].clone();
        discovered.info.source = "discovered".into();
        discovered.info.target.id = "discovered:one".into();
        state
            .store
            .save_models("alice".into(), vec![discovered], true, true)
            .await
            .unwrap();
        state
            .store
            .save_models("alice".into(), vec![], true, false)
            .await
            .unwrap();
        assert_eq!(state.store.models("alice".into()).await.unwrap().len(), 2);
        state
            .store
            .save_models("alice".into(), vec![], true, true)
            .await
            .unwrap();
        let models = state.store.models("alice".into()).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].info.source, "custom");
    }
    #[tokio::test]
    async fn job_model_snapshot_and_credentials_survive_a_database_restart() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::regression_tests::state_at(root.path()).await;
        save(
            State(state.clone()),
            identity("alice"),
            Json(custom("https://one.test/v1", Some("key"))),
        )
        .await;
        let mut job =
            crate::regression_tests::entry_at(root.path(), super::super::JobStatus::Queued);
        job.record.model = Some(selected(&state, "alice", "custom:test").await.unwrap());
        let id = job.record.id;
        state.store.insert(job, 100).await.unwrap();
        drop(state);
        let state = crate::regression_tests::state_at(root.path()).await;
        let job = state.store.get("alice".into(), id).await.unwrap().unwrap();
        let target = job.record.model.unwrap();
        assert_eq!(target.model_id, "test-chat");
        assert_eq!(credential(&state, "alice", &target).await.unwrap(), "key");
    }
    #[cfg(feature = "lazycat")]
    #[tokio::test]
    async fn canonical_discovery_reports_missing_authorization_before_network_access() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::regression_tests::state_at(root.path()).await;
        std::sync::Arc::get_mut(&mut state.config)
            .unwrap()
            .discovery_url = "http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments".into();
        let error = discover_models(&state, &identity("alice"), &HeaderMap::new())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("尚未获得微服用户授权"));
        assert!(state.store.models("alice".into()).await.unwrap().is_empty());
    }
    #[cfg(feature = "lazycat")]
    #[tokio::test]
    async fn debug_discovery_credentials_are_scoped_to_the_bound_uid_and_exact_url() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::regression_tests::state_at(root.path()).await;
        std::sync::Arc::get_mut(&mut state.config)
            .unwrap()
            .discovery_url = "http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments".into();
        tokio::fs::create_dir_all(&state.config.data_dir)
            .await
            .unwrap();
        let path = state.config.data_dir.join("discovery-api-auth.json");
        let target = "https://ai.box.heiyu.space/api/v1/model-deployments";
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "uid":"alice", "url":target, "api_auth_token":"private-debug-token"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let mut incoming = HeaderMap::new();
        incoming.insert("cookie", "private-cookie".parse().unwrap());
        incoming.insert("x-hc-user-ticket", "user-ticket".parse().unwrap());
        incoming.insert("lzc-api-auth-token", "caller-token".parse().unwrap());
        let alice = UserIdentity {
            uid: "alice".into(),
            owner: "hashed-owner".into(),
        };
        let request = discovery_request(&state, &alice, &incoming)
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), target);
        let credential = request.headers().get("lzc-api-auth-token").unwrap();
        assert_eq!(credential, "private-debug-token");
        assert!(credential.is_sensitive());
        assert!(!format!("{request:?}").contains("private-debug-token"));
        for name in ["cookie", "x-hc-user-ticket", "authorization"] {
            assert!(request.headers().get(name).is_none());
        }
        let bob = discovery_request(&state, &identity("bob"), &incoming)
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(bob.url().host_str(), Some("app.cloud.lazycat.app.ai.lzcx"));
        assert!(bob.headers().get("lzc-api-auth-token").is_none());
        assert_eq!(
            bob.headers().get("x-hc-user-ticket").unwrap(),
            "user-ticket"
        );
        assert!(
            discovery_request(&state, &identity("bob"), &HeaderMap::new())
                .await
                .is_err()
        );
        let public = response_json(list(State(state.clone()), alice).await).await;
        assert!(!public.to_string().contains("private-debug-token"));
        tokio::fs::remove_file(path).await.unwrap();
        assert!(
            discovery_request(&state, &identity("alice"), &HeaderMap::new())
                .await
                .is_err()
        );
    }
    #[cfg(feature = "lazycat")]
    #[tokio::test]
    async fn debug_discovery_rejects_invalid_destinations_and_credentials_without_echoing_secrets()
    {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::regression_tests::state_at(root.path()).await;
        std::sync::Arc::get_mut(&mut state.config)
            .unwrap()
            .discovery_url = "https://normal.test".into();
        tokio::fs::create_dir_all(&state.config.data_dir)
            .await
            .unwrap();
        for (url, token) in [
            (
                "http://ai.box.heiyu.space/api/v1/model-deployments",
                "private-debug-token",
            ),
            (
                "https://user:private-debug-token@ai.box.heiyu.space/api/v1/model-deployments",
                "key",
            ),
            (
                "https://ai.box.heiyu.space/api/v1/model-deployments",
                "private-debug-token\r\nInjected: header",
            ),
            ("https://ai.box.heiyu.space/api/v1/model-deployments", " "),
        ] {
            tokio::fs::write(
                state.config.data_dir.join("discovery-api-auth.json"),
                serde_json::to_vec(
                    &serde_json::json!({"uid":"alice", "url":url, "api_auth_token":token}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let error = discovery_request(&state, &identity("alice"), &HeaderMap::new())
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("模型发现调试"));
            assert!(!format!("{error:?}").contains("private-debug-token"));
        }
    }
    #[cfg(feature = "lazycat")]
    #[tokio::test]
    async fn delegated_discovery_uses_user_ticket_but_inference_metadata_does_not() {
        use axum::{routing::get, Router};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let partial = Arc::new(AtomicBool::new(false));
        let flag = partial.clone();
        let api_base = format!("http://{address}/v1");
        let app = Router::new().route("/api/v1/model-deployments", get(move |headers: HeaderMap| {
            let flag = flag.clone(); let base = api_base.clone();
            async move {
                assert_eq!(headers.get("x-hc-user-ticket").unwrap(), "alice-ticket");
                assert!(headers.get("authorization").is_none());
                assert!(headers.get("cookie").is_none());
                if flag.load(Ordering::SeqCst) {
                    Json(serde_json::json!({"version":1,"deployments":[],"partial":true,"warnings":["one pod unavailable"]}))
                } else {
                    Json(serde_json::json!({"version":1,"partial":false,"warnings":[],"deployments":[{
                        "key":"chat:one","name":"Any chat model","modelId":"chat-model","category":"chat",
                        "deploymentStatus":"succeeded","runtimeStatus":"unknown","apiAccessible":true,
                        "devices":[{"name":"pod","online":true}],"api":{"baseUrl":base,"apiKey":"inference-key"}
                    }]}))
                }
            }
        })).route("/health", get(|headers: HeaderMap| async move {
            assert!(headers.get("x-hc-user-ticket").is_none());
            assert!(headers.get("lzc-api-auth-token").is_none());
            assert!(headers.get("cookie").is_none());
            assert_eq!(headers.get("authorization").unwrap(),"Bearer inference-key");
            Json(serde_json::json!({"status":"ready","max_context":4096,"scheduler":{"max_active":128}}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::regression_tests::state_at(root.path()).await;
        let host = "app.cloud.lazycat.app.ai.lzcx";
        let config = Arc::get_mut(&mut state.config).unwrap();
        config.discovery_url = format!("http://{host}:{}/api/v1/model-deployments", address.port());
        config.client = reqwest::Client::builder()
            .no_proxy()
            .resolve(host, address)
            .build()
            .unwrap();
        state.delegate_tickets.lock().await.insert(
            "alice".into(),
            ("alice-ticket".parse().unwrap(), std::time::Instant::now()),
        );
        let response = discover_models(&state, &identity("alice"), &HeaderMap::new())
            .await
            .unwrap();
        assert!(!response.partial);
        assert_eq!(response.models.len(), 1);
        assert_eq!(response.models[0].target.context_length, 4096);
        assert_eq!(response.models[0].server_max_active, Some(128));
        assert_eq!(response.models[0].target.concurrency, 4);
        assert_eq!(response.models[0].target.adapter, PromptAdapter::Generic);
        assert!(!serde_json::to_string(&response)
            .unwrap()
            .contains("inference-key"));
        partial.store(true, Ordering::SeqCst);
        let response = discover_models(&state, &identity("alice"), &HeaderMap::new())
            .await
            .unwrap();
        assert!(response.partial);
        assert_eq!(response.models.len(), 1);
        assert_eq!(response.warnings, vec!["one pod unavailable"]);
        server.abort();
    }
    #[tokio::test]
    async fn empty_gateway_and_login_pages_have_actionable_errors_without_echoing_bodies() {
        use axum::{response::Html, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/empty", get(|| async { "" })).route(
            "/login",
            get(|| async { Html("<html>private-login-page</html>") }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{address}/empty"))
            .send()
            .await
            .unwrap();
        let message = read_json(response).await.unwrap_err().to_string();
        assert!(message.contains("空响应"));
        assert!(message.contains("200"));
        assert!(!message.contains("EOF"));
        let response = client
            .get(format!("http://{address}/login"))
            .send()
            .await
            .unwrap();
        let message = read_json(response).await.unwrap_err().to_string();
        assert!(message.contains("网页而非 JSON"));
        assert!(!message.contains("private-login-page"));
        task.abort();
    }
}
