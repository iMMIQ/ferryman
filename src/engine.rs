//! The vLLM sender — translates one self-contained [`Unit`] per call.
//!
//! The engine is deliberately minimal: [`Engine::exec_unit`] takes a [`Unit`]
//! (an Independent segment, or a Batched slice with its own in-file context),
//! consults the cache, calls `translate`/`translate_batch`, caches the wins, and
//! returns a [`UnitDone`]. It never errors (a unit failure is logged and counted
//! as empty `pairs`), so one bad unit can't abort a batch.
//!
//! Everything above one unit — the shared concurrency pool, lazy file opening,
//! progressive writing, Ctrl-C, the progress bar — lives in [`crate::batch`],
//! the queue layer. That split keeps the engine free of any notion of files,
//! formats, or strategies: it only knows how to send one chunk of text to vLLM.

use crate::cache::Cache;
use crate::format::SegmentId;
use crate::translate;
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub fn build_translation_client(
    timeout: Duration,
    bearer_token: Option<&str>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(token) = bearer_token {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        builder = builder.default_headers(headers);
    }
    Ok(builder.build()?)
}

pub struct Engine {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    target: String,
    concurrency: usize,
    cache: Option<Cache>,
    request_limiter: Option<Arc<Semaphore>>,
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
        Engine {
            client,
            endpoint,
            model,
            target,
            concurrency,
            cache,
            request_limiter: None,
        }
    }

    /// Share an aggregate request budget with other engines. The Web scheduler
    /// uses one limiter per model preset so concurrent jobs cannot multiply the
    /// configured vLLM concurrency.
    pub fn with_request_limiter(mut self, limiter: Arc<Semaphore>) -> Self {
        self.request_limiter = Some(limiter);
        self
    }

    /// The shared pool's in-flight cap (the queue layer sizes itself to this).
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    async fn acquire_request_permit(&self) -> Option<OwnedSemaphorePermit> {
        match &self.request_limiter {
            Some(limiter) => Some(
                limiter
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("translation request limiter must remain open"),
            ),
            None => None,
        }
    }

    /// Translate one [`Unit`]: cache fast-path, then `translate`/`translate_batch`,
    /// caching the wins. Never returns `Err` — a single segment or batch failure
    /// is logged and counted (empty `pairs`), never bubbled, so one bad unit
    /// can't abort the batch.
    pub async fn exec_unit(&self, unit: Unit) -> UnitDone {
        let client = &self.client;
        let endpoint = &self.endpoint;
        let model = &self.model;
        let target = &self.target;
        let cache = &self.cache;
        let file = unit.file();

        match unit {
            // Independent: one segment per request, cache checked and filled
            // around a single translate() call.
            Unit::Single { id, text, .. } => {
                let key = cache.as_ref().map(|c| c.key(model, target, &text));
                if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                    if let Some(v) = c.get(k) {
                        return UnitDone {
                            file,
                            attempted: 1,
                            pairs: vec![(id, v)],
                        };
                    }
                }
                let _request_permit = self.acquire_request_permit().await;
                match translate::translate(client, endpoint, model, &text, target).await {
                    Ok(tr) => {
                        // Put before returning: even if the future is dropped
                        // right after (Ctrl-C between completion and drain),
                        // the next run finds the cache populated.
                        if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                            c.put(k, &tr);
                        }
                        UnitDone {
                            file,
                            attempted: 1,
                            pairs: vec![(id, tr)],
                        }
                    }
                    Err(e) => {
                        eprintln!("warn: segment {} failed: {}", id, e);
                        UnitDone {
                            file,
                            attempted: 1,
                            pairs: vec![],
                        }
                    }
                }
            }

            // Batched: N cues per request with read-only context. An all-cached
            // fast path skips the HTTP round-trip; otherwise translate_batch
            // returns one Option per cue (Some = done, None = the model
            // skipped/failed that cue, kept original by the writer). A partial
            // batch still yields every cue it could — one degenerate cue costs
            // only itself.
            Unit::Batch {
                ids, cues, context, ..
            } => {
                let n = cues.len();
                if n == 0 {
                    return UnitDone {
                        file,
                        attempted: 0,
                        pairs: vec![],
                    };
                }
                let cue_refs: Vec<&str> = cues.iter().map(|s| s.as_str()).collect();
                let ctx_refs: Vec<&str> = context.iter().map(|s| s.as_str()).collect();
                // Per-cue cache keys (shared by the get fast-path and the put).
                let keys: Vec<Option<String>> = cue_refs
                    .iter()
                    .map(|t| cache.as_ref().map(|c| c.key(model, target, t)))
                    .collect();

                // All-cached fast path: skip the HTTP round-trip entirely.
                let mut results: Vec<Option<String>> = keys
                    .iter()
                    .map(|k| {
                        k.as_deref()
                            .and_then(|kk| cache.as_ref().and_then(|c| c.get(kk)))
                    })
                    .collect();
                if results.iter().all(|v| v.is_some()) {
                    let pairs = ids
                        .into_iter()
                        .zip(results.into_iter().map(|v| v.unwrap()))
                        .collect();
                    return UnitDone {
                        file,
                        attempted: n,
                        pairs,
                    };
                }

                // Partial-cache path: request only the misses and keep the
                // hits. Re-running a batch whose previous attempt half-failed
                // must not re-translate (and re-bill) the cues that already
                // succeeded.
                let miss_idx: Vec<usize> = (0..n).filter(|&idx| results[idx].is_none()).collect();
                let miss_cues: Vec<&str> = miss_idx.iter().map(|&idx| cue_refs[idx]).collect();
                let _request_permit = self.acquire_request_permit().await;
                let trs = translate::translate_batch(
                    client, endpoint, model, &miss_cues, &ctx_refs, target,
                )
                .await;
                for (slot, &idx) in miss_idx.iter().enumerate() {
                    if let Some(tr) = &trs[slot] {
                        if let (Some(c), Some(k)) = (cache.as_ref(), keys[idx].as_deref()) {
                            c.put(k, tr);
                        }
                        results[idx] = Some(tr.clone());
                    }
                }
                let mut pairs = Vec::with_capacity(n);
                for idx in 0..n {
                    if let Some(tr) = results[idx].take() {
                        pairs.push((ids[idx], tr));
                    }
                }
                UnitDone {
                    file,
                    attempted: n,
                    pairs,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_engine(limiter: Arc<Semaphore>) -> Engine {
        Engine::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_string(),
            "model".to_string(),
            "target".to_string(),
            8,
            None,
        )
        .with_request_limiter(limiter)
    }

    #[tokio::test]
    async fn engines_share_one_request_budget() {
        let limiter = Arc::new(Semaphore::new(1));
        let first = test_engine(limiter.clone());
        let second = test_engine(limiter);

        let first_permit = first.acquire_request_permit().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), second.acquire_request_permit())
                .await
                .is_err()
        );

        drop(first_permit);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), second.acquire_request_permit())
                .await
                .is_ok()
        );
    }

    /// A partial-cache batch must send only the missing cues to vLLM and reuse
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
                    let content = body["messages"][0]["content"].as_str().unwrap().to_string();
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
        // Prime the cache for two of the three cues.
        for text in ["one", "two"] {
            let key = cache.key("model", "target", text);
            cache.put(&key, &format!("cached-{text}"));
        }

        let engine = Engine::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            "model".to_string(),
            "target".to_string(),
            4,
            Some(cache),
        );
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

        std::fs::remove_dir_all(cache_dir).ok();
    }

    fn uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
