# Ferryman

> **Ferryman** — a multi-format translation tool (epub / txt / subtitles / …).
> *"渡船工": ferries content across the river between languages and formats.*
>
> **Status:** EPUB, DOCX, plain text, Markdown, SRT, VTT, ASS/SSA and LRC ship
> today. Ferryman can run as a CLI or as a Lazycat MicroServer + AI Pod Web app.

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

## Web app and Lazycat deployment

Ferryman's Web deployment has two processes:

- `ferryman-web` runs on the MicroServer. It owns uploads, per-user files, the
  persistent job queue, progress/cancellation and result downloads. Job metadata
  and state live in a SQLite WAL database; source/result files stay on disk, and
  only nonterminal jobs are retained in memory. History is read with cursor
  pagination while active jobs use a small polling endpoint. After a system or
  application restart, queued/model-starting/translating/writing jobs are reset
  to queued and dispatched automatically; the content cache skips segments that
  were already translated. Temporary AI Pod outages keep waiting with backoff
  instead of turning recovered jobs into failures. Up to eight jobs
  using the same preset run together through one process-wide HTTP client and
  request budget (256 requests for 7B, 128 for 30B). Preset groups stay ordered,
  so the scheduler never tries to switch models while jobs are active.
- `ferryman-agent` is the lightweight AI Pod controller. It starts exactly one
  Hy-MT2 vLLM child process (`7b-fp8` or `30b-fp8`) on demand and unloads it
  after all leases expire and the idle timeout elapses. It also detects existing
  models, downloads missing weights with pause/resume and integrity checks, and
  benchmarks ModelScope, HF Mirror and Hugging Face using real model shards.

The Web UI accepts an uploaded file or a mixed selection of files and directories
from the user's MicroServer documents and mounted Lazycat cloud drives. The picker
can filter the current directory by name and entry type. Selected directories are
walked recursively, overlapping selections are deduplicated, and relative paths
are preserved. Mounted files can be saved beside each source with a suffix,
atomically overwrite the source when explicitly requested, or be collected under
one chosen directory on either storage type. Advanced settings expose the shared
batch size, context window and translation-cache controls; the target language is
free-form with common languages offered as suggestions. Cancelling a running job
keeps any partial document available for download without overwriting a mounted
source.
The LPK requests `document.read/write` and `media.read/write`, and enables the
corresponding `/lzcapp/run/mnt/home` and `/lzcapp/media/RemoteFS` mounts. Both
mounts contain one directory per MicroServer UID; the backend confines every
request to the signed-in user's subtree, skips symlinks, and keeps internal
queue data under `/lzcapp/var/ferryman`.

Run both locally with a shared control token:

```bash
export FERRYMAN_AGENT_TOKEN='replace-with-at-least-16-characters'

FERRYMAN_MODEL_ROOT="$HOME/model" \
FERRYMAN_AGENT_LISTEN=127.0.0.1:8090 \
cargo run --bin ferryman-agent

mkdir -p ./ferryman-documents ./ferryman-remotefs
FERRYMAN_AGENT_URL=http://127.0.0.1:8090 \
FERRYMAN_USER_DOCUMENTS_DIR=./ferryman-documents \
FERRYMAN_REMOTE_FS_DIR=./ferryman-remotefs \
FERRYMAN_ALLOW_LOCAL_USER=true \
cargo run --bin ferryman-web
```

The Web UI is then available at `http://127.0.0.1:8080`.

For AI Pod deployment, the release build packages the native ARM64 controller
binary directly inside `ai-pod-service` and reuses the official vLLM image:

```bash
# On an ARM64 development host, install the MicroServer Web cross compiler once:
# sudo apt-get install gcc-x86-64-linux-gnu
sh scripts/build-release.sh
```

Models are downloaded from the Web model manager into the app's persistent AI
Pod data directory. Existing directories in these locations are detected without
another download:

```text
models/Hy-MT2-7B-FP8
models/Hy-MT2-30B-A3B-FP8
```

Model weights and incomplete downloads live under `LZC_AGENT_DATA_DIR` on the
selected AI Pod, not on the MicroServer. Rebuildable vLLM/JIT caches live under
`LZC_AGENT_CACHE_DIR`. Upgrading the app or restarting the AI Pod preserves the
models; switching to another AI Pod requires that pod to download its own copy.

The `FERRYMAN_AGENT_TOKEN` in `lzc-manifest.yml` and
`ai-pod-service/docker-compose.yml` must always contain the same production
value. Then lint and build the LPK V2 package:

```bash
lzc-cli project lint .
lzc-cli project build .
```

### Standalone Docker deployment (single machine)

On a single machine without Lazycat — a Jetson board or any Linux host with an
NVIDIA GPU — `docker-compose.standalone.yml` runs both processes with plain
Docker. Everything builds from source inside Docker, so no cross toolchain or
host Rust install is needed:

```bash
cp .env.example .env   # then set FERRYMAN_AGENT_TOKEN (openssl rand -hex 32)
docker compose -f docker-compose.standalone.yml up -d --build
```

The web UI listens on `http://<host>:$FERRYMAN_WEB_PORT` (default 8080). The
job database, both libraries, model weights and vLLM caches live in host
directories configured in `.env`; `init-dirs` creates the
`local-development-user/` library subtree on first start. Models are
downloaded through the web model manager, or detected automatically when
`$FERRYMAN_MODELS_DIR` already contains `Hy-MT2-7B-FP8/` or
`Hy-MT2-30B-A3B-FP8/`.

The stack runs in **single-user mode**: every visitor shares the
`local-development-user` identity, so keep the port on a trusted network. For
multiple users, front the web service with an authenticating reverse proxy
that injects a per-user `safe_uid` header, and create
`<documents>/<uid>/` and `<remotefs>/<uid>/` directories for each user.

Non-Jetson hosts: set `VLLM_BASE_IMAGE` to a vLLM image matching the host
CUDA stack and `FERRYMAN_VLLM_LD_PRELOAD=` (empty) in `.env`. The agent sizes
every launch from the GPU that is actually present — it probes total/free
memory through the image's torch, sums the model's safetensors weights, and
derives `--gpu-memory-utilization` and the KV cache size from them (on the
reference 64 GiB Orin this reproduces the tuned 0.30/0.55 + 8/3 GiB values;
a preset that cannot fit is rejected with the numbers instead of failing
inside vLLM). `--kv-cache-memory-bytes` is passed only when the chosen vLLM
build accepts it; `FERRYMAN_VLLM_KV_CACHE_FLAG=0/1` overrides that probe.

### Shared core boundary

The CLI and Web adapter intentionally stop at the same library boundary:

- `format` parses and renders every supported document type.
- `translate` owns prompts, retries and response parsing.
- `engine` owns caching, authenticated HTTP clients and request limiting.
- `batch` owns input discovery, output naming, concurrent dispatch, cancellation
  and progressive writes.
- `settings` owns defaults and Web safety bounds.

`ferryman` maps CLI flags into these shared types. `ferryman-web` adds identity,
mounted-storage validation, persistent job records and model leases, then calls
the same engine and batch functions. `ferryman-agent` remains isolated to AI Pod
model lifecycle and vLLM proxying.

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
| `--kv-cache-memory-bytes` | `8 GiB` (7b) / `3 GiB` (30b) | fixed KV cache capacity; avoids Jetson UMA profiling variance (`--serve`) |
| `--max-model-len` | `8192` (7b) / `4096` (30b) | model context (`--serve`) |
| `--max-num-seqs` | `512` (both) | vLLM admission cap; 512 unlocks the 30B throughput ceiling (`--serve`) |
| `--enforce-eager` | off | force eager mode (disable CUDA graphs). Both presets leave it off (graphs are faster on this Jetson); set only to A/B test eager (`--serve`) |
| `--health-timeout` | `600` | seconds to wait for health (`--serve`) |

> Both presets enable **CUDA graphs** (omit `--enforce-eager`). Measured on this
> Jetson: 30B ~2.9x faster single-stream + peak ~1222 tok/s; 7B +8% ceiling
> (868→938 tok/s) + 15% at low concurrency. (The old "graphs hurt on Jetson"
> note was AWQ-specific; for FP8 on this vLLM build graphs are a net win.)
> KV cache capacity is fixed per preset because CUDA free-memory deltas are not
> stable on Jetson UMA systems when Linux reclaims memory or uses zram swap.

## Resumability & interruption

- **Translation cache.** Every translated block is written to a content-addressed
  cache keyed by `(model, target, text)`, so re-running ferryman on the same
  book with the same model + target language skips already-done blocks almost
  instantly. Cache bodies remain sharded files rather than SQLite rows: cache
  writes are frequent, best-effort optimization data and should not inflate or
  contend on the authoritative job database. `--no-cache` disables it;
  `--cache-dir` points it elsewhere.
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
