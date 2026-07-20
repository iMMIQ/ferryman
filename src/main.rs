//! ferryman — translate a document into bilingual (original + translation)
//! output via a vLLM-served model. EPUB, SRT, VTT, ASS, LRC, TXT and MD ship
//! today; docx is planned — plug a new format into `src/format/` and it just works.
//!
//! The original formatting is preserved byte-for-byte (via lol_html for EPUB;
//! cue timing/structure is preserved verbatim for subtitles); after each
//! translated block a styled sibling carrying the translation is inserted.

mod archive;
mod cache;
mod container;
mod engine;
mod format;
mod html;
mod translate;

use crate::cache::Cache;
use crate::engine::{Engine, Unit};
use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ferryman",
    about = "Translate a document into a bilingual side-by-side output via vLLM (EPUB, SRT, VTT, ASS, LRC, TXT, MD)"
)]
struct Cli {
    /// Input file or directory. A file is translated directly (format
    /// auto-detected from the extension). A directory is walked recursively and
    /// every supported file (epub, srt, vtt, ass, lrc, txt, md) is translated;
    /// unsupported files and ferryman's own suffixed outputs are skipped.
    #[arg(long, short = 'i')]
    input: PathBuf,

    /// Output path (single-file mode only; rejected with a directory input).
    /// If neither --output nor --in-place is given, each file is written to a
    /// sibling named `<name>.bilingual.<ext>` next to the original.
    #[arg(long, short = 'o', conflicts_with = "in_place")]
    output: Option<PathBuf>,

    /// Overwrite each input file in place (single file or directory). Mutually
    /// exclusive with --output. Each file is written to a sibling temp file
    /// first, then atomically renamed over the original, so a crash mid-write
    /// can't truncate the source.
    #[arg(long)]
    in_place: bool,

    /// Output mode: `bilingual` (default) keeps the original and appends the
    /// translation; `replace` writes only the translation.
    #[arg(long, value_enum, default_value_t = OutputMode::Bilingual)]
    mode: OutputMode,

    /// vLLM OpenAI-compatible endpoint (used when --serve is NOT set).
    #[arg(long, default_value = "http://localhost:8001")]
    endpoint: String,

    /// Target language (full name, e.g. 中文 / English / 日本語).
    #[arg(long, default_value = "中文")]
    target: String,

    /// Optional cap on total translated blocks (for quick testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Subtitle cues per translation request (subtitle inputs only). Batching
    /// keeps cross-cue context and orders the result strictly one-to-one; the
    /// model returns one translation per cue, no merge/split. (default: 25)
    #[arg(long, default_value_t = 25)]
    subtitle_batch_size: usize,

    /// Number of preceding cues sent as read-only context with each subtitle
    /// batch (not translated, not emitted) — keeps the translation fluent
    /// across cue boundaries. (default: 5)
    #[arg(long, default_value_t = 5)]
    subtitle_context: usize,

    /// Disable the on-disk translation cache (retranslate everything). By
    /// default completed translations are cached so re-runs skip them and a
    /// Ctrl-C'd run keeps what finished.
    #[arg(long)]
    no_cache: bool,

    /// Cache directory (default: $XDG_CACHE_HOME/ferryman or ~/.cache/ferryman).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 180)]
    timeout: u64,

    /// Model preset: bundles the model path + the optimal vLLM serve config for
    /// this Jetson. `7b-fp8` = Hy-MT2-7B-FP8 (light/fast, the original default);
    /// `30b-fp8` = Hy-MT2-30B-A3B-FP8 (higher quality; CUDA graphs on,
    /// max-num-seqs 512, util 0.55 — measured ~1222 tok/s peak). Every flag below
    /// overrides the preset, so you can still tweak any single knob.
    #[arg(long, value_enum, default_value_t = Preset::SevenBFp8)]
    preset: Preset,

    /// Served model id (OpenAI `model` field) when NOT --serve. Defaults to the
    /// preset's model (same id the container serves).
    #[arg(long)]
    model: Option<String>,

    // --- container management (self-hosted model deployment) ---
    /// Launch & manage the vLLM container ourselves (shut it down afterwards).
    #[arg(long)]
    serve: bool,

    /// Docker image to run when --serve.
    #[arg(long, default_value = "docker.io/catdogai/lzc-aipod-vllm:agxorin-cu126-src-18f658bb3185-20260703")]
    image: String,

    /// Host directory holding the model files (mounted into the container).
    /// Defaults to the preset's model dir.
    #[arg(long)]
    host_model_dir: Option<String>,

    /// Host directory persisted as the container's JIT/compile cache. The cu126
    /// image's v1 engine JIT-compiles FlashInfer/Triton kernels on first launch
    /// (~2.5-5 min); persisting them reuses the compiled kernels on later launches.
    /// Defaults to `$MODEL_ROOT/vllm-cache` (same root as the presets).
    #[arg(long)]
    host_cache_dir: Option<String>,

    /// Model path inside the container; also the served model id.
    /// Defaults to the preset's model.
    #[arg(long)]
    serve_model: Option<String>,

    /// Container name (removed on exit).
    #[arg(long, default_value = "ferryman-vllm")]
    container_name: String,

    /// Host port to map to the container's 8000.
    #[arg(long, default_value_t = 8001)]
    host_port: u16,

    /// Quantization method, e.g. `awq_marlin`. Omit to let vLLM auto-detect (FP8).
    #[arg(long)]
    quantization: Option<String>,

    /// Compute dtype (default: float16 for 7b, auto→bf16 for 30b).
    #[arg(long)]
    vllm_dtype: Option<String>,

    /// KV cache dtype. `fp8` halves KV-cache memory and boosts decode throughput;
    /// `auto` uses the model's native dtype. (default: fp8)
    #[arg(long)]
    kv_cache_dtype: Option<String>,

    /// gpu-memory-utilization (default: 0.30 for 7b, 0.55 for 30b).
    #[arg(long)]
    gpu_memory_utilization: Option<f32>,

    /// max-model-len (default: 8192 for 7b, 4096 for 30b).
    #[arg(long)]
    max_model_len: Option<u32>,

    /// max-num-seqs, vLLM's admission cap (default: 512 for both presets).
    #[arg(long)]
    max_num_seqs: Option<u32>,

    /// Force eager mode (disable torch.compile + CUDA graphs). Both presets
    /// leave this off — graphs are faster on this Jetson. Set only to A/B test
    /// eager. (README documents this as "omit --enforce-eager" for graphs-on.)
    #[arg(long)]
    enforce_eager: bool,

    /// Max concurrent translation requests (default: 256 for 7b, 128 for 30b).
    #[arg(long)]
    concurrency: Option<usize>,

    /// Seconds to wait for the container to become healthy (cold start ~2.5-5 min).
    #[arg(long, default_value_t = 600)]
    health_timeout: u64,
}

/// Bundled model + optimal serve config per preset.
#[derive(Clone, Copy, clap::ValueEnum, PartialEq, Debug)]
pub enum Preset {
    /// Hy-MT2-7B-FP8 — light & fast, the original default.
    #[value(name = "7b-fp8")]
    SevenBFp8,
    /// Hy-MT2-30B-A3B-FP8 — higher quality; optimal serve config measured on
    /// this Jetson (CUDA graphs on, max-num-seqs 512 → ~1222 tok/s peak).
    #[value(name = "30b-fp8")]
    ThirtyBFp8,
}

struct PresetCfg {
    host_model_dir: String,
    serve_model: &'static str,
    dtype: &'static str,
    kv_cache_dtype: &'static str,
    gpu_memory_utilization: f32,
    max_model_len: u32,
    max_num_seqs: Option<u32>,
    /// false = CUDA graphs ON (omit --enforce-eager).
    enforce_eager: bool,
    concurrency: usize,
}

impl Preset {
    fn cfg(self) -> PresetCfg {
        match self {
            Preset::SevenBFp8 => PresetCfg {
                host_model_dir: format!("{}/Hy-MT2-7B-FP8", model_root()),
                serve_model: "/models/Hy-MT2-7B-FP8",
                dtype: "float16",
                kv_cache_dtype: "fp8",
                // 7B weights are only ~7 GiB; 0.30 already yields ~258k fp8 KV
                // tokens, far past what compute can keep busy.
                gpu_memory_utilization: 0.30,
                max_model_len: 8192,
                // 7B saturates on COMPUTE ~c256-512; 512 lets short blocks reach
                // the ceiling (default 256 leaves throughput on the table).
                max_num_seqs: Some(512),
                // CUDA graphs ON: measured +8% throughput ceiling (868→938 tok/s)
                // and +15% at low concurrency. Smaller win than the 30B (7B is
                // dense → fewer kernels/step → less CPU launch overhead to remove)
                // but effectively free: capture is 1.7 GiB and the 7B footprint is
                // tiny. (The old "graphs hurt 2x" note was AWQ-specific, not FP8.)
                enforce_eager: false,
                // Near the compute saturation point: ~895 tok/s @ ~11s/block.
                // (Old default 96 topped out at ~637 tok/s.)
                concurrency: 256,
            },
            Preset::ThirtyBFp8 => PresetCfg {
                host_model_dir: format!("{}/Hy-MT2-30B-A3B-FP8", model_root()),
                serve_model: "/models/Hy-MT2-30B-A3B-FP8",
                dtype: "auto",
                kv_cache_dtype: "fp8",
                // Reliable minimum: weights are 28.6G = 47% of 61G, so util must
                // be ≥~0.52 for positive KV (0.45 went negative-KV and failed).
                gpu_memory_utilization: 0.55,
                max_model_len: 4096,
                // Unlocks the throughput ceiling (default 256 caps at ~878 tok/s;
                // 512 reaches 1222). KV (~66-108k, varies) binds real paragraphs
                // ~c150-240, so concurrency 128 stays safely under that.
                max_num_seqs: Some(512),
                // CUDA graphs ON: 2.9x faster single-stream, +9% @c256, peak 1222
                // vs 878 eager. Capture uses only ~1 GiB. (The 7B-AWQ "graphs hurt"
                // finding does NOT apply to this 30B-FP8 + vLLM-main build.)
                enforce_eager: false,
                concurrency: 128,
            },
        }
    }
}

/// Default on-disk cache dir: `$XDG_CACHE_HOME/ferryman`, else
/// `$HOME/.cache/ferryman`. Avoids pulling a `dirs`-style crate for one lookup.
fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("ferryman");
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache").join("ferryman")
}

/// Default model root: `$HOME/model`. Resolved at runtime so the binary never
/// bakes in a specific user's home path — the presets join a model subpath onto
/// this. If `$HOME` is unset, falls back to a relative `model/` (will then fail
/// clearly at the docker mount, which is the right place to surface it).
fn model_root() -> String {
    std::env::var("HOME")
        .map(|h| format!("{h}/model"))
        .unwrap_or_else(|_| "model".to_string())
}

/// Suffix inserted before the extension when neither `--output` nor `--in-place`
/// is given: `book.epub` → `book.bilingual.epub`. Also doubles as the marker
/// the directory walk skips, so re-running a dir doesn't retranslate its output.
const OUTPUT_SUFFIX: &str = "bilingual";

/// Recursively collect every supported, non-output file under `root`, sorted
/// for deterministic ordering. Symlinks are skipped (avoids cycles).
fn collect_inputs(root: &Path) -> Result<Vec<PathBuf>> {
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
        // them. (book.bilingual.epub → stem's last dot-segment == "bilingual".)
        if format::Format::from_path(path).is_ok() && !is_suffix_output(path) {
            out.push(path.to_path_buf());
        }
    }
    Ok(())
}

/// Is `path` one of our suffixed outputs? The last dot-segment of the file stem
/// equals [`OUTPUT_SUFFIX`] (`book.bilingual` → `bilingual`).
fn is_suffix_output(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| stem.rsplit('.').next())
        .is_some_and(|last| last == OUTPUT_SUFFIX)
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

/// One opened input file awaiting its (post-drain) write.
struct FileSlot {
    doc: Box<dyn Document>,
    out_path: PathBuf,
    in_place: bool,
    input: PathBuf,
}

/// Turn a file's segments into self-contained [`Unit`]s for the shared pool.
///
/// `Independent` → one [`Unit::Single`] per segment; `Batched` → contiguous
/// batches of `batch_size` cues, each carrying `context` read-only preceding
/// cues (same file, in order). `budget` is the global `--limit`: each emitted
/// segment decrements it, and the last batch of a Batched file is shrunk to fit
/// (the `<cN>` delimiter scheme is count-agnostic, so a short final batch is
/// fine). When the budget hits zero the file stops emitting — so `--limit` caps
/// the whole batch, not each file.
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve the preset, then let any explicit --flag override it.
    let p = cli.preset.cfg();
    let host_model_dir = cli
        .host_model_dir
        .clone()
        .unwrap_or_else(|| p.host_model_dir.clone());
    let host_cache_dir = cli
        .host_cache_dir
        .clone()
        .unwrap_or_else(|| format!("{}/vllm-cache", model_root()));
    let serve_model = cli
        .serve_model
        .clone()
        .unwrap_or_else(|| p.serve_model.to_string());
    let dtype = cli.vllm_dtype.clone().unwrap_or_else(|| p.dtype.to_string());
    let kv_cache_dtype = cli
        .kv_cache_dtype
        .clone()
        .unwrap_or_else(|| p.kv_cache_dtype.to_string());
    let gpu_memory_utilization = cli.gpu_memory_utilization.unwrap_or(p.gpu_memory_utilization);
    let max_model_len = cli.max_model_len.unwrap_or(p.max_model_len);
    let max_num_seqs = cli.max_num_seqs.or(p.max_num_seqs);
    let enforce_eager = cli.enforce_eager || p.enforce_eager;
    let concurrency = cli.concurrency.unwrap_or(p.concurrency);

    eprintln!(
        "preset: {:?} | model {} | concurrency {} | {} KV | util {} | max-model-len {} | graphs {}",
        cli.preset,
        serve_model,
        concurrency,
        kv_cache_dtype,
        gpu_memory_utilization,
        max_model_len,
        if enforce_eager { "off (eager)" } else { "on" }
    );

    // Optionally launch (and on exit tear down) the vLLM container ourselves.
    let spec = container::ServeSpec {
        image: cli.image.clone(),
        host_model_dir: host_model_dir.clone(),
        host_cache_dir: host_cache_dir.clone(),
        container_model: serve_model.clone(),
        host_port: cli.host_port,
        container_name: cli.container_name.clone(),
        quantization: cli.quantization.clone(),
        dtype: dtype.clone(),
        kv_cache_dtype: kv_cache_dtype.clone(),
        gpu_memory_utilization,
        max_model_len,
        max_num_seqs,
        enforce_eager,
        health_timeout: cli.health_timeout,
    };
    // Guard stays alive until the end of main → container removed after translation
    // (and on any error via `?`, since Drop runs on unwind/return).
    let _guard = if cli.serve {
        Some(container::ContainerGuard::launch(&spec).await?)
    } else {
        None
    };
    let endpoint = _guard
        .as_ref()
        .map(|g| g.endpoint().to_string())
        .unwrap_or_else(|| cli.endpoint.clone());
    let model = if cli.serve {
        serve_model.clone()
    } else {
        cli.model
            .clone()
            .unwrap_or_else(|| serve_model.clone())
    };

    // --- format-agnostic translation pipeline ---
    // Open the input (dispatches on extension), pull its segments, translate
    // them all in one concurrent batch, write the result back in the input's
    // own format. Adding a format never touches anything below this line.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cli.timeout))
        .build()?;
    let cache = if cli.no_cache {
        None
    } else {
        let dir = cli.cache_dir.clone().unwrap_or_else(default_cache_dir);
        Cache::open(Some(dir))
    };
    let engine = Engine::new(client, endpoint, model, cli.target.clone(), concurrency, cache);

    // --- input enumeration ---
    // A directory is walked recursively for supported files; a single file is
    // processed as-is. --output (one path) can't be paired with a directory.
    if cli.input.is_dir() && cli.output.is_some() {
        anyhow::bail!(
            "--output cannot be combined with a directory input; use --in-place, \
             or drop both to write a '{OUTPUT_SUFFIX}' sibling next to each file"
        );
    }
    let inputs = if cli.input.is_dir() {
        let files = collect_inputs(&cli.input)?;
        eprintln!(
            "input dir {}: {} supported file(s) (recursed)",
            cli.input.display(),
            files.len()
        );
        files
    } else {
        vec![cli.input.clone()]
    };

    // --- open every file, build a flat shared work pool ---
    // Each file's segments become self-contained Units (Independent => one
    // Single per segment; Batched => batches with in-file context). All units
    // across all files drain through ONE concurrency pool in engine.run, so
    // small files and file tails no longer leave the GPU idle. A file that
    // fails to open is logged and skipped, never aborting the batch.
    let mut slots: Vec<FileSlot> = Vec::new();
    let mut units: Vec<Unit> = Vec::new();
    let mut budget = cli.limit; // global cap across the whole batch (None = unlimited).
    let mut open_failed: Vec<(PathBuf, String)> = Vec::new();
    for input in &inputs {
        let doc = match format::open(input, None) {
            Ok(d) => d,
            Err(e) => {
                let msg = format!("{e:#}");
                eprintln!("error: open {}: {} — skipping", input.display(), msg);
                open_failed.push((input.clone(), msg));
                continue;
            }
        };
        let segments = doc.segments();
        eprintln!(
            "{}: {} block(s) [{}]",
            input.display(),
            segments.len(),
            doc.format_name()
        );
        // Format picks Independent vs Batched; the CLI supplies batch params.
        let strategy = match doc.strategy() {
            Strategy::Independent => Strategy::Independent,
            Strategy::Batched { .. } => Strategy::Batched {
                batch_size: cli.subtitle_batch_size,
                context: cli.subtitle_context,
            },
        };
        let file_idx = slots.len();
        units.extend(build_units(file_idx, &segments, strategy, &mut budget));
        slots.push(FileSlot {
            doc,
            out_path: resolve_output(input, cli.in_place, cli.output.as_deref()),
            in_place: cli.in_place,
            input: input.clone(),
        });
    }

    // --- drain the whole batch through one shared concurrency pool ---
    let total_segments: usize = units.iter().map(Unit::attempted).sum();
    eprintln!(
        "batch: {} file(s), {} unit(s), {} segment(s)",
        slots.len(),
        units.len(),
        total_segments
    );
    let run_out = engine.run(units, total_segments).await;

    // --- partition results per file, write each (post-drain) ---
    // Writes happen after the drain (not progressively): the old per-file loop
    // already drained-then-wrote each file, so collapsing N (drain+write) pairs
    // into one drain + one write phase is wall-clock-neutral and keeps the drain
    // free of write stalls. Subset semantics (missing ids => unchanged) handle
    // partial / Ctrl-C output. A write failure is logged and skipped.
    let mut per_file: Vec<Vec<(SegmentId, String)>> = vec![Vec::new(); slots.len()];
    for ud in run_out.done {
        if let Some(slot) = per_file.get_mut(ud.file) {
            slot.extend(ud.pairs);
        }
    }
    for pairs in per_file.iter_mut() {
        pairs.sort_by_key(|(id, _)| *id);
    }

    let mut ok = 0usize;
    let mut write_failed: Vec<(PathBuf, String)> = Vec::new();
    for (i, slot) in slots.iter_mut().enumerate() {
        let pairs = &per_file[i];
        let write_target = if slot.in_place {
            inplace_temp(&slot.out_path)
        } else {
            slot.out_path.clone()
        };
        match slot.doc.write(pairs, &write_target, cli.mode) {
            Ok(()) => {
                if slot.in_place {
                    if let Err(e) = std::fs::rename(&write_target, &slot.out_path) {
                        let msg = format!("rename into place {}: {}", slot.out_path.display(), e);
                        eprintln!("error: {}", msg);
                        write_failed.push((slot.input.clone(), msg));
                        continue;
                    }
                }
                eprintln!(
                    "wrote: {} ({} block(s) translated){}",
                    slot.out_path.display(),
                    pairs.len(),
                    if run_out.cancelled { " [partial]" } else { "" }
                );
                ok += 1;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                eprintln!("error: write {}: {} — skipping", slot.out_path.display(), msg);
                write_failed.push((slot.input.clone(), msg));
            }
        }
    }

    // --- batch summary ---
    let failed_files = open_failed.len() + write_failed.len();
    eprintln!(
        "\nbatch: {} file(s) ok, {} failed; {} segment(s) translated, {} failed{}",
        ok,
        failed_files,
        run_out.translated,
        run_out.failed,
        if run_out.cancelled { " (interrupted)" } else { "" }
    );
    for (p, m) in open_failed.iter().chain(write_failed.iter()) {
        eprintln!("  failed: {} ({})", p.display(), m);
    }
    Ok(())
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
        let ids: Vec<_> = units
            .iter()
            .map(|u| match u {
                Unit::Single { id, .. } => *id,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec![0, 1, 2]);
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
        // batch_size 25, limit 3 -> a single batch of 3 (not 25).
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
        // Two files sharing one global budget (mirrors main's loop): file 0
        // consumes all 3, file 1 gets nothing.
        let segs: Vec<_> = (0..5).map(|i| seg(i, "x")).collect();
        let mut budget = Some(3);
        let u1 = build_units(0, &segs, Strategy::Independent, &mut budget);
        let u2 = build_units(1, &segs, Strategy::Independent, &mut budget);
        assert_eq!(u1.len(), 3);
        assert_eq!(u2.len(), 0);
        assert_eq!(budget, Some(0));
    }

    #[test]
    fn attempted_sums_to_segment_count() {
        let segs: Vec<_> = (0..5).map(|i| seg(i, "x")).collect();
        let units = build_units(
            0,
            &segs,
            Strategy::Batched {
                batch_size: 2,
                context: 0,
            },
            &mut None,
        );
        let total: usize = units.iter().map(Unit::attempted).sum();
        assert_eq!(total, 5);
    }
}
