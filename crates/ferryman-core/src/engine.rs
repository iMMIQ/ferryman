//! Cached translation of format-independent units through OpenAI-compatible HTTP APIs.

use crate::cache::Cache;
use crate::format::SegmentId;
use crate::model_api::{ModelTarget, RequestLimit};
use crate::translate;
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::sync::Arc;
use std::time::Duration;

pub fn build_translation_client(
    timeout: Duration,
    bearer_token: Option<&str>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(token) = bearer_token {
        let mut headers = HeaderMap::new();
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    Ok(builder.build()?)
}

// Include all neighboring cues and the position: identical words can carry
// different meanings even within one batch. Singles use a separate namespace.
fn cache_scope(endpoint: &str, cues: &[&str], context: &[&str], index: Option<usize>) -> String {
    serde_json::to_string(&(endpoint.trim_end_matches('/'), index, cues, context))
        .expect("serialize translation scope")
}

pub struct Engine {
    client: reqwest::Client,
    model_target: ModelTarget,
    target: String,
    concurrency: usize,
    cache: Option<Cache>,
    checkpoint: Option<Cache>,
    api_key: Option<String>,
    request_limit: Option<Arc<RequestLimit>>,
}

/// One self-contained unit of translation work. The caller ([`crate::batch`])
/// tags each with its file index so results can be routed back; the engine
/// never inspects `file`.
#[derive(Clone, Debug)]
pub enum Unit {
    /// One Independent segment → one `translate()` call.
    Single {
        file: usize,
        id: SegmentId,
        text: String,
    },
    /// One Batched slice → one `translate_batch()` call. `context` is read-only
    /// preceding cues from the same file (not translated, not emitted).
    Batch {
        file: usize,
        ids: Vec<SegmentId>,
        cues: Vec<String>,
        context: Vec<String>,
    },
}

impl Unit {
    /// How many segments this unit attempts (1 for Single, `cues.len()` for
    /// Batch). The queue layer advances the progress bar by this much.
    pub fn attempted(&self) -> usize {
        match self {
            Unit::Single { .. } => 1,
            Unit::Batch { cues, .. } => cues.len(),
        }
    }

    fn file(&self) -> usize {
        match self {
            Unit::Single { file, .. } | Unit::Batch { file, .. } => *file,
        }
    }
}

/// Outcome of executing one [`Unit`]: `attempted` segments went out, `pairs`
/// came back translated. A Single has `attempted == 1`; a Batch has
/// `attempted == cues.len()`. The gap is the failed/skipped count.
#[derive(Debug)]
pub struct UnitDone {
    pub file: usize,
    pub attempted: usize,
    pub pairs: Vec<(SegmentId, String)>,
}

impl Engine {
    pub fn new(
        client: reqwest::Client,
        endpoint: String,
        model: String,
        target: String,
        concurrency: usize,
        cache: Option<Cache>,
    ) -> Self {
        let model_target = ModelTarget::from_endpoint(&endpoint, &model, concurrency);
        Engine {
            client,
            model_target,
            target,
            concurrency,
            cache,
            checkpoint: None,
            api_key: None,
            request_limit: None,
        }
    }

    pub fn with_model_target(mut self, target: ModelTarget) -> Self {
        self.model_target = target;
        self
    }

    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key.filter(|key| !key.is_empty());
        self
    }

    pub fn with_request_limit(mut self, limit: Arc<RequestLimit>) -> Self {
        self.request_limit = Some(limit);
        self
    }

    /// Job-local progress survives retries even when shared caching is off.
    pub fn with_checkpoint(mut self, checkpoint: Option<Cache>) -> Self {
        self.checkpoint = checkpoint;
        self
    }

    async fn cached(&self, key: &str) -> Option<String> {
        if let Some(checkpoint) = &self.checkpoint {
            if let Some(value) = checkpoint.get(key).await {
                return Some(value);
            }
        }
        let value = self.cache.as_ref()?.get(key).await?;
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.put(key, &value).await;
        }
        Some(value)
    }

    async fn remember(&self, key: &str, value: &str) {
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.put(key, value).await;
        }
        if let Some(cache) = &self.cache {
            cache.put(key, value).await;
        }
    }

    /// The shared pool's in-flight cap (the queue layer sizes itself to this).
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Translate one [`Unit`]: cache fast-path, then `translate`/`translate_batch`,
    /// caching the wins. Never returns `Err` — a single segment or batch failure
    /// is logged and counted (empty `pairs`), never bubbled, so one bad unit
    /// can't abort the batch.
    pub async fn exec_unit(&self, unit: Unit) -> UnitDone {
        let client = translate::ChatClient {
            client: &self.client,
            target: &self.model_target,
            api_key: self.api_key.as_deref(),
            limit: self.request_limit.as_ref(),
        };
        let file = unit.file();
        let single = matches!(unit, Unit::Single { .. });
        let (ids, cues, context) = match unit {
            Unit::Single { id, text, .. } => (vec![id], vec![text], vec![]),
            Unit::Batch {
                ids, cues, context, ..
            } => (ids, cues, context),
        };
        let attempted = cues.len();
        let cue_refs: Vec<_> = cues.iter().map(String::as_str).collect();
        let ctx_refs: Vec<_> = context.iter().map(String::as_str).collect();
        let cache_enabled = self.cache.is_some() || self.checkpoint.is_some();
        let keys: Vec<_> = cues
            .iter()
            .enumerate()
            .map(|(index, text)| {
                cache_enabled.then(|| {
                    let scope = if single {
                        cache_scope(&self.model_target.base_url, &[], &[], None)
                    } else {
                        cache_scope(
                            &self.model_target.base_url,
                            &cue_refs,
                            &ctx_refs,
                            Some(index),
                        )
                    };
                    let scope = format!("{}:{scope}", self.model_target.cache_identity());
                    Cache::key(&self.model_target.model_id, &self.target, text, &scope)
                })
            })
            .collect();
        let mut results = Vec::with_capacity(attempted);
        for key in &keys {
            results.push(match key {
                Some(key) => self.cached(key).await,
                None => None,
            });
        }
        let misses: Vec<_> = (0..attempted)
            .filter(|&index| results[index].is_none())
            .collect();
        if !misses.is_empty() {
            let translations = if single {
                let result = translate::translate_using(&client, &cues[0], &self.target).await;
                if let Err(error) = &result {
                    tracing::warn!(segment = ids[0], %error, "segment translation failed");
                }
                vec![result.ok()]
            } else {
                let texts: Vec<_> = misses.iter().map(|&index| cue_refs[index]).collect();
                translate::translate_batch_using(&client, &texts, &ctx_refs, &self.target).await
            };
            for (index, translation) in misses.into_iter().zip(translations) {
                if let Some(translation) = translation {
                    if let Some(key) = &keys[index] {
                        self.remember(key, &translation).await;
                    }
                    results[index] = Some(translation);
                }
            }
        }
        UnitDone {
            file,
            attempted,
            pairs: ids
                .into_iter()
                .zip(results)
                .filter_map(|(id, text)| text.map(|text| (id, text)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn cache_scope_distinguishes_context_strategy_neighbors_and_position() {
        let base = cache_scope("http://model", &["bank", "river"], &["water"], Some(0));
        for different in [
            cache_scope("http://other", &["bank", "river"], &["water"], Some(0)),
            cache_scope("http://model", &["bank", "river"], &["money"], Some(0)),
            cache_scope("http://model", &["bank", "money"], &["water"], Some(0)),
            cache_scope("http://model", &["bank", "river"], &["water"], Some(1)),
            cache_scope("http://model", &[], &[], None),
        ] {
            assert_ne!(base, different);
        }
    }

    /// A partial-cache batch must send only the missing cues to orinfer and reuse
    /// the cached translations for the rest — a rerun after a half-failed
    /// batch must not re-translate (or re-bill) the cues that already landed.
    #[tokio::test]
    async fn batch_units_translate_only_cache_misses() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let prompts: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_prompts = prompts.clone();
        let handler_attempts = attempts.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(
                move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let content = body["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    handler_prompts.lock().unwrap().push(content.clone());
                    handler_attempts.fetch_add(1, Ordering::Relaxed);
                    // Echo every <cN>…</cN> tag back with a translated marker so
                    // parse_tagged sees a perfect response.
                    let translated = translate::parse_tagged(&content, 64);
                    let mut out = String::new();
                    for (idx, slot) in translated.iter().enumerate() {
                        if let Some(text) = slot {
                            out.push_str(&format!("<c{}>{}</c{}>\n", idx + 1, text, idx + 1));
                        }
                    }
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"content": out.trim_end()}}]
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let cache_dir = std::env::temp_dir().join(format!("ferryman-cache-test-{}", uuid()));
        let cache = Cache::open(Some(cache_dir.clone())).unwrap();
        let target = ModelTarget::from_endpoint(&format!("http://{addr}"), "model", 4);
        // Prime the cache for two of the three cues.
        for (index, text) in ["one", "two"].into_iter().enumerate() {
            let scope = format!(
                "{}:{}",
                target.cache_identity(),
                cache_scope(&target.base_url, &["one", "two", "three"], &[], Some(index),)
            );
            let key = Cache::key("model", "target", text, &scope);
            cache.put(&key, &format!("cached-{text}")).await;
        }

        let engine = Engine::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            "model".to_string(),
            "target".to_string(),
            4,
            None,
        )
        .with_checkpoint(Some(cache));
        let done = engine
            .exec_unit(Unit::Batch {
                file: 0,
                ids: vec![1, 2, 3],
                cues: vec!["one".to_string(), "two".to_string(), "three".to_string()],
                context: vec![],
            })
            .await;

        assert_eq!(done.attempted, 3);
        assert_eq!(
            done.pairs,
            vec![
                (1, "cached-one".to_string()),
                (2, "cached-two".to_string()),
                (3, "three".to_string()), // round-tripped through the fake server
            ]
        );
        // Exactly one request, carrying only the missing cue.
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        {
            let prompts = prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(
                prompts[0].contains("<c1>three</c1>"),
                "only the miss goes out"
            );
            assert!(
                !prompts[0].contains("<c1>one</c1>"),
                "cached cues stay home"
            );
        }
        // Reopen only the job checkpoint: shared caching stays disabled.
        let engine = Engine::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            "model".into(),
            "target".into(),
            4,
            None,
        )
        .with_checkpoint(Cache::open(Some(cache_dir.clone())));
        // Same full request resumes entirely from cache; a different context
        // must send all cues again instead of reusing the old meaning.
        for (context, expected_calls) in [(vec![], 1), (vec!["different narrative".into()], 2)] {
            let result = engine
                .exec_unit(Unit::Batch {
                    file: 1,
                    ids: vec![1, 2, 3],
                    cues: vec!["one".into(), "two".into(), "three".into()],
                    context,
                })
                .await;
            assert_eq!(result.pairs.len(), 3);
            assert_eq!(attempts.load(Ordering::Relaxed), expected_calls);
            if expected_calls == 2 {
                assert_eq!(result.pairs[0].1, "one");
            }
        }

        std::fs::remove_dir_all(cache_dir).ok();
    }

    #[tokio::test]
    async fn independent_jobs_share_actual_http_request_limit_above_four() {
        use crate::model_api::{ModelTarget, PromptAdapter, RequestLimits};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Semaphore;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let (handler_active, handler_peak, handler_release) =
            (active.clone(), peak.clone(), release.clone());
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move || {
            let (active, peak, release) =
                (handler_active.clone(), handler_peak.clone(), handler_release.clone());
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                release.acquire().await.unwrap().forget();
                active.fetch_sub(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"choices":[{"message":{"content":"translated"},"finish_reason":"stop"}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = ModelTarget {
            id: "test".into(),
            name: "test".into(),
            base_url: base.clone(),
            model_id: "model".into(),
            adapter: PromptAdapter::Generic,
            context_length: 2048,
            max_output_tokens: 768,
            concurrency: 6,
        };
        let registry = RequestLimits::default();
        let engines: Vec<_> = (0..2)
            .map(|_| {
                Arc::new(
                    Engine::new(
                        reqwest::Client::new(),
                        base.clone(),
                        "model".into(),
                        "Chinese".into(),
                        6,
                        None,
                    )
                    .with_request_limit(registry.for_model(&target, 6))
                    .with_model_target(target.clone()),
                )
            })
            .collect();
        let mut jobs = Vec::new();
        for i in 0..12 {
            let engine = engines[i % 2].clone();
            jobs.push(tokio::spawn(async move {
                engine
                    .exec_unit(Unit::Single {
                        file: i % 2,
                        id: i,
                        text: format!("text {i}"),
                    })
                    .await
            }));
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while active.load(Ordering::SeqCst) < 6 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 6);
        release.add_permits(12);
        for job in jobs {
            let done = tokio::time::timeout(Duration::from_secs(3), job)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(done.pairs.len(), 1);
        }
        assert_eq!(peak.load(Ordering::SeqCst), 6);
        server.abort();
    }

    fn uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
