//! The queue layer — turns a list of input paths into translated outputs with
//! **bounded memory** and **one shared concurrency pool**.
//!
//! The shape is a backpressured producer/consumer:
//! - **Consumer**: a [`FuturesUnordered`] of [`Engine::exec_unit`] futures, kept
//!   at ≤ `concurrency` in flight (the pool). On Ctrl-C it stops.
//! - **Producer**: [`BatchState::next_unit`] opens the next input file *lazily*
//!   — only when the pool has a free slot and no open file still has pending
//!   units. A large file fills the pool by itself (only one such file open at a
//!   time); many small files open together but each is tiny. So peak memory is
//!   the IR of the files *in flight*, never the whole directory.
//! - **Writer**: the moment a file's last unit completes, it is written
//!   (`spawn_blocking`, so the drain keeps polling HTTP) and its parsed IR is
//!   dropped. A file that fails to open or write is logged and skipped; a Ctrl-C
//!   writes partial output for the files still open.
//!
//! `--limit` is a global segment budget shared across all files (a Batched
//! file's last batch is shrunk to fit).

use crate::engine::{Engine, Unit, UnitDone};
use crate::format::{Document, Format, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const BILINGUAL_OUTPUT_SUFFIX: &str = "bilingual";
pub const TRANSLATED_OUTPUT_SUFFIX: &str = "translated";

/// Knobs for a batch run, resolved from the CLI.
#[derive(Clone)]
pub struct BatchOpts {
    pub mode: OutputMode,
    pub in_place: bool,
    /// Single-file explicit output (`None` for directory mode / suffix / in-place).
    pub output: Option<PathBuf>,
    pub batch_size: usize,
    pub context: usize,
    /// Global segment cap across the whole batch (`None` = unlimited).
    pub limit: Option<usize>,
    /// Estimated prompt-size ceiling per batch request, in characters
    /// (CJK ≈ 1 char per token, so chars over-estimate latin text — the safe
    /// direction). Sized from the preset's context window so a batch that
    /// obviously cannot fit is split client-side, instead of paying a doomed
    /// request that 400s before `translate_split` recovers by halving.
    /// `0` disables the pre-split (count-only batching).
    pub prompt_char_budget: usize,
}

pub struct BatchSummary {
    pub ok_files: usize,
    pub failed_files: Vec<(PathBuf, String)>,
    pub translated: usize,
    pub failed: usize,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BatchProgress {
    pub total: usize,
    pub completed: usize,
    pub translated: usize,
    pub failed: usize,
}

pub type ProgressCallback = Arc<dyn Fn(BatchProgress) + Send + Sync>;

/// One opened file awaiting its progressive write. Held only while it has units
/// pending or in flight; dropped (written) the moment its last unit completes.
struct OpenFile {
    doc: Box<dyn Document + Send>,
    out_path: PathBuf,
    in_place: bool,
    input: PathBuf,
    pending: VecDeque<Unit>,
    pairs: Vec<(SegmentId, String)>,
    /// Units dispatched but not yet completed. The file is done when this hits 0
    /// and `pending` is empty.
    outstanding: usize,
}

/// Result of a spawned write, surfaced when the JoinSet is awaited.
struct WriteOutcome {
    input: PathBuf,
    out_path: PathBuf,
    err: Option<String>,
}

// ── public entry point ──────────────────────────────────────────────────────

/// Run a whole batch through one shared concurrency pool with lazy file opening
/// and progressive writing. See the module docs.
pub async fn run_batch(engine: &Engine, inputs: Vec<PathBuf>, opts: BatchOpts) -> BatchSummary {
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.set_message("translating");

    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let summary = run_batch_impl(engine, inputs, opts, cancel, Some(pb), None).await;
    signal_task.abort();
    summary
}

/// Run a batch under application-level cancellation and progress reporting.
/// This is used by the Web job runner; unlike [`run_batch`], it has no terminal
/// UI and never installs a process-wide Ctrl-C handler.
pub async fn run_batch_controlled(
    engine: &Engine,
    inputs: Vec<PathBuf>,
    opts: BatchOpts,
    cancel: CancellationToken,
    on_progress: ProgressCallback,
) -> BatchSummary {
    run_batch_impl(engine, inputs, opts, cancel, None, Some(on_progress)).await
}

async fn run_batch_impl(
    engine: &Engine,
    inputs: Vec<PathBuf>,
    opts: BatchOpts,
    cancel: CancellationToken,
    pb: Option<ProgressBar>,
    on_progress: Option<ProgressCallback>,
) -> BatchSummary {
    let concurrency = engine.concurrency().max(1);

    let mut state = BatchState {
        opts,
        inputs: inputs.into_iter(),
        open: HashMap::new(),
        next_file_idx: 0,
        write_tasks: JoinSet::new(),
        budget: None, // initialized from opts below
        pb,
        on_progress,
        progress: BatchProgress::default(),
        translated: 0,
        failed: 0,
        ok_files: 0,
        failed_files: Vec::new(),
        cancelled: false,
    };
    // Carry the global segment budget through each sequential lazy parse.
    state.budget = state.opts.limit;

    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
    let mut parsing = JoinSet::new();
    loop {
        if cancel.is_cancelled() {
            state.cancelled = true;
            break;
        }
        while in_flight.len() < concurrency {
            match state.next_unit() {
                Some(unit) => in_flight.push(engine.exec_unit(unit)),
                None => break,
            }
        }
        // One lazy parse per batch, at most two across all Web/CLI batches.
        // The HTTP drain remains polled while parsing or waiting for a permit.
        if in_flight.len() < concurrency && parsing.is_empty() {
            if let Some(input) = state.inputs.next() {
                let fidx = state.next_file_idx;
                state.next_file_idx += 1;
                let opts = state.opts.clone();
                let budget = state.budget;
                parsing.spawn(async move {
                    let path = input.clone();
                    let result = run_parser(parser_limiter(), move || {
                        prepare_file(opts, fidx, input, budget)
                    })
                    .await;
                    (fidx, path, result)
                });
            }
        }
        if in_flight.is_empty() && parsing.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                state.cancelled = true;
                break;
            }
            Some(result) = parsing.join_next(), if !parsing.is_empty() => {
                match result {
                    Ok((fidx, _, Ok(Ok((file, budget))))) => {
                        state.budget = budget;
                        state.accept_file(fidx, file);
                    }
                    Ok((_, path, result)) => {
                        let error = match result {
                            Ok(Err(error)) => format!("{error:#}"),
                            Err(error) => format!("parse task failed: {error}"),
                            Ok(Ok(_)) => unreachable!(),
                        };
                        state.failed_files.push((path, error));
                    }
                    Err(error) => state.failed_files.push((PathBuf::new(), format!("parse task failed: {error}"))),
                }
            }
            Some(done) = in_flight.next(), if !in_flight.is_empty() => state.on_done(done),
        }
    }
    // Drop HTTP futures before waiting for partial writes. A parser already
    // executing cannot be interrupted, but retains its global permit until it
    // finishes; cancelled jobs cannot accumulate unbounded blocking work.
    drop(in_flight);
    drop(parsing);

    state.finish().await
}

fn parser_limiter() -> Arc<Semaphore> {
    static LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMITER.get_or_init(|| Arc::new(Semaphore::new(2))).clone()
}

async fn run_parser<T: Send + 'static>(
    limiter: Arc<Semaphore>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let permit = limiter
        .acquire_owned()
        .await
        .expect("parser limiter remains open");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

fn prepare_file(
    opts: BatchOpts,
    fidx: usize,
    input: PathBuf,
    mut budget: Option<usize>,
) -> Result<(OpenFile, Option<usize>)> {
    let doc = crate::format::open(&input, None)?;
    let segments = doc.segments();
    eprintln!(
        "{}: {} block(s) [{}]",
        input.display(),
        segments.len(),
        doc.format_name()
    );
    let strategy = match doc.strategy() {
        Strategy::Independent => Strategy::Independent,
        Strategy::Batched { .. } => Strategy::Batched {
            batch_size: opts.batch_size,
            context: opts.context,
        },
    };
    let units = build_units(
        fidx,
        &segments,
        strategy,
        &mut budget,
        opts.prompt_char_budget,
    );
    Ok((
        OpenFile {
            doc,
            out_path: resolve_output(&input, opts.in_place, opts.output.as_deref(), opts.mode),
            in_place: opts.in_place,
            input,
            pending: units.into_iter().collect(),
            pairs: Vec::new(),
            outstanding: 0,
        },
        budget,
    ))
}

// ── queue state machine ─────────────────────────────────────────────────────

struct BatchState {
    opts: BatchOpts,
    inputs: std::vec::IntoIter<PathBuf>,
    open: HashMap<usize, OpenFile>,
    next_file_idx: usize,
    write_tasks: JoinSet<WriteOutcome>,
    budget: Option<usize>,
    pb: Option<ProgressBar>,
    on_progress: Option<ProgressCallback>,
    progress: BatchProgress,
    translated: usize,
    failed: usize,
    ok_files: usize,
    failed_files: Vec<(PathBuf, String)>,
    cancelled: bool,
}

impl BatchState {
    fn next_unit(&mut self) -> Option<Unit> {
        for file in self.open.values_mut() {
            if let Some(unit) = file.pending.pop_front() {
                file.outstanding += 1;
                return Some(unit);
            }
        }
        None
    }

    fn accept_file(&mut self, fidx: usize, file: OpenFile) {
        let added = file.pending.iter().map(Unit::attempted).sum::<usize>();
        self.progress.total += added;
        if let Some(pb) = &self.pb {
            pb.inc_length(added as u64);
        }
        self.report_progress();
        if file.pending.is_empty() {
            self.spawn_write(file);
        } else {
            self.open.insert(fidx, file);
        }
    }

    /// A unit finished: route its pairs, advance the bar, and write the file if
    /// it just completed.
    fn on_done(&mut self, done: UnitDone) {
        if let Some(pb) = &self.pb {
            pb.inc(done.attempted as u64);
        }
        self.translated += done.pairs.len();
        self.failed += done.attempted - done.pairs.len();
        self.progress.completed += done.attempted;
        self.progress.translated = self.translated;
        self.progress.failed = self.failed;
        self.report_progress();
        let fidx = done.file;
        let Some(of) = self.open.get_mut(&fidx) else {
            return;
        };
        of.pairs.extend(done.pairs);
        of.outstanding -= 1;
        if of.outstanding == 0 && of.pending.is_empty() {
            let mut of = self.open.remove(&fidx).unwrap();
            of.pairs.sort_by_key(|(id, _)| *id);
            self.spawn_write(of);
        }
    }

    /// Write a completed file on a blocking thread (the drain keeps polling).
    /// The file's IR is dropped when the blocking task returns.
    fn spawn_write(&mut self, of: OpenFile) {
        let input = of.input.clone();
        let out_path = of.out_path.clone();
        let mode = self.opts.mode;
        self.write_tasks.spawn(async move {
            let res = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut of = of; // mutable so doc.write (&mut self) is callable
                let target = if of.in_place {
                    inplace_temp(&of.out_path)
                } else {
                    of.out_path.clone()
                };
                of.doc
                    .write(&of.pairs, &target, mode)
                    .with_context(|| format!("write {}", target.display()))?;
                if of.in_place {
                    std::fs::rename(&target, &of.out_path)
                        .with_context(|| format!("rename into place {}", of.out_path.display()))?;
                }
                Ok(())
            })
            .await;
            let err = match res {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(join_err) => Some(format!("write task failed: {join_err}")),
            };
            WriteOutcome {
                input,
                out_path,
                err,
            }
        });
    }

    /// After the drain: write partials for any file still open (Ctrl-C case),
    /// then await every spawned write and tally the results.
    async fn finish(mut self) -> BatchSummary {
        // Collect first so the drain's borrow of self.open ends before we call
        // self.spawn_write (which takes &mut self) inside the loop.
        let drained: Vec<OpenFile> = self.open.drain().map(|(_, of)| of).collect();
        for mut of in drained {
            of.pairs.sort_by_key(|(id, _)| *id);
            self.spawn_write(of);
        }
        while let Some(outcome) = self.write_tasks.join_next().await {
            match outcome {
                Ok(wo) => match wo.err {
                    None => {
                        eprintln!("wrote: {}", wo.out_path.display());
                        self.ok_files += 1;
                    }
                    Some(msg) => {
                        eprintln!("error: write {}: {}", wo.out_path.display(), msg);
                        self.failed_files.push((wo.input, msg));
                    }
                },
                Err(join_err) => {
                    eprintln!("error: write task join failed: {join_err}");
                }
            }
        }
        if let Some(pb) = &self.pb {
            pb.finish_and_clear();
        }
        self.report_progress();
        BatchSummary {
            ok_files: self.ok_files,
            failed_files: self.failed_files,
            translated: self.translated,
            failed: self.failed,
            cancelled: self.cancelled,
        }
    }

    fn report_progress(&self) {
        if let Some(callback) = &self.on_progress {
            callback(self.progress.clone());
        }
    }
}

// ── unit building ───────────────────────────────────────────────────────────

/// Turn a file's segments into self-contained [`Unit`]s for the shared pool.
///
/// `Independent` → one [`Unit::Single`] per segment; `Batched` → contiguous
/// batches of `batch_size` cues, each carrying `context` read-only preceding
/// cues (same file, in order). `budget` is the global `--limit`: each emitted
/// segment decrements it, and the last batch of a Batched file is shrunk to fit.
/// When the budget hits zero the file stops emitting.
fn build_units(
    file: usize,
    segments: &[Segment],
    strategy: Strategy,
    budget: &mut Option<usize>,
    prompt_char_budget: usize,
) -> Vec<Unit> {
    /// How many of `want` segments the budget still allows, decrementing it.
    fn allow(budget: &mut Option<usize>, want: usize) -> usize {
        match budget {
            Some(b) => {
                let c = want.min(*b);
                *b -= c;
                c
            }
            None => want,
        }
    }

    let mut units = Vec::new();
    match strategy {
        Strategy::Independent => {
            for seg in segments {
                if allow(budget, 1) == 0 {
                    break;
                }
                units.push(Unit::Single {
                    file,
                    id: seg.id,
                    text: seg.text.clone(),
                });
            }
        }
        Strategy::Batched {
            batch_size,
            context,
        } => {
            let batch_size = batch_size.max(1); // guard against a nonsensical 0.
            let mut i = 0;
            while i < segments.len() {
                let want = batch_size.min(segments.len() - i);
                let n = allow(budget, want);
                if n == 0 {
                    break;
                }
                let end = i + n;
                let ctx_start = i.saturating_sub(context);
                // Split the count-sized batch further by estimated prompt size
                // so a batch that clearly cannot fit the model's context
                // window never leaves the client (see `BatchOpts::prompt_char_budget`).
                // A single cue longer than the whole budget still goes out
                // alone — the translate layer's overflow recovery owns it.
                if prompt_char_budget > 0 {
                    let ctx_chars: usize =
                        segments[ctx_start..i].iter().map(|s| s.text.len()).sum();
                    let mut chars = BATCH_PROMPT_OVERHEAD_CHARS + ctx_chars;
                    let mut sub_end = i;
                    while sub_end < end {
                        chars += segments[sub_end].text.len() + 16; // the <cN></cN> wrapper
                        if sub_end > i && chars > prompt_char_budget {
                            break;
                        }
                        sub_end += 1;
                    }
                    units.push(Unit::Batch {
                        file,
                        ids: segments[i..sub_end].iter().map(|s| s.id).collect(),
                        cues: segments[i..sub_end]
                            .iter()
                            .map(|s| s.text.clone())
                            .collect(),
                        context: segments[ctx_start..i]
                            .iter()
                            .map(|s| s.text.clone())
                            .collect(),
                    });
                    i = sub_end;
                } else {
                    units.push(Unit::Batch {
                        file,
                        ids: segments[i..end].iter().map(|s| s.id).collect(),
                        cues: segments[i..end].iter().map(|s| s.text.clone()).collect(),
                        context: segments[ctx_start..i]
                            .iter()
                            .map(|s| s.text.clone())
                            .collect(),
                    });
                    i = end;
                }
            }
        }
    }
    units
}

/// Rough size of the batch prompt template + per-batch slack, subtracted up
/// front when estimating whether a batch fits the context window. A little
/// generous on purpose — underestimating the template is what 400s requests.
const BATCH_PROMPT_OVERHEAD_CHARS: usize = 512;

// ── input discovery + output paths ──────────────────────────────────────────

/// Recursively collect every supported, non-output file under `root`, sorted
/// for deterministic ordering. Symlinks are skipped (avoids cycles).
pub fn collect_inputs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    visit(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn visit(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))?
        {
            visit(&entry?.path(), out)?;
        }
    } else if meta.is_file() {
        // Keep the filter in sync with supported formats and skip both output
        // modes so a directory rerun does not translate generated files again.
        if Format::from_path(path).is_ok() && !is_generated_output(path) {
            out.push(path.to_path_buf());
        }
    }
    Ok(())
}

/// Whether `path` is a bilingual or replace-mode output generated by Ferryman.
pub fn is_generated_output(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| stem.rsplit_once('.'))
        .is_some_and(|(_, last)| matches!(last, BILINGUAL_OUTPUT_SUFFIX | TRANSLATED_OUTPUT_SUFFIX))
}

/// Add the mode-specific output suffix while preserving the extension.
pub fn suffixed_output_path(path: &Path, mode: OutputMode) -> PathBuf {
    let suffix = match mode {
        OutputMode::Bilingual => BILINGUAL_OUTPUT_SUFFIX,
        OutputMode::Replace => TRANSLATED_OUTPUT_SUFFIX,
    };
    let mut name = path
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(suffix);
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    path.with_file_name(name)
}

/// Hidden sibling temp file used for atomic in-place writes.
fn inplace_temp(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".ferryman-tmp");
    target.with_file_name(name)
}

/// Resolve a file's output path: explicit `--output`, in-place, or a suffixed
/// sibling next to the source.
fn resolve_output(
    input: &Path,
    in_place: bool,
    explicit: Option<&Path>,
    mode: OutputMode,
) -> PathBuf {
    if in_place {
        input.to_path_buf()
    } else if let Some(o) = explicit {
        o.to_path_buf()
    } else {
        suffixed_output_path(input, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parser_does_not_block_runtime_and_keeps_permit_after_cancellation() {
        let limiter = Arc::new(Semaphore::new(1));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task_limiter = limiter.clone();
        let task = tokio::spawn(run_parser(task_limiter, move || {
            started.send(()).unwrap();
            released
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), ready)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let _ = task.await;
        // Aborting the async owner must not release a running parser's slot.
        assert!(limiter.clone().try_acquire_owned().is_err());
        release.send(()).unwrap();
        let permit = tokio::time::timeout(std::time::Duration::from_secs(1), limiter.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn lazy_parser_writes_passthrough_and_reports_bad_inputs() {
        let root = std::env::temp_dir().join(format!("ferryman-parse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("empty.txt");
        std::fs::write(&input, "").unwrap();
        let engine = Engine::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".into(),
            "model".into(),
            "zh".into(),
            2,
            None,
        );
        let opts = BatchOpts {
            mode: OutputMode::Bilingual,
            in_place: false,
            output: None,
            batch_size: 2,
            context: 0,
            limit: None,
            prompt_char_budget: 0,
        };
        let summary = run_batch_impl(
            &engine,
            vec![root.join("missing.txt"), input.clone()],
            opts,
            CancellationToken::new(),
            None,
            None,
        )
        .await;
        assert_eq!(summary.failed_files.len(), 1);
        assert_eq!(summary.ok_files, 1);
        assert_eq!(
            std::fs::read(suffixed_output_path(&input, OutputMode::Bilingual)).unwrap(),
            b""
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn lazy_parsing_translates_multiple_files_and_honors_cancellation() {
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|| async {
            axum::Json(serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": "<c1>translated</c1>"}}]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = std::env::temp_dir().join(format!("ferryman-batch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let inputs: Vec<_> = ["one.txt", "two.txt"].map(|name| root.join(name)).into();
        for input in &inputs {
            std::fs::write(input, "source text").unwrap();
        }
        let opts = BatchOpts {
            mode: OutputMode::Bilingual,
            in_place: false,
            output: None,
            batch_size: 1,
            context: 0,
            limit: None,
            prompt_char_budget: 0,
        };
        let engine = Engine::new(
            reqwest::Client::new(),
            endpoint,
            "model".into(),
            "en".into(),
            2,
            None,
        );
        let summary = run_batch_impl(
            &engine,
            inputs.clone(),
            opts.clone(),
            CancellationToken::new(),
            None,
            None,
        )
        .await;
        assert_eq!(
            (summary.ok_files, summary.translated, summary.failed),
            (2, 2, 0)
        );
        assert!(summary.failed_files.is_empty());
        for input in &inputs {
            assert!(
                std::fs::read_to_string(suffixed_output_path(input, OutputMode::Bilingual))
                    .unwrap()
                    .contains("translated")
            );
        }
        // With both parser slots occupied, cancellation must finish without
        // waiting for the semaphore or touching an input file.
        let permits = parser_limiter().acquire_many_owned(2).await.unwrap();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            stop.cancel();
        });
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run_batch_impl(&engine, inputs, opts, cancel, None, None),
        )
        .await
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.ok_files, 0);
        cancel_task.await.unwrap();
        drop(permits);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn seg(id: usize, text: &str) -> Segment {
        Segment {
            id,
            text: text.to_string(),
        }
    }

    #[test]
    fn independent_emits_one_single_per_segment() {
        let segs = vec![seg(0, "a"), seg(1, "b"), seg(2, "c")];
        let units = build_units(7, &segs, Strategy::Independent, &mut None, 0);
        assert_eq!(units.len(), 3);
        assert!(units
            .iter()
            .all(|u| matches!(u, Unit::Single { file: 7, .. })));
    }

    #[test]
    fn batched_slices_into_batches_with_context() {
        // batch_size 2, context 1, 5 segments -> batches of 2,2,1.
        let segs = vec![
            seg(0, "a"),
            seg(1, "b"),
            seg(2, "c"),
            seg(3, "d"),
            seg(4, "e"),
        ];
        let units = build_units(
            0,
            &segs,
            Strategy::Batched {
                batch_size: 2,
                context: 1,
            },
            &mut None,
            0,
        );
        assert_eq!(units.len(), 3);
        // batch 0 (starts at i=0): cues a,b; context = segs[0..0] = none.
        match &units[0] {
            Unit::Batch {
                ids, cues, context, ..
            } => {
                assert_eq!(*ids, vec![0, 1]);
                assert_eq!(*cues, vec!["a".to_string(), "b".to_string()]);
                assert!(context.is_empty());
            }
            _ => panic!("expected Batch"),
        }
        // batch 1 (starts at i=2): cues c,d; context = segs[1..2] = [b].
        match &units[1] {
            Unit::Batch { cues, context, .. } => {
                assert_eq!(*cues, vec!["c".to_string(), "d".to_string()]);
                assert_eq!(*context, vec!["b".to_string()]);
            }
            _ => panic!("expected Batch"),
        }
        // batch 2 (starts at i=4): shrunk to cue e; context = segs[3..4] = [d].
        match &units[2] {
            Unit::Batch { cues, context, .. } => {
                assert_eq!(*cues, vec!["e".to_string()]);
                assert_eq!(*context, vec!["d".to_string()]);
            }
            _ => panic!("expected Batch"),
        }
    }

    #[test]
    fn limit_shrinks_last_batch_to_fit() {
        let segs: Vec<_> = (0..10).map(|i| seg(i, "x")).collect();
        let units = build_units(
            0,
            &segs,
            Strategy::Batched {
                batch_size: 25,
                context: 5,
            },
            &mut Some(3),
            0,
        );
        assert_eq!(units.len(), 1);
        match &units[0] {
            Unit::Batch { cues, .. } => assert_eq!(cues.len(), 3),
            _ => panic!("expected Batch"),
        }
    }

    #[test]
    fn limit_caps_total_across_independent() {
        let segs: Vec<_> = (0..5).map(|i| seg(i, "x")).collect();
        let units = build_units(0, &segs, Strategy::Independent, &mut Some(2), 0);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn prompt_budget_splits_batches_client_side() {
        // 6 cues × 500 chars: with a budget admitting ~2 cues per request, one
        // count-sized batch of 6 becomes three batches of 2 — no doomed 400
        // probing needed before translate_split would halve its way down.
        let segs: Vec<_> = (0..6).map(|i| seg(i, &"x".repeat(500))).collect();
        let units = build_units(
            0,
            &segs,
            Strategy::Batched {
                batch_size: 6,
                context: 0,
            },
            &mut None,
            512 + 16 + 2 * 516, // template + two cues
        );
        let sizes: Vec<usize> = units
            .iter()
            .map(|unit| match unit {
                Unit::Batch { cues, .. } => cues.len(),
                Unit::Single { .. } => 0,
            })
            .collect();
        assert_eq!(sizes, vec![2, 2, 2]);
        // IDs stay in order across the split batches.
        let ids: Vec<SegmentId> = units
            .iter()
            .flat_map(|unit| match unit {
                Unit::Batch { ids, .. } => ids.clone(),
                Unit::Single { .. } => vec![],
            })
            .collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn oversize_single_cue_still_emits_alone_under_budget() {
        // A cue longer than the whole budget goes out as its own batch rather
        // than being dropped — the translate layer's overflow recovery owns it.
        let segs = vec![seg(0, &"x".repeat(5_000)), seg(1, "short")];
        let units = build_units(
            0,
            &segs,
            Strategy::Batched {
                batch_size: 2,
                context: 0,
            },
            &mut None,
            1_000,
        );
        let sizes: Vec<usize> = units
            .iter()
            .map(|unit| match unit {
                Unit::Batch { cues, .. } => cues.len(),
                Unit::Single { .. } => 0,
            })
            .collect();
        assert_eq!(sizes, vec![1, 1]);
    }

    #[test]
    fn empty_segments_emit_nothing() {
        let units = build_units(
            0,
            &[],
            Strategy::Batched {
                batch_size: 25,
                context: 5,
            },
            &mut None,
            0,
        );
        assert!(units.is_empty());
        let units = build_units(0, &[], Strategy::Independent, &mut None, 0);
        assert!(units.is_empty());
    }

    #[test]
    fn budget_shared_across_files() {
        // Two files sharing one global budget (mirrors the lazy open loop):
        // file 0 consumes all 3, file 1 gets nothing.
        let segs: Vec<_> = (0..5).map(|i| seg(i, "x")).collect();
        let mut budget = Some(3);
        let u1 = build_units(0, &segs, Strategy::Independent, &mut budget, 0);
        let u2 = build_units(1, &segs, Strategy::Independent, &mut budget, 0);
        assert_eq!(u1.len(), 3);
        assert_eq!(u2.len(), 0);
        assert_eq!(budget, Some(0));
    }

    #[test]
    fn generated_outputs_detect_both_modes() {
        assert!(is_generated_output(Path::new("book.bilingual.epub")));
        assert!(is_generated_output(Path::new("/x/y/a.translated.txt")));
        assert!(!is_generated_output(Path::new("book.epub")));
        assert!(!is_generated_output(Path::new("bilingual.md")));
    }

    #[test]
    fn output_suffix_tracks_mode() {
        assert_eq!(
            suffixed_output_path(Path::new("book.epub"), OutputMode::Bilingual),
            PathBuf::from("book.bilingual.epub")
        );
        assert_eq!(
            suffixed_output_path(Path::new("book.epub"), OutputMode::Replace),
            PathBuf::from("book.translated.epub")
        );
    }
}
