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

pub struct Engine {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    target: String,
    concurrency: usize,
    cache: Option<Cache>,
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
                let cached: Vec<Option<String>> = keys
                    .iter()
                    .map(|k| {
                        k.as_deref()
                            .and_then(|kk| cache.as_ref().and_then(|c| c.get(kk)))
                    })
                    .collect();
                if cached.iter().all(|v| v.is_some()) {
                    let pairs = ids
                        .into_iter()
                        .zip(cached.into_iter().map(|v| v.unwrap()))
                        .collect();
                    return UnitDone {
                        file,
                        attempted: n,
                        pairs,
                    };
                }

                let trs = translate::translate_batch(
                    client, endpoint, model, &cue_refs, &ctx_refs, target,
                )
                .await;
                let mut pairs = Vec::with_capacity(n);
                for idx in 0..n {
                    if let Some(tr) = &trs[idx] {
                        if let (Some(c), Some(k)) = (cache.as_ref(), keys[idx].as_deref()) {
                            c.put(k, tr);
                        }
                        pairs.push((ids[idx], tr.clone()));
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
