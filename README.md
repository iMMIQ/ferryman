# Ferryman

> **Ferryman** — a multi-format translation tool (epub / txt / subtitles / …).
> *"渡船工": ferries content across the river between languages and formats.*
>
> **Status:** ships the bilingual-EPUB translator today (ported from the old
> `epub-translator`); plain-text and subtitle pipelines are planned. The
> package/binary are now `ferryman`; the epub code itself is unchanged.

Produce a **bilingual (original + Chinese) EPUB** from any EPUB book by translating
its content through a vLLM-served model. The original formatting is preserved
byte-for-byte; after each translated block a styled sibling carrying the
translation is inserted.

Built and tested against Tencent Hunyuan **Hy-MT2-7B** translation models served
by `docker.io/catdogai/lzc-aipod-vllm:agxorin-cu126-src-18f658bb3185-20260703` on a Jetson AGX Orin.
**FP8** is the recommended variant (≈full precision, fast); AWQ (`awq_marlin`)
is faster but slightly less faithful; bf16 is the full-precision reference.
Works with any OpenAI-compatible `/v1/chat/completions` endpoint.

With `--serve` the program launches the vLLM container itself and tears it down
when finished (no need to run the server separately).

## How it works

1. Unzips the EPUB, parses `META-INF/container.xml` → the OPF, and resolves the
   ordered **spine** content documents (skips the EPUB3 `nav` document).
2. For each content XHTML, a single `lol_html` rewrite pass:
   - injects a `<style>` for `.hy-zh` into `<head>`,
   - finds **leaf** block elements (`p`, `h1`–`h6`, `li`, `blockquote`,
     `figcaption`, `dt`, `dd`), where a "leaf" is a tracked block that contains
     no other tracked block (containers are skipped so nested content isn't
     double-translated),
   - collects each leaf's plain text and inserts a placeholder comment after it.
3. Translates all collected texts concurrently (configurable, semaphore-limited)
   via the model, using Hy-MT2's official "Default Translation" prompt and
   recommended params (temp 0.7 / top_p 0.6 / top_k 20 / rep_penalty 1.05).
4. Replaces each placeholder with `<p class="hy-zh">…</p>` (or `<li>`/`<dt>`/`<dd>`
   to stay list-valid), HTML-escaped, and strips any leftover placeholders.
5. Re-zips, with `mimetype` stored uncompressed as the first entry (EPUB-valid).

## Build

```bash
cargo build --release
```

## Usage

### Self-managed container (recommended)

`--serve` makes the program start the vLLM container, wait until it's healthy,
translate, then shut the container down — so nothing needs to be running
beforehand:

```bash
# defaults are the `7b-fp8` preset (Hy-MT2-7B-FP8): launches the container, translates, cleans up
./target/release/ferryman \
  --input  "lonely planet Iceland.epub" \
  --output "Iceland_bilingual.epub" \
  --serve

# Hy-MT2-30B-A3B-FP8: higher quality. `--preset 30b-fp8` injects the optimal
# serve config we benchmarked on this Jetson — CUDA graphs ON, max-num-seqs 512,
# gpu-memory-utilization 0.55, fp8 KV cache, max-model-len 4096 (~1222 tok/s peak,
# 2.9x faster single-stream than eager). Needs ~34 GiB free; concurrency defaults
# to 128 (raise toward 256 for short blocks, the 30B has headroom past it).
./target/release/ferryman -i book.epub -o out.epub --serve --preset 30b-fp8

# switch to AWQ (fastest): point at the AWQ model dir + awq_marlin quantization
./target/release/ferryman -i book.epub -o out.epub --serve \
  --preset 7b-fp8 \
  --host-model-dir ~/model/Hy-MT2-7B-AWQ \
  --serve-model /models/Hy-MT2-7B-AWQ \
  --quantization awq_marlin --gpu-memory-utilization 0.25

# quick smoke test (only translate ~20 blocks)
./target/release/ferryman -i book.epub -o out.epub --serve --limit 20
```

### External server (no `--serve`)

If a vLLM (or any OpenAI-compatible) server is already running, skip `--serve`
and point at it — the preset still picks the right model id:

```bash
# 7B (already running on :8001)
./target/release/ferryman -i book.epub -o out.epub \
  --endpoint http://localhost:8001 --target 中文

# 30B (already running on :8001) — preset sets the model id + concurrency
./target/release/ferryman -i book.epub -o out.epub --preset 30b-fp8
```

### Directory / batch mode

Point `--input` at a directory to translate a whole library in one go. It walks
recursively, picks up every supported file, and **reuses one engine (and one
`--serve` container) for the whole batch** — a single file failure is logged and
skipped, never aborting the rest. A Ctrl-C writes the current file's partial
output and stops the batch.

```bash
# each book.epub -> book.bilingual.epub (sibling); unsupported files skipped
./target/release/ferryman -i ~/data/books --preset 30b-fp8

# overwrite every file in place (atomic temp + rename; originals not truncated)
./target/release/ferryman -i ~/data/books --in-place --preset 30b-fp8
```

Re-running a directory is safe: `*.bilingual.*` outputs are skipped, and the
on-disk cache means already-translated blocks are instant.

### Options

| flag | default | description |
|---|---|---|
| `--input` | — | input file **or directory**. A directory is walked recursively and every supported file (`.epub .srt .vtt .ass .ssa .lrc .txt .md`) is translated; unsupported files and ferryman's own `*.bilingual.*` outputs are skipped. |
| `--output` | — | output path (single file only; rejected with a directory input). If neither `--output` nor `--in-place` is set, each file is written next to its source as `<name>.bilingual.<ext>`. |
| `--in-place` | off | overwrite each input file in place (writes a sibling temp, then atomically renames over the original). Works for a single file or a directory. Mutually exclusive with `--output`. |
| `--preset` | `7b-fp8` | model + optimal serve config bundle: `7b-fp8` (Hy-MT2-7B-FP8) or `30b-fp8` (Hy-MT2-30B-A3B-FP8). Every flag below overrides the preset. |
| `--serve` | off | launch & manage the vLLM container (removed on exit) |
| `--endpoint` | `http://localhost:8001` | base URL (used when not `--serve`) |
| `--model` | preset | served model id (used when not `--serve`) |
| `--target` | `中文` | target language full name (`English`, `日本語`, …) |
| `--concurrency` | `256` (7b) / `128` (30b) | max concurrent translation requests |
| `--limit` | — | cap total translated blocks (testing) |
| `--no-cache` | off | disable the on-disk translation cache (retranslate every block) |
| `--cache-dir` | `$XDG_CACHE_HOME/ferryman` or `~/.cache/ferryman` | translation cache dir; lets re-runs skip done blocks and keeps finished ones after Ctrl-C |
| `--timeout` | `180` | per-request timeout (seconds) |
| `--image` | `…catdogai/lzc-aipod-vllm:agxorin-cu126-…` | docker image (`--serve`) |
| `--host-model-dir` | preset | host model dir to mount (`--serve`) |
| `--host-cache-dir` | `…/vllm-cache` | persisted JIT/compile cache (FlashInfer/Triton/vLLM/inductor) — first launch compiles (~2.5-5 min), later launches reuse it (`--serve`) |
| `--serve-model` | preset | in-container model path + id (`--serve`) |
| `--container-name` | `ferryman-vllm` | container name (`--serve`) |
| `--host-port` | `8001` | host port → container 8000 (`--serve`) |
| `--quantization` | — | e.g. `awq_marlin`; omit to auto-detect/FP8 (`--serve`) |
| `--vllm-dtype` | `float16` (7b) / `auto` (30b) | compute dtype (`--serve`) |
| `--kv-cache-dtype` | `fp8` | KV cache dtype (`fp8` halves KV memory + boosts decode; `auto` = native) (`--serve`) |
| `--gpu-memory-utilization` | `0.30` (7b) / `0.55` (30b) | vLLM GPU memory util (`--serve`) |
| `--max-model-len` | `8192` (7b) / `4096` (30b) | model context (`--serve`) |
| `--max-num-seqs` | `512` (both) | vLLM admission cap; 512 unlocks the 30B throughput ceiling (`--serve`) |
| `--enforce-eager` | off | force eager mode (disable CUDA graphs). Both presets leave it off (graphs are faster on this Jetson); set only to A/B test eager (`--serve`) |
| `--health-timeout` | `600` | seconds to wait for health (`--serve`) |

> Both presets enable **CUDA graphs** (omit `--enforce-eager`). Measured on this
> Jetson: 30B ~2.9x faster single-stream + peak ~1222 tok/s; 7B +8% ceiling
> (868→938 tok/s) + 15% at low concurrency. (The old "graphs hurt on Jetson"
> note was AWQ-specific; for FP8 on this vLLM build graphs are a net win.)

## Resumability & interruption

- **Translation cache.** Every translated block is written to a content-addressed
  cache keyed by `(model, target, text)`, so re-running ferryman on the same
  book with the same model + target language skips already-done blocks almost
  instantly. `--no-cache` disables it; `--cache-dir` points it elsewhere.
- **Ctrl-C is safe.** One Ctrl-C stops dispatching new requests, cancels the few
  in flight, writes the partial bilingual EPUB gathered so far, and (with
  `--serve`) tears the container down — nothing leaks. Re-running then resumes
  from the cache. (Press **once**; a second Ctrl-C during the final write is
  swallowed. Second-press force-quit may come later.)
- **Per-block failures never abort the run.** A 4xx (bad model id, malformed
  request, or a block over the context window) fails that one block immediately
  without retrying; 5xx / 429 / network errors retry with backoff. Failed blocks
  are left untranslated in the output and counted in the summary.

## Notes / limitations

- Translations are plain text; inline markup inside a block (e.g. `<strong>`,
  `<a>`) is preserved in the **original** but rendered as plain text in the
  appended translation.
- `<pre>`/`<code>`/`<script>`/`<style>`/`<svg>`/`<head>` content is not translated.
- Table cells (`td`/`th`) and `<div>` wrappers are intentionally not translated
  (to avoid duplicating nested content and to keep XHTML valid).
- A single block longer than the model's context window (`--max-model-len`,
  8192 for `7b-fp8` / 4096 for `30b-fp8`) may be truncated by the model. The
  output budget is left unset so vLLM fills whatever context remains after the
  prompt; a block whose own input exceeds `--max-model-len` fails that block
  rather than producing a truncated translation.
