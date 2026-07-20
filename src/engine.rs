//! Format-agnostic translation engine.
//!
//! Takes a flat list of [`Segment`]s and translates them concurrently via the
//! LLM, with a progress bar and a global `--limit`. Extracted from `main` so
//! adding a new format needs no change here — the engine only ever sees plain
//! text.
//!
//! - **Concurrency** is bounded by [`StreamExt::buffer_unordered`] (no separate
//!   `Semaphore`): only `concurrency` requests are in flight at once, and
//!   completed results stream in as soon as they're ready instead of waiting
//!   on the whole batch.
//! - **Caching**: translations are persisted on disk ([`crate::cache::Cache`])
//!   so re-runs skip already-translated blocks and a Ctrl-C'd run keeps every
//!   block that finished. Cache hits are short-circuited before any HTTP call.
//! - **Cancellation**: Ctrl-C stops dispatching new requests, drops the few
//!   in-flight HTTP futures, and returns a partial [`EngineOut`] so `main` can
//!   still write what completed.

use crate::cache::Cache;
use crate::format::{Segment, SegmentId};
use crate::translate;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};

pub struct Engine {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    target: String,
    concurrency: usize,
    cache: Option<Cache>,
}

/// Outcome of an [`Engine::translate`] run.
pub struct EngineOut {
    /// `(segment_id, translation)` for every segment that succeeded, sorted by
    /// id (the stream completes out of order; `Document::write` is order-
    /// independent but a sorted output stays diff-stable across runs).
    pub translations: Vec<(SegmentId, String)>,
    /// Count of segments translated successfully (including cache hits).
    pub translated: usize,
    /// Count of segments whose translation failed (already logged).
    pub failed: usize,
    /// True if the run was interrupted by Ctrl-C. `translations` then holds the
    /// partial set; `main` still writes it so completed work isn't lost.
    pub cancelled: bool,
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

    /// Translate `segments` concurrently. `limit` caps the total number
    /// translated (applied by truncating before fan-out; cache hits consume the
    /// budget just like misses). Single failures are logged and counted, never
    /// bubbled — one bad block can't abort the run. On Ctrl-C the run stops
    /// early and returns the partial result with [`EngineOut::cancelled`] set.
    pub async fn translate(&self, segments: &[Segment], limit: Option<usize>) -> EngineOut {
        // One concurrent pass over the whole document. --limit takes the first N
        // segments in document order regardless of cache hit/miss.
        let work: &[Segment] = match limit {
            Some(n) => &segments[..n.min(segments.len())],
            None => segments,
        };

        let pb = ProgressBar::new(work.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        pb.set_message(format!("translating {} blocks", work.len()));

        // Borrow &self once; the per-segment closure captures these by ref, so
        // endpoint/model/target are never cloned per segment.
        let client = &self.client;
        let endpoint = &self.endpoint;
        let model = &self.model;
        let target = &self.target;
        let cache = &self.cache;

        let mut stream = stream::iter(work.iter())
            .map(|seg| {
                let text = &seg.text;
                // Hash once; shared by the get() and put() below.
                let key = cache.as_ref().map(|c| c.key(model, target, text));
                async move {
                    // Cache hit short-circuit — no HTTP, no slot held beyond µs.
                    if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                        if let Some(v) = c.get(k) {
                            return (seg.id, Ok(v));
                        }
                    }
                    let res = translate::translate(client, endpoint, model, text, target).await;
                    match res {
                        Ok(tr) => {
                            // Put before returning: even if the future is
                            // dropped right after (Ctrl-C between completion and
                            // drain), the next run finds the cache populated.
                            if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                                c.put(k, &tr);
                            }
                            (seg.id, Ok(tr))
                        }
                        Err(e) => (seg.id, Err(e)),
                    }
                }
            })
            .buffer_unordered(self.concurrency);

        let mut translations = Vec::new();
        let mut translated = 0usize;
        let mut failed = 0usize;
        let mut cancelled = false;

        // Race the drain loop against Ctrl-C. On signal we stop dispatching new
        // requests, drop the stream (cancelling the few in-flight HTTP futures —
        // a dropped reqwest future closes its connection), and return partial.
        let cancel = tokio::signal::ctrl_c();
        tokio::pin!(cancel);

        loop {
            tokio::select! {
                // Poll the cancel future first so Ctrl-C is observed promptly.
                biased;
                _ = &mut cancel => {
                    cancelled = true;
                    break;
                }
                item = stream.next() => match item {
                    None => break,
                    Some((id, Ok(tr))) => {
                        translations.push((id, tr));
                        translated += 1;
                        pb.inc(1);
                    }
                    Some((id, Err(e))) => {
                        failed += 1;
                        pb.inc(1);
                        eprintln!("warn: segment {} failed: {}", id, e);
                    }
                }
            }
        }
        drop(stream);
        pb.finish_and_clear();

        // buffer_unordered yields in completion order; sort for stable diffs.
        translations.sort_by_key(|(id, _)| *id);
        EngineOut {
            translations,
            translated,
            failed,
            cancelled,
        }
    }
}
