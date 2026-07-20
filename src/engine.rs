//! Format-agnostic translation engine — one shared concurrency pool.
//!
//! The caller ([`crate::main`]) turns every input file's segments into
//! self-contained [`Unit`]s (an Independent segment → [`Unit::Single`]; a
//! Batched slice → [`Unit::Batch`] carrying its own in-file context) and hands
//! the whole flattened batch to [`Engine::run`]. All units — across every file
//! — drain through **one** `buffer_unordered(concurrency)` pool, so small files
//! and file tails no longer leave the GPU idle the way a per-file loop does.
//!
//! - **Concurrency** is bounded by [`StreamExt::buffer_unordered`] (no separate
//!   `Semaphore`): only `concurrency` requests are in flight at once, and
//!   completed results stream in as soon as they're ready.
//! - **Caching**: translations are persisted on disk ([`crate::cache::Cache`])
//!   so re-runs skip already-translated blocks and a Ctrl-C'd run keeps every
//!   block that finished. Cache hits are short-circuited before any HTTP call.
//! - **Cancellation**: Ctrl-C stops dispatching new units, drops the few
//!   in-flight HTTP futures, and returns a partial [`RunOut`] so `main` can
//!   still write what completed.
//!
//! ## Why the caller builds units
//!
//! A unit is self-contained — [`translate::translate`] (one segment) and
//! [`translate::translate_batch`] (N cues + read-only context) are stateless
//! borrowed calls with no cross-unit dependency. The only batching constraint
//! is *within* a file (a batch's context is the same file's preceding cues), so
//! the caller builds per-file units and flattens them; the engine stays a dumb
//! executor that knows nothing about files, formats, or strategies.

use crate::cache::Cache;
use crate::format::SegmentId;
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

/// One self-contained unit of translation work. The caller tags each with its
/// file index so results can be routed back; the engine never inspects `file`.
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
    /// Batch). The drain loop advances the progress bar by this much.
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

/// Outcome of [`Engine::run`]. `done` is in **completion order** (the stream
/// completes out of order); the caller partitions by `file` and sorts each
/// file's pairs by id for a diff-stable write.
#[derive(Debug)]
pub struct RunOut {
    pub done: Vec<UnitDone>,
    /// Count of segments translated successfully (including cache hits).
    pub translated: usize,
    /// Count of segments whose translation failed (already logged per unit).
    pub failed: usize,
    /// True if the run was interrupted by Ctrl-C. `done` then holds the partial
    /// set; `main` still writes it so completed work isn't lost.
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

    /// Execute one [`Unit`]: cache fast-path, then `translate`/`translate_batch`,
    /// caching the wins. Never returns `Err` — a single segment or batch failure
    /// is logged and counted (empty `pairs`), never bubbled, so one bad unit
    /// can't abort the batch.
    async fn exec_unit(&self, unit: Unit) -> UnitDone {
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
                    .map(|k| k.as_deref().and_then(|kk| cache.as_ref().and_then(|c| c.get(kk))))
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

                let trs =
                    translate::translate_batch(client, endpoint, model, &cue_refs, &ctx_refs, target)
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

    /// Drain `units` through one shared concurrency pool with one progress bar
    /// and one Ctrl-C race, returning every completed [`UnitDone`] (in
    /// completion order — the caller partitions by `file` and sorts).
    ///
    /// `total_segments` is the post-`--limit` sum of [`Unit::attempted`] across
    /// all units, so the progress bar reaches 100%. Single failures are logged
    /// and counted inside [`exec_unit`], never bubbled. On Ctrl-C the run stops
    /// early and returns the partial result with [`RunOut::cancelled`] set;
    /// every completed translation is already on disk via the cache, so a re-run
    /// picks up where it left off.
    pub async fn run(&self, units: Vec<Unit>, total_segments: usize) -> RunOut {
        let pb = ProgressBar::new(total_segments as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        pb.set_message(format!(
            "translating {} segment(s) in {} unit(s)",
            total_segments,
            units.len()
        ));

        let mut stream = stream::iter(units)
            .map(|u| self.exec_unit(u))
            .buffer_unordered(self.concurrency);

        let mut done = Vec::new();
        let mut translated = 0usize;
        let mut failed = 0usize;
        let mut cancelled = false;

        // Race the drain loop against Ctrl-C. On signal we stop dispatching new
        // units, drop the stream (cancelling the few in-flight HTTP futures — a
        // dropped reqwest future closes its connection), and return partial.
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
                    // `attempted` went out; `pairs` came back. The gap
                    // (attempted - pairs.len()) is the failed/skipped count.
                    Some(out) => {
                        translated += out.pairs.len();
                        failed += out.attempted - out.pairs.len();
                        pb.inc(out.attempted as u64);
                        done.push(out);
                    }
                }
            }
        }
        drop(stream);
        pb.finish_and_clear();

        RunOut {
            done,
            translated,
            failed,
            cancelled,
        }
    }
}
