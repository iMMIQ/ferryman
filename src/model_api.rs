//! OpenAI-compatible endpoint normalization and versioned translation adapters.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum PromptAdapter {
    #[default]
    Generic,
    IndexTranslate,
}

#[derive(Clone, Debug, Deserialize, Serialize, ts_rs::TS)]
pub struct ModelTarget {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub adapter: PromptAdapter,
    pub context_length: u32,
    pub max_output_tokens: u32,
    pub concurrency: usize,
}

impl ModelTarget {
    pub fn validate(&mut self) -> Result<()> {
        self.base_url = normalize_base_url(&self.base_url)?;
        self.model_id = self.model_id.trim().to_string();
        self.name = self.name.trim().to_string();
        if self.model_id.is_empty() || self.model_id.len() > 512 {
            bail!("请填写有效的模型 ID");
        }
        if self.name.is_empty() || self.name.len() > 512 {
            bail!("请填写模型名称");
        }
        if !(512..=2_000_000).contains(&self.context_length)
            || self.max_output_tokens == 0
            || self.max_output_tokens > self.context_length / 2
        {
            bail!("上下文至少为 512 tokens，输出上限须大于 0 且不超过上下文的一半");
        }
        // Zero is an unresolved discovered limit. It cannot be used to run a
        // job; custom connections require an explicit positive value.
        if self.concurrency > Semaphore::MAX_PERMITS {
            bail!("并发上限超出运行时支持范围");
        }
        Ok(())
    }

    pub fn prompt_char_budget(&self) -> usize {
        // Conservative for mixed scripts: reserve output, framing and batch tags.
        (self.context_length.saturating_sub(self.max_output_tokens) as usize / 2)
            .saturating_sub(256)
            .max(128)
    }

    pub fn cache_identity(&self) -> String {
        format!(
            "chat-adapter-v1:{:?}:{}:{}",
            self.adapter, self.context_length, self.max_output_tokens
        )
    }
}

#[derive(Default)]
pub struct RequestLimits {
    models: Mutex<HashMap<(String, String), Weak<RequestLimit>>>,
}

impl RequestLimits {
    pub fn for_model(&self, target: &ModelTarget, limit: usize) -> Arc<RequestLimit> {
        let mut models = self.models.lock().expect("request limit registry");
        models.retain(|_, limiter| limiter.strong_count() > 0);
        let key = (target.base_url.clone(), target.model_id.clone());
        if let Some(limiter) = models.get(&key).and_then(Weak::upgrade) {
            limiter.set_limit(limit);
            return limiter;
        }
        let limiter = Arc::new(RequestLimit::new(limit));
        models.insert(key, Arc::downgrade(&limiter));
        limiter
    }
}

struct LimitState {
    limit: usize,
    in_flight: usize,
}

pub struct RequestLimit {
    state: Mutex<LimitState>,
    changed: Notify,
}

impl RequestLimit {
    fn new(limit: usize) -> Self {
        assert!(limit > 0 && limit <= Semaphore::MAX_PERMITS);
        Self {
            state: Mutex::new(LimitState {
                limit,
                in_flight: 0,
            }),
            changed: Notify::new(),
        }
    }

    fn set_limit(&self, limit: usize) {
        assert!(limit > 0 && limit <= Semaphore::MAX_PERMITS);
        self.state.lock().expect("request limit state").limit = limit;
        self.changed.notify_waiters();
    }

    pub async fn acquire(self: &Arc<Self>) -> RequestPermit {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state.lock().expect("request limit state");
                if state.in_flight < state.limit {
                    state.in_flight += 1;
                    return RequestPermit {
                        limiter: self.clone(),
                    };
                }
            }
            changed.await;
        }
    }
}

pub struct RequestPermit {
    limiter: Arc<RequestLimit>,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.limiter
            .state
            .lock()
            .expect("request limit state")
            .in_flight -= 1;
        self.limiter.changed.notify_waiters();
    }
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value.trim())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("API 地址须为 HTTP(S) URL，不能包含账号、密码、查询参数或片段");
    }
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/chat/completions").unwrap_or(path);
    let path = if path.is_empty() { "/v1" } else { path };
    let path = path.to_string();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn chat_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.ends_with("/v1") {
        format!("{endpoint}/chat/completions")
    } else {
        format!("{endpoint}/v1/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> ModelTarget {
        ModelTarget {
            id: "test".into(),
            name: "test".into(),
            base_url: "http://test/v1".into(),
            model_id: "model".into(),
            adapter: PromptAdapter::Generic,
            context_length: 2048,
            max_output_tokens: 768,
            concurrency: 128,
        }
    }

    #[tokio::test]
    async fn requests_share_reported_capacity_and_release_on_cancellation() {
        use futures::FutureExt;
        let registry = RequestLimits::default();
        let first = registry.for_model(&target(), 128);
        let second = registry.for_model(&target(), 128);
        assert!(Arc::ptr_eq(&first, &second));
        let mut leases = Vec::new();
        for _ in 0..128 {
            leases.push(first.acquire().now_or_never().unwrap());
        }
        assert!(second.acquire().now_or_never().is_none());
        leases.pop();
        let replacement = second.acquire().now_or_never().unwrap();
        drop(replacement);
        drop(leases);
        assert_eq!(first.state.lock().unwrap().in_flight, 0);
        let mut other = target();
        other.model_id = "other".into();
        assert!(!Arc::ptr_eq(&first, &registry.for_model(&other, 128)));
    }

    #[tokio::test]
    async fn resized_limit_drains_active_requests_and_wakes_waiters() {
        use futures::FutureExt;
        let registry = RequestLimits::default();
        let limit = registry.for_model(&target(), 3);
        let a = limit.acquire().await;
        let b = limit.acquire().await;
        let c = limit.acquire().await;
        registry.for_model(&target(), 1);
        drop(a);
        drop(b);
        assert!(limit.acquire().now_or_never().is_none());
        drop(c);
        let held = limit.acquire().await;
        let waiter_limit = limit.clone();
        let waiter = tokio::spawn(async move { waiter_limit.acquire().await });
        tokio::task::yield_now().await;
        registry.for_model(&target(), 2);
        let lease = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        drop(lease);
        drop(held);
        assert_eq!(limit.state.lock().unwrap().in_flight, 0);
    }

    #[test]
    fn endpoints_preserve_provider_paths_without_duplicate_versions() {
        assert_eq!(
            normalize_base_url("http://localhost:18156").unwrap(),
            "http://localhost:18156/v1"
        );
        assert_eq!(
            normalize_base_url("https://example.com/api/v1/chat/completions/").unwrap(),
            "https://example.com/api/v1"
        );
        assert_eq!(
            chat_url("http://localhost:18156/v1"),
            "http://localhost:18156/v1/chat/completions"
        );
        assert!(normalize_base_url("https://user:secret@example.com/v1").is_err());
        assert!(normalize_base_url("file:///etc/passwd").is_err());
    }
}
