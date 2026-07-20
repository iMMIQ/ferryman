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

    /// Translate subtitle cues in contextual batches — the path used when
    /// [`crate::format::Document::batched`] is true.
    ///
    /// Cues are short and only make sense in the flow around them, so instead
    /// of one request per cue (which loses context) consecutive cues go out
    /// behind a single prompt and the response is aligned strictly one-to-one
    /// by `#N` (see [`crate::translate::translate_batch`]). `batch_size` caps
    /// cues per request; `context` cues preceding each batch ride along as
    /// read-only context (translated-free, not emitted).
    ///
    /// Same concurrency / caching / Ctrl-C semantics as [`Engine::translate`]:
    /// [`StreamExt::buffer_unordered`] bounds in-flight batches, each cue's
    /// translation is cached individually (so a re-run skips finished batches),
    /// and Ctrl-C stops dispatching new batches and returns a partial
    /// [`EngineOut`]. A batch that fails to align after retries fails as a unit
    /// — its cues are counted `failed` and emitted with their original text
    /// (never silently merged or split).
    pub async fn translate_subtitles(
        &self,
        segments: &[Segment],
        batch_size: usize,
        context: usize,
        limit: Option<usize>,
    ) -> EngineOut {
        // --limit takes the first N translatable cues in document order.
        let work: &[Segment] = match limit {
            Some(n) => &segments[..n.min(segments.len())],
            None => segments,
        };
        // Guard against a nonsensical 0.
        let batch_size = batch_size.max(1);

        // Slice the work into contiguous batches. Each batch carries its own
        // context = the `context` cues immediately preceding it (from `work`),
        // kept separate so the model sees continuity without those lines being
        // numbered, translated, or counted in the output.
        struct Batch {
            ids: Vec<SegmentId>,
            cues: Vec<String>,
            context: Vec<String>,
        }
        let mut batches: Vec<Batch> = Vec::new();
        let mut i = 0;
        while i < work.len() {
            let end = (i + batch_size).min(work.len());
            let ctx_start = i.saturating_sub(context);
            batches.push(Batch {
                ids: work[i..end].iter().map(|s| s.id).collect(),
                cues: work[i..end].iter().map(|s| s.text.clone()).collect(),
                context: work[ctx_start..i].iter().map(|s| s.text.clone()).collect(),
            });
            i = end;
        }

        let pb = ProgressBar::new(work.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        pb.set_message(format!(
            "translating {} cues in {} batch(es)",
            work.len(),
            batches.len()
        ));

        // Borrow &self once; the per-batch closure captures these by ref.
        let client = &self.client;
        let endpoint = &self.endpoint;
        let model = &self.model;
        let target = &self.target;
        let cache = &self.cache;

        let mut stream = stream::iter(batches.into_iter())
            .map(|b| {
                let n = b.cues.len();
                async move {
                    // References live inside the block so they borrow the moved `b`.
                    let cue_refs: Vec<&str> = b.cues.iter().map(|s| s.as_str()).collect();
                    let ctx_refs: Vec<&str> = b.context.iter().map(|s| s.as_str()).collect();
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
                        let pairs = b
                            .ids
                            .into_iter()
                            .zip(cached.into_iter().map(|v| v.unwrap()))
                            .collect();
                        return (n, Ok(pairs));
                    }

                    // Send the full batch. Cached cues inside a partially-cached
                    // batch are re-translated and re-cached — bounded waste (only
                    // the batch straddling a resume boundary), and it keeps the
                    // alignment simple (a contiguous #1..#N run).
                    match translate::translate_batch(
                        client, endpoint, model, &cue_refs, &ctx_refs, target,
                    )
                    .await
                    {
                        Ok(trs) => {
                            let mut pairs = Vec::with_capacity(n);
                            for idx in 0..n {
                                if let (Some(c), Some(k)) =
                                    (cache.as_ref(), keys[idx].as_deref())
                                {
                                    c.put(k, &trs[idx]);
                                }
                                pairs.push((b.ids[idx], trs[idx].clone()));
                            }
                            (n, Ok(pairs))
                        }
                        Err(e) => (n, Err(e)),
                    }
                }
            })
            .buffer_unordered(self.concurrency);

        let mut translations = Vec::new();
        let mut translated = 0usize;
        let mut failed = 0usize;
        let mut cancelled = false;

        // Same Ctrl-C race as translate(): stop dispatching, drop in-flight
        // batches, return partial. The progress bar advances by batch size so a
        // 25-cue batch finishing moves all 25 at once.
        let cancel = tokio::signal::ctrl_c();
        tokio::pin!(cancel);

        loop {
            tokio::select! {
                biased;
                _ = &mut cancel => {
                    cancelled = true;
                    break;
                }
                item = stream.next() => match item {
                    None => break,
                    Some((n, Ok(pairs))) => {
                        translations.extend(pairs);
                        translated += n;
                        pb.inc(n as u64);
                    }
                    Some((n, Err(e))) => {
                        failed += n;
                        pb.inc(n as u64);
                        eprintln!("warn: subtitle batch ({} cues) failed: {}", n, e);
                    }
                }
            }
        }
        drop(stream);
        pb.finish_and_clear();

        translations.sort_by_key(|(id, _)| *id);
        EngineOut {
            translations,
            translated,
            failed,
            cancelled,
        }
    }
}
