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
use tokio::task::JoinSet;

/// Suffix inserted before the extension when no explicit output path is given
/// (and not `--in-place`): `book.epub` -> `book.bilingual.epub`. Also the marker
/// the directory walk skips so re-running doesn't retranslate its own output.
pub(crate) const OUTPUT_SUFFIX: &str = "bilingual";

/// Knobs for a batch run, resolved from the CLI.
pub struct BatchOpts {
    pub mode: OutputMode,
    pub in_place: bool,
    /// Single-file explicit output (`None` for directory mode / suffix / in-place).
    pub output: Option<PathBuf>,
    pub batch_size: usize,
    pub context: usize,
    /// Global segment cap across the whole batch (`None` = unlimited).
    pub limit: Option<usize>,
}

pub struct BatchSummary {
    pub ok_files: usize,
    pub failed_files: Vec<(PathBuf, String)>,
    pub translated: usize,
    pub failed: usize,
    pub cancelled: bool,
}

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
    let concurrency = engine.concurrency();
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.set_message("translating");

    let mut state = BatchState {
        opts,
        inputs: inputs.into_iter(),
        open: HashMap::new(),
        next_file_idx: 0,
        write_tasks: JoinSet::new(),
        budget: None, // set below (borrowed mutably across open_file calls)
        pb,
        translated: 0,
        failed: 0,
        ok_files: 0,
        failed_files: Vec::new(),
        cancelled: false,
    };
    // `budget` can't be set in the struct literal because open_file borrows
    // &mut self (which includes budget) — initialize here.
    state.budget = state.opts.limit;

    let cancel = tokio::signal::ctrl_c();
    tokio::pin!(cancel);
    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();

    loop {
        // Keep the pool full: dispatch units (opening files lazily) until the
        // pool is at capacity or we run out of work.
        while in_flight.len() < concurrency {
            match state.next_unit() {
                Some(unit) => in_flight.push(engine.exec_unit(unit)),
                None => break,
            }
        }
        if in_flight.is_empty() {
            break; // nothing pending, nothing in flight → done
        }
        tokio::select! {
            // Poll cancel first so Ctrl-C is observed promptly.
            biased;
            _ = &mut cancel => {
                state.cancelled = true;
                break;
            }
            Some(done) = in_flight.next() => state.on_done(done),
        }
    }
    // `in_flight` drops here: any still-running HTTP futures are cancelled (a
    // dropped reqwest future closes its connection), exactly like the old drain.

    state.finish().await
}

// ── queue state machine ─────────────────────────────────────────────────────

struct BatchState {
    opts: BatchOpts,
    inputs: std::vec::IntoIter<PathBuf>,
    open: HashMap<usize, OpenFile>,
    next_file_idx: usize,
    write_tasks: JoinSet<WriteOutcome>,
    budget: Option<usize>,
    pb: ProgressBar,
    translated: usize,
    failed: usize,
    ok_files: usize,
    failed_files: Vec<(PathBuf, String)>,
    cancelled: bool,
}

impl BatchState {
    /// The next unit to dispatch, opening a new file lazily when no open file
    /// has pending units. Returns `None` once all inputs are exhausted.
    fn next_unit(&mut self) -> Option<Unit> {
        loop {
            // 1. Pop a pending unit from any open file (keeps large files
            //    draining before we open anything new).
            for of in self.open.values_mut() {
                if let Some(u) = of.pending.pop_front() {
                    of.outstanding += 1;
                    return Some(u);
                }
            }
            // 2. No pending units anywhere → open the next input file.
            let input = self.inputs.next()?;
            let fidx = self.next_file_idx;
            self.next_file_idx += 1;
            match self.open_file(fidx, input) {
                Ok(mut of) => {
                    // The bar's total grows as we learn each file's segment count.
                    self.pb
                        .inc_length(of.pending.iter().map(Unit::attempted).sum::<usize>() as u64);
                    if of.pending.is_empty() {
                        // Zero translatable segments: write the passthrough now,
                        // don't track it as an open file.
                        self.spawn_write(of);
                        continue;
                    }
                    let first = of.pending.pop_front();
                    of.outstanding = 1; // first unit dispatched below
                    self.open.insert(fidx, of);
                    return first;
                }
                Err((input, msg)) => {
                    eprintln!("error: open {}: {} — skipping", input.display(), msg);
                    self.failed_files.push((input, msg));
                    continue;
                }
            }
        }
    }

    /// Parse one file and build its units (respecting the global `--limit`).
    fn open_file(&mut self, fidx: usize, input: PathBuf) -> Result<OpenFile, (PathBuf, String)> {
        let doc = match crate::format::open(&input, None) {
            Ok(d) => d,
            Err(e) => return Err((input, format!("{e:#}"))),
        };
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
                batch_size: self.opts.batch_size,
                context: self.opts.context,
            },
        };
        let units = build_units(fidx, &segments, strategy, &mut self.budget);
        Ok(OpenFile {
            doc,
            out_path: resolve_output(&input, self.opts.in_place, self.opts.output.as_deref()),
            in_place: self.opts.in_place,
            input,
            pending: units.into_iter().collect(),
            pairs: Vec::new(),
            outstanding: 0,
        })
    }

    /// A unit finished: route its pairs, advance the bar, and write the file if
    /// it just completed.
    fn on_done(&mut self, done: UnitDone) {
        self.pb.inc(done.attempted as u64);
        self.translated += done.pairs.len();
        self.failed += done.attempted - done.pairs.len();
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
        self.pb.finish_and_clear();
        BatchSummary {
            ok_files: self.ok_files,
            failed_files: self.failed_files,
            translated: self.translated,
            failed: self.failed,
            cancelled: self.cancelled,
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
        Strategy::Batched { batch_size, context } => {
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
    units
}

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
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("read dir {}", path.display()))?
        {
            visit(&entry?.path(), out)?;
        }
    } else if meta.is_file() {
        // Keep the filter in sync with supported formats via from_path, and
        // skip our own suffixed outputs so re-running a dir doesn't retranslate
        // them (book.bilingual.epub → stem's last dot-segment == "bilingual").
        if Format::from_path(path).is_ok() && !is_suffix_output(path) {
            out.push(path.to_path_buf());
        }
    }
    Ok(())
}

/// Is `path` one of our suffixed outputs? The last dot-segment of the file stem
/// equals [`OUTPUT_SUFFIX`] (`book.bilingual` → `bilingual`). A stem with no dot
/// (e.g. `bilingual.md`) is not a match — our outputs always have a prefix.
pub(crate) fn is_suffix_output(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| stem.rsplit_once('.'))
        .is_some_and(|(_, last)| last == OUTPUT_SUFFIX)
}

/// `book.epub` → `book.bilingual.epub` (same directory). Used when neither
/// `--output` nor `--in-place` is given.
fn suffix_path(path: &Path) -> PathBuf {
    let mut name = path.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(".");
    name.push(OUTPUT_SUFFIX);
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
fn resolve_output(input: &Path, in_place: bool, explicit: Option<&Path>) -> PathBuf {
    if in_place {
        input.to_path_buf()
    } else if let Some(o) = explicit {
        o.to_path_buf()
    } else {
        suffix_path(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: usize, text: &str) -> Segment {
        Segment {
            id,
            text: text.to_string(),
        }
    }

    #[test]
    fn independent_emits_one_single_per_segment() {
        let segs = vec![seg(0, "a"), seg(1, "b"), seg(2, "c")];
        let units = build_units(7, &segs, Strategy::Independent, &mut None);
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
        );
        assert_eq!(units.len(), 3);
        // batch 0 (starts at i=0): cues a,b; context = segs[0..0] = none.
        match &units[0] {
            Unit::Batch { ids, cues, context, .. } => {
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
        let units = build_units(0, &segs, Strategy::Independent, &mut Some(2));
        assert_eq!(units.len(), 2);
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
        );
        assert!(units.is_empty());
        let units = build_units(0, &[], Strategy::Independent, &mut None);
        assert!(units.is_empty());
    }

    #[test]
    fn budget_shared_across_files() {
        // Two files sharing one global budget (mirrors the lazy open loop):
        // file 0 consumes all 3, file 1 gets nothing.
        let segs: Vec<_> = (0..5).map(|i| seg(i, "x")).collect();
        let mut budget = Some(3);
        let u1 = build_units(0, &segs, Strategy::Independent, &mut budget);
        let u2 = build_units(1, &segs, Strategy::Independent, &mut budget);
        assert_eq!(u1.len(), 3);
        assert_eq!(u2.len(), 0);
        assert_eq!(budget, Some(0));
    }

    #[test]
    fn is_suffix_output_detects_bilingual_stem() {
        assert!(is_suffix_output(Path::new("book.bilingual.epub")));
        assert!(is_suffix_output(Path::new("/x/y/a.bilingual.txt")));
        assert!(!is_suffix_output(Path::new("book.epub")));
        assert!(!is_suffix_output(Path::new("bilingual.md"))); // stem has no dot
    }
}
