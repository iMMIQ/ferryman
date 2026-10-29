//! Lazycat-only discovery and delegated authentication.
use super::*;
use axum::http::HeaderMap;
use std::io::ErrorKind;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Discovery {
    version: u32,
    deployments: Vec<Deployment>,
    #[serde(default)]
    partial: bool,
    #[serde(default)]
    warnings: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Deployment {
    key: String,
    name: String,
    #[serde(default)]
    model_id: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    deployment_status: String,
    #[serde(default)]
    runtime_status: String,
    api_accessible: Option<bool>,
    #[serde(default)]
    devices: Vec<Device>,
    api: DeploymentApi,
}
#[derive(Deserialize)]
struct Device {
    name: String,
    online: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentApi {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
}

pub(super) fn deployment_model(deployment: Deployment) -> Result<Option<StoredModel>> {
    if deployment.model_id.is_empty() || deployment.api.base_url.is_empty() {
        return Ok(None);
    }
    let adapter = if deployment
        .model_id
        .to_ascii_lowercase()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .starts_with("index-translate-")
    {
        PromptAdapter::IndexTranslate
    } else {
        PromptAdapter::Generic
    };
    let mut target = ModelTarget {
        id: format!("discovered:{}", deployment.key),
        name: deployment.name,
        base_url: deployment.api.base_url,
        model_id: deployment.model_id,
        adapter,
        context_length: 2048,
        max_output_tokens: 768,
        concurrency: 4,
    };
    target.validate()?;
    let available = deployment.deployment_status == "succeeded"
        && deployment.api_accessible != Some(false)
        && !matches!(
            deployment.runtime_status.as_str(),
            "stopped" | "offline" | "failed"
        )
        && (deployment.devices.is_empty() || deployment.devices.iter().any(|device| device.online));
    Ok(Some(StoredModel {
        info: ModelInfo {
            target,
            source: "discovered".into(),
            category: deployment.category,
            deployment_status: deployment.deployment_status,
            runtime_status: deployment.runtime_status,
            available,
            has_api_key: !deployment.api.api_key.is_empty(),
            devices: deployment
                .devices
                .into_iter()
                .map(|device| device.name)
                .collect(),
        },
        api_key: deployment.api.api_key,
    }))
}

pub(crate) async fn discover(
    State(state): State<AppState>,
    identity: UserIdentity,
    headers: HeaderMap,
) -> Response {
    match discover_models(&state, &identity, &headers).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

// Administrator-provisioned debugging credentials. This file is never part of
// the package, browser API, model registry, or inference configuration.
#[derive(Deserialize)]
struct DiscoveryApiAuth {
    uid: String,
    url: String,
    api_auth_token: String,
}

pub(super) async fn discovery_request(
    state: &AppState,
    identity: &UserIdentity,
    headers: &HeaderMap,
) -> Result<reqwest::RequestBuilder> {
    if state.config.discovery_url.is_empty() {
        bail!("自动发现未启用，请添加自定义 API");
    }
    let auth_path = state.config.data_dir.join("discovery-api-auth.json");
    match tokio::fs::read(auth_path).await {
        Ok(bytes) => {
            let auth: DiscoveryApiAuth =
                serde_json::from_slice(&bytes).context("模型发现调试配置格式无效")?;
            // An administrator's token must never be reused on behalf of a
            // different signed-in user. Other users retain normal delegation.
            if auth.uid == identity.uid {
                let url = reqwest::Url::parse(&auth.url).context("模型发现调试地址无效")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    bail!("模型发现调试地址必须是无内嵌凭据的 HTTPS 地址");
                }
                if auth.api_auth_token.trim().is_empty() {
                    bail!("模型发现调试凭据为空");
                }
                let mut token = axum::http::HeaderValue::from_str(&auth.api_auth_token)
                    .context("模型发现调试凭据格式无效")?;
                token.set_sensitive(true);
                return Ok(state
                    .config
                    .client
                    .get(url)
                    .header("Lzc-Api-Auth-Token", token));
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => bail!("无法读取模型发现调试配置"),
    }
    let url = reqwest::Url::parse(&state.config.discovery_url).context("模型发现地址无效")?;
    let mut request = state.config.client.get(url.clone());
    // Delegate tickets are only for the platform's canonical app ingress, never
    // for discovered inference URLs or arbitrary custom APIs. Do not persist them.
    if url
        .host_str()
        .is_some_and(|host| host.starts_with("app.") && host.ends_with(".lzcx"))
    {
        let ticket = match headers.get("x-hc-user-ticket") {
            Some(ticket) => Some(ticket.clone()),
            None => state
                .delegate_tickets
                .lock()
                .await
                .get(&identity.owner)
                .filter(|(_, time)| time.elapsed() < Duration::from_secs(900))
                .map(|(ticket, _)| ticket.clone()),
        }
        .context("尚未获得微服用户授权，请通过懒猫客户端重新打开应用，或配置自定义 API")?;
        request = request.header("X-HC-USER-TICKET", ticket);
    }
    Ok(request)
}

pub(super) async fn discover_models(
    state: &AppState,
    identity: &UserIdentity,
    headers: &HeaderMap,
) -> Result<ModelsResponse> {
    let request = discovery_request(state, identity, headers).await?;
    let response = request.send().await.context("无法连接模型发现服务")?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        state.delegate_tickets.lock().await.remove(&identity.owner);
        bail!("模型发现服务尚未授权，请检查微服应用间访问授权和发现地址；也可以添加自定义 API");
    }
    if !response.status().is_success() {
        bail!("模型发现服务返回 HTTP {}", response.status());
    }
    let discovery: Discovery =
        serde_json::from_value(read_json(response).await?).context("模型发现响应格式无效")?;
    if discovery.version != 1 {
        bail!("不支持的模型发现协议版本 {}", discovery.version);
    }
    let mut warnings: Vec<String> = discovery
        .warnings
        .into_iter()
        .map(|value| match value {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .collect();
    let mut models = vec![];
    let mut invalid = discovery.deployments.len() > 256;
    if invalid {
        warnings.push("部署记录超过 256 条，本次仅处理前 256 条并保留历史记录".into());
    }
    for deployment in discovery.deployments.into_iter().take(256) {
        let name = deployment.name.clone();
        match deployment_model(deployment) {
            Ok(Some(model)) => models.push(model),
            Ok(None) => {}
            Err(_) => {
                invalid = true;
                warnings.push(format!("{name} 的模型调用配置无效，已保留之前的发现记录"));
            }
        }
    }
    // Health metadata is optional. Failure never hides an otherwise discoverable
    // chat model; unknown status is explicitly allowed by the discovery contract.
    use futures::{stream, StreamExt};
    let client = &state.config.client;
    let models: Vec<_> = stream::iter(models)
        .map(|mut model| async move {
            if model.info.available {
                if let Ok(health) =
                    health_metadata(client, &model.info.target, &model.api_key).await
                {
                    if let Some(context) = health
                        .get("max_context")
                        .and_then(Value::as_u64)
                        .filter(|n| (512..=2_000_000).contains(n))
                    {
                        model.info.target.context_length = context as u32;
                        model.info.target.max_output_tokens = (context as u32 / 2).min(8192);
                    }
                }
            }
            model
        })
        .buffer_unordered(4)
        .collect()
        .await;
    let partial = discovery.partial || invalid;
    state
        .store
        .save_models(identity.owner.clone(), models, true, !partial)
        .await?;
    Ok(ModelsResponse {
        models: sorted(state.store.models(identity.owner.clone()).await?),
        discovery_enabled: true,
        partial,
        warnings,
    })
}
