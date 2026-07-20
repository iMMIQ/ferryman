//! Format-agnostic translation engine.
//!
//! Takes a flat list of [`Segment`]s and translates them concurrently via the
//! LLM, with a progress bar and a global `--limit`. Extracted from `main` so
//! adding a new format needs no change here — the engine only ever sees plain
//! text, plus a [`Strategy`] telling it how to batch the work.
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
//!
//! ## One method, two strategies
//!
//! [`Engine::translate`] handles both [`Strategy::Independent`] (one segment per
//! request, via [`translate::translate`]) and [`Strategy::Batched`] (N segments
//! per request with context, via [`translate::translate_batch`]). The two paths
//! share one drain loop, cache, and Ctrl-C race; only the per-item future and
//! the request shape differ. Batching is a translation strategy orthogonal to
//! format grammar — any format may opt in via [`crate::format::Document::strategy`].

use crate::cache::Cache;
use crate::format::{Segment, SegmentId, Strategy};
use crate::translate;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::pin::Pin;

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

/// One dispatched unit of work, normalized across strategies: `attempted`
/// segments went out, `pairs` came back translated. An Independent item has
/// `attempted == 1`; a Batched request has `attempted == N`. The drain loop
/// only needs these two counts — `failed += attempted - pairs.len()`.
struct ItemOutcome {
    attempted: usize,
    pairs: Vec<(SegmentId, String)>,
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

    /// Translate `segments` per `strategy`.
    ///
    /// [`Strategy::Independent`] sends one segment per request (self-contained
    /// blocks, e.g. EPUB paragraphs); [`Strategy::Batched`] sends `batch_size`
    /// consecutive segments per request, each preceded by `context` read-only
    /// segments, and aligns the result strictly one-to-one (subtitle cues,
    /// continuous prose). `limit` caps the total translated (applied by
    /// truncating before fan-out; cache hits consume the budget just like
    /// misses).
    ///
    /// Single failures are logged and counted, never bubbled — one bad block
    /// can't abort the run. On Ctrl-C the run stops early and returns the
    /// partial result with [`EngineOut::cancelled`] set.
    ///
    /// Both strategies share one concurrency bound, one cache, one Ctrl-C race,
    /// and one drain loop; only the per-item future differs. The two stream
    /// sources are boxed behind a common `dyn Stream<Item = ItemOutcome>` so
    /// the drain is unified — a single heap allocation, negligible next to the
    /// HTTP work each item drives.
    pub async fn translate(
        &self,
        segments: &[Segment],
        strategy: Strategy,
        limit: Option<usize>,
    ) -> EngineOut {
        // --limit takes the first N segments in document order regardless of
        // cache hit/miss.
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
        pb.set_message(format!("translating {} segment(s)", work.len()));

        // Borrow &self once; the per-item closures capture these by ref, so
        // endpoint/model/target are never cloned per segment.
        let client = &self.client;
        let endpoint = &self.endpoint;
        let model = &self.model;
        let target = &self.target;
        let cache = &self.cache;

        let mut stream: Pin<Box<dyn stream::Stream<Item = ItemOutcome> + Send>> = match strategy {
            Strategy::Independent => Box::pin(
                stream::iter(work.iter())
                    .map(|seg| {
                        let text = &seg.text;
                        // Hash once; shared by the get() and put() below.
                        let key = cache.as_ref().map(|c| c.key(model, target, text));
                        async move {
                            // Cache hit short-circuit — no HTTP, no slot held beyond µs.
                            if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                                if let Some(v) = c.get(k) {
                                    return ItemOutcome {
                                        attempted: 1,
                                        pairs: vec![(seg.id, v)],
                                    };
                                }
                            }
                            match translate::translate(client, endpoint, model, text, target).await {
                                Ok(tr) => {
                                    // Put before returning: even if the future is
                                    // dropped right after (Ctrl-C between completion
                                    // and drain), the next run finds the cache
                                    // populated.
                                    if let (Some(c), Some(k)) = (cache.as_ref(), key.as_deref()) {
                                        c.put(k, &tr);
                                    }
                                    ItemOutcome {
                                        attempted: 1,
                                        pairs: vec![(seg.id, tr)],
                                    }
                                }
                                Err(e) => {
                                    eprintln!("warn: segment {} failed: {}", seg.id, e);
                                    ItemOutcome {
                                        attempted: 1,
                                        pairs: vec![],
                                    }
                                }
                            }
                        }
                    })
                    .buffer_unordered(self.concurrency),
            ),
            Strategy::Batched {
                batch_size,
                context,
            } => {
                let batch_size = batch_size.max(1); // guard against a nonsensical 0.

                // Slice the work into contiguous batches. Each batch carries its
                // own context = the `context` segments immediately preceding it
                // (from `work`), kept separate so the model sees continuity
                // without those lines being numbered, translated, or counted.
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
                pb.set_message(format!(
                    "translating {} segment(s) in {} batch(es)",
                    work.len(),
                    batches.len()
                ));

                Box::pin(
                    stream::iter(batches)
                        .map(|b| {
                            let n = b.cues.len();
                            async move {
                                // References live inside the block so they borrow the moved `b`.
                                let cue_refs: Vec<&str> = b.cues.iter().map(|s| s.as_str()).collect();
                                let ctx_refs: Vec<&str> =
                                    b.context.iter().map(|s| s.as_str()).collect();
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
                                    return ItemOutcome {
                                        attempted: n,
                                        pairs,
                                    };
                                }

                                // translate_batch returns one Option per cue: Some = done,
                                // None = the model skipped/failed that cue (kept original by
                                // the writer). Cache the wins; a partial batch still yields
                                // every cue it could — one degenerate cue costs only itself.
                                let trs = translate::translate_batch(
                                    client, endpoint, model, &cue_refs, &ctx_refs, target,
                                )
                                .await;
                                let mut pairs = Vec::with_capacity(n);
                                for idx in 0..n {
                                    if let Some(tr) = &trs[idx] {
                                        if let (Some(c), Some(k)) =
                                            (cache.as_ref(), keys[idx].as_deref())
                                        {
                                            c.put(k, tr);
                                        }
                                        pairs.push((b.ids[idx], tr.clone()));
                                    }
                                }
                                ItemOutcome {
                                    attempted: n,
                                    pairs,
                                }
                            }
                        })
                        .buffer_unordered(self.concurrency),
                )
            }
        };

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
                    // `attempted` went out; `pairs` came back. The gap (attempted
                    // - pairs.len()) is the failed/skipped count — one degenerate
                    // cue in a batch costs only itself, never the whole batch.
                    Some(out) => {
                        translated += out.pairs.len();
                        failed += out.attempted - out.pairs.len();
                        translations.extend(out.pairs);
                        pb.inc(out.attempted as u64);
                    }
                }
            }
        }
        drop(stream);
        pb.finish_and_clear();

        // Items complete out of order; sort for stable diffs.
        translations.sort_by_key(|(id, _)| *id);
        EngineOut {
            translations,
            translated,
            failed,
            cancelled,
        }
    }
}
