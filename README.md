# Ferryman

> **Ferryman** — a multi-format translation tool (epub / pdf / txt / subtitles / …).
> *"渡船工": ferries content across the river between languages and formats.*
>
> **Status:** EPUB, DOCX, PDF, plain text, Markdown, SRT, VTT, ASS/SSA and LRC
> ship today. Ferryman can run as a CLI or as a Lazycat MicroServer + AI Pod Web app.

Produce a **bilingual (original + Chinese) EPUB** from any EPUB book by translating
its content through a Orinfer-served model. The original formatting is preserved
byte-for-byte; after each translated block a styled sibling carrying the
translation is inserted.

Ferryman uses **Orinfer 0.2.0** on Jetson AGX Orin with the prepared
**Index-Translate-2B / 9B / 35B-A3B-preview E8P Q4A8** models. The default model alias is `index-translate-2b`. The AI Pod Agent owns the
inference process, listening only on container loopback `127.0.0.1:18088`.
The inference port is not published; the Web backend uses authenticated Agent calls.

With `--serve`, Ferryman starts a container built from the bundled Orinfer
runtime and stops it after translation.

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
   via Index translation prompts, with greedy sampling and thinking disabled.
4. Replaces each placeholder with `<p class="hy-zh">…</p>` (or `<li>`/`<dt>`/`<dd>`
   to stay list-valid), HTML-escaped, and strips any leftover placeholders.
5. Re-zips, with `mimetype` stored uncompressed as the first entry (EPUB-valid).

### PDF (extract → translate → edit the original in place)

PDF pages place glyph runs at coordinates, so Ferryman first *reconstructs
paragraphs from geometry*: a custom `pdf-extract` output device keeps every
character's position, characters group into visual lines (same baseline,
forward direction) and lines into paragraphs — previous line ran full width,
same leading, flush left margin, dehyphenated — exactly like a docx
paragraph. A sentence wrapped over three lines is one segment, and
paragraphs even continue across page breaks. Table-of-contents entries
(dot-leader or right-aligned-number fills detected geometrically), page
numbers and other letterless lines pass through untouched; ligatures (ﬁ/ﬂ…)
are normalized before translation; some producers' broken `/W` glyph-width
arrays are repaired so line geometry survives extraction.

The output edits the original file with `lopdf`, preserving figures, diagrams
and vector art while laying out translated text:

- **bilingual** (default): a translation page of the same size is inserted
  after every original page, each paragraph's Chinese drawn at the mirrored
  position in gray (shrunk to fit its slot); original pages are untouched.
- **replace**: each paragraph is covered by a background-colored rect on its
  own page and the translation drawn in the same box.

Rendering needs one CJK-capable OpenType font on disk: `FERRYMAN_PDF_FONT`
overrides discovery, otherwise the first of Noto Sans CJK / WenQuanYi /
Droid Sans Fallback in the usual distro locations (the Lazycat image bundles
it at `/app/fonts`; the standalone image installs `fonts-noto-cjk`). The
font is embedded **subset to the glyphs actually drawn** as a CID/Type0 font
with a generated ToUnicode map and logical `ActualText`. Unicode line breaking,
grapheme boundaries, Rustybuzz shaping and bidi ordering preserve combining marks,
ligatures and right-to-left runs. Optional Noto/DejaVu fallbacks are discovered in
`/app/fonts` and standard distro directories; set `FERRYMAN_PDF_FALLBACK_FONTS`
to an OS path-list of additional fonts (colon-separated on Linux). Install
`fonts-noto-core` for Arabic, Hebrew, Devanagari and Thai coverage; the release
script bundles available fallbacks. Missing glyph coverage fails explicitly with
a font configuration hint instead of silently dropping characters. Fonts are
subset to the glyphs used, keeping output much smaller than full font files. Scanned PDFs without a text layer and encrypted PDFs fail with a
clear error.

## Build

```bash
cargo build --release
```

## Tests and resource limits

PDF tests require a CJK font and fail if it is missing. On Debian/Ubuntu,
install `fonts-noto-cjk` and `fonts-dejavu-core` (the latter supplies the Arabic
shaping fixture), or point `FERRYMAN_PDF_FONT` at an installed CJK font.
CI installs both and explicitly selects Noto Sans CJK.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

EPUB/DOCX archives are limited to 10,000 entries, 128 MiB per decompressed
entry and 512 MiB decompressed in total. Both declared sizes and actual reads
are checked. Document parsing runs on blocking threads, with at most two
parsers per process. Cancellation stops waiting immediately; an already
running parser finishes in the background while retaining its concurrency
slot. Empty or explicitly truncated model responses are never cached as
successful translations; truncation triggers bounded splitting/retranslation.

### Browser UI regression tests

The frontend uses Vue 3, TypeScript, Vite, Reka UI and TanStack Vue Query. Node 22.12+ is needed
for development/builds; production still serves static files from Rust without
a Node process. `npm run build` type-checks the frontend and writes fingerprinted
assets to `dist/web` (the default `FERRYMAN_WEB_DIR`).

Browser regression tests serve the production assets and mock API responses.
A separate smoke test starts the real Rust server against temporary storage and
checks config, directory selection, preview and job lifecycle contracts without a GPU.
Its temporary job waits for an unavailable agent, then is cancelled and deleted through the UI.

```bash
npm ci
npm run dev # Vite dev server; /api proxies to Rust on 127.0.0.1:8080
# In another terminal, or stop the dev server before running tests:
npx playwright install --with-deps chromium
npm run types:check # regenerate Rust DTOs into a temporary directory and compare
npm run test:unit
npm test # type-check, production build, browser regression tests
cargo build --locked --bin ferryman-web
npm run test:smoke
# Optional screenshot artifacts:
UI_SCREENSHOT_DIR=/tmp/ferryman-ui npm test
```

The suite covers mobile navigation, keyboard focus across polling/dialogs,
partial results, connection recovery, multi-file upload/cancellation, overwrite
preview, request races and per-job mutation locks. CI also runs strict frontend
formatting, request-layer tests, the real-server smoke test and Rust tests.

Frontend code is split by domain under `web/src`: `useJobs`, `useSubmission`,
`useFolder`, and `useRuntime` own state; components render that state. Desktop
tables and mobile cards share actions and progress components. `lib/api.ts`
centralizes Fetch timeouts/errors. Vue Query owns query keys, cancellation,
request deduplication, polling and invalidation; polling pauses when the page is
hidden. Mutations are not automatically retried, and business-level per-job locks
prevent duplicate actions. Request snapshots and overwrite acknowledgement remain
in `useSubmission`.

Wire types in `web/src/lib/generated` are generated from Rust with ts-rs; do not
edit them manually. Run `npm run types:generate` after changing a DTO and include
the generated changes. `npm run types:check` checks drift without modifying the
working tree. Rust numeric counters/timestamps use JSON numbers, so the exporter
maps large integer types to TypeScript `number` (values must remain within JS safe
integer precision); nullable and omitted fields retain their Serde semantics.
Generated types do not validate untrusted JSON at runtime. UI drafts and derived
views live separately in `lib/types.ts`.

Playwright Test manages browser fixtures, the Vite preview server, retrying DOM
assertions and failure screenshots/traces. CI uploads failure artifacts from
`test-results` and `playwright-report`; inspect traces with `npx playwright show-trace`.

UI tokens live in
`tokens.css`, with shared buttons, notices, dialogs and empty states under
`components/ui`. Run `npm run format` after editing frontend sources.

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
  using the same preset may run together. Web translation uses Orinfer JSONL:
  requests from one document are collected into bounded rounds (up to 2048
  requests / 63 MiB), and the Agent serializes GPU rounds with up to 32 active
  model requests. Preset groups stay ordered, so the scheduler never tries to
  switch models while jobs are active.
- `ferryman-agent` is the lightweight AI Pod controller. It starts exactly one
  Orinfer child process (`index-translate-2b`, `index-translate-9b` or `index-translate-35b`) on demand and unloads it
  after all leases expire and the idle timeout elapses. Document jobs invoke
  `orinfer batch` with throughput scheduling and automatic prefix planning;
  online/manual serving and batch execution are mutually exclusive. It also detects existing
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
On mobile, **New translation / Tasks** tabs keep the queue accessible and the
submit action stays visible at the bottom. Uploads accept multiple files, show
progress, and retain unsubmitted files after cancellation or errors. Every
submission has a preview; mounted-directory previews list output paths and
skip reasons, and overwriting requires an explicit acknowledgement. Previews
are advisory: submission rechecks the files and destination.

A job with untranslated segments is shown as **Partially completed**, with
separate translated/total counts and a retry action. Failed and cancelled jobs
can also resume. Each new Web job stores its successful translations in a
private `checkpoint` directory, independent of the optional shared cache;
retrying reuses these entries. Jobs created before this update may need to
retranslate if their old shared cache is unavailable. Retrying keeps saved
results until a replacement is ready and refuses to overwrite user edits made
since the previous attempt. Task details expose full paths, errors and partial
result downloads; connection errors do not clear previously loaded history.

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

npm ci && npm run build
mkdir -p ./ferryman-documents/local-development-user ./ferryman-remotefs/local-development-user
FERRYMAN_AGENT_URL=http://127.0.0.1:8090 \
FERRYMAN_USER_DOCUMENTS_DIR=./ferryman-documents \
FERRYMAN_REMOTE_FS_DIR=./ferryman-remotefs \
FERRYMAN_ALLOW_LOCAL_USER=true \
cargo run --bin ferryman-web
```

The Web UI is then available at `http://127.0.0.1:8080`.

For AI Pod deployment, the release build packages the explicit ARM64 controller
binary in a separate resource-only LPK. It bundles the verified Orinfer 0.2.0
engine release and installs all three Index execution packages during the build:

```bash
# Install the pinned toolchains, cross compilers, fonts and verifier dependencies
# described in docs/packaging.md. Build either product independently:
sh scripts/build-release.sh
sh scripts/build-agent-release.sh
```

Models are downloaded from the Web model manager into the app's persistent AI
Pod data directory. Existing directories in these locations are detected without
another download:

```text
models/Index-Translate-2B-Orinfer-E8P-Q4A8
models/Index-Translate-9B-Orinfer-E8P-Q4A8
models/Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8
```

Model weights and incomplete downloads live under `LZC_AGENT_DATA_DIR` on the
selected AI Pod, not on the MicroServer. Runtime caches live under `LZC_AGENT_CACHE_DIR`. Installed execution packages
are read directly from the read-only AI Pod resource at `/opt/orinfer/packages`. Upgrading the app or restarting the AI Pod preserves the
models; switching to another AI Pod requires that pod to download its own copy.

Lazycat deployments generate a device-specific `FERRYMAN_AGENT_TOKEN` in the
Web application's persistent private data on first startup. The Web service
pairs that token with the AI Pod, where only its SHA-256 hash is persisted under
`${LZC_AGENT_DATA_DIR}/auth`. No production bearer token is stored in the
repository or either LPK. The token file is mode `0600`. Then lint and build the
Web and AI Pod resource packages:

```bash
lzc-cli project lint .
lzc-cli project release .
lzc-cli project release -f lzc-build.aipod.yml
```

The AI Pod resource package requires LZCOS 1.5.2+ and currently supports AGX Orin.
See [packaging](docs/packaging.md) for final LPK verification, local CI packaging,
artifact checksums, toolchain pins and standalone build targets.

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
job database, both libraries, model weights and runtime caches live in host
directories configured in `.env`; `init-dirs` creates the
`local-development-user/` library subtree on first start. Models are
downloaded through the web model manager, or detected automatically when
`$FERRYMAN_MODELS_DIR` already contains the Index model directories listed above.

The stack runs in **single-user mode**: every visitor shares the
`local-development-user` identity, so keep the port on a trusted network. For
multiple users, front the web service with an authenticating reverse proxy
that injects a per-user `safe_uid` header, and create
`<documents>/<uid>/` and `<remotefs>/<uid>/` directories for each user.

The packaged inference runtime targets ARM64 Jetson AGX Orin / SM87. Models
are downloaded into the persistent `/models` mount, including their
`cache/model.json` and `cache/weights` subdirectories. The 35B download sources are
[Hugging Face](https://huggingface.co/immiq/Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8)
and [ModelScope](https://modelscope.cn/models/iammiq/Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8).
Existing prepared model directories are detected locally.

The AI Pod and standalone Agent deploy their own model processes. Models start
on demand, switch after leases drain and unload after the idle timeout. The
Agent reads weights from `/models` and serves inference only on container
loopback `127.0.0.1:18088`. No inference port is mapped to the host.

### Shared core boundary

The CLI and Web adapter intentionally stop at the same library boundary:

- `format` parses and renders every supported document type.
- `translate` owns prompts, retries and response parsing.
- `engine` owns caching, checkpoints and execution through HTTP or JSONL.
- `jsonl` groups Chat requests, polls completed rows by byte cursor and maps
  `custom_id` back to the waiting translation. Failed/truncated requests use
  the same bounded retry/split logic and create fresh recovery rounds.
- `batch` owns input discovery, output naming, concurrent dispatch, cancellation
  and progressive writes.
- `settings` owns defaults and Web safety bounds.

`ferryman` maps CLI flags into these shared types. `ferryman-web` adds identity,
mounted-storage validation, persistent job records and model leases, then calls
the same engine and batch functions. `ferryman-agent` remains isolated to AI Pod
model lifecycle, the authenticated JSONL spool and Orinfer proxying.

JSONL input/output files live only in the AI Pod runtime cache and are removed
when a round finishes or is cancelled. Interrupted Agent runs can resume the same
immutable input with Orinfer's `--resume`; completed Web units also survive in
per-job checkpoints. Truncated output is never cached as a complete translation.
Cancellation sends SIGTERM, waits for the worker to drain, then reaps its process
group. Unreachable Web leases expire and stop orphan workers; stale spools are
collected automatically. Model inference has no public HTTP port.

## Usage

### CLI development with an existing Orinfer service

```bash
./target/release/ferryman -i book.epub -o bilingual.epub
./target/release/ferryman -i book.epub -o bilingual.epub --preset index-translate-9b
./target/release/ferryman -i book.epub -o bilingual.epub --preset index-translate-35b
./target/release/ferryman -i ./books --in-place --target English
```

The endpoint defaults to `http://127.0.0.1:8088`. `--model` overrides the
OpenAI model alias; `--endpoint` selects a different server.

### Self-managed container

Build the standalone Agent image first, then use `--serve`. The image includes
the engine and all three execution packages; model weights come from the host:

```bash
docker compose -f docker-compose.standalone.yml build agent
./target/release/ferryman -i book.epub -o bilingual.epub --serve \
  --host-model-dir /path/to/Index-Translate-2B-Orinfer-E8P-Q4A8
```

| Flag | Default | Purpose |
| --- | --- | --- |
| `--preset` | `index-translate-2b` | `index-translate-2b`, `index-translate-9b`, `index-translate-35b` |
| `--endpoint` | `http://127.0.0.1:8088` | Existing inference service |
| `--model` | Preset alias | OpenAI model name |
| `--target` | `中文` | Target language |
| `--concurrency` | `32` | Concurrent translation requests |
| `--serve` | Off | Start and stop an Orinfer Docker container |
| `--image` | `ferryman-agent:standalone` | Image used by `--serve` |
| `--host-model-dir` | `$HOME/model/<preset directory>` | Host weights |
| `--host-cache-dir` | `$HOME/model/orinfer-cache` | Runtime cache |
| `--container-name` | `ferryman-orinfer` | Managed container name |
| `--host-port` | `8088` | Host port mapped to container 8088 |
| `--no-cache` | Off | Disable translation caching |
| `--health-timeout` | `600` | Startup timeout in seconds |

Run `ferryman --help` for document, batching, output and cache options.

## Resumability & interruption

- **Translation cache.** Every translated block is written to a content-addressed
  cache keyed by the translation revision, endpoint, model, target language,
  text and strategy. Batched keys also include all neighboring cues, preceding
  context and the cue position, so changing context or batch boundaries cannot
  reuse an unrelated translation. Older cache revisions are ignored (existing
  files are not deleted); the first run after this update translates again. Cache bodies remain sharded files rather than SQLite rows: cache
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
  are left untranslated in the output and counted in the summary. The CLI exits
  with a nonzero status if any file or segment failed, or the run was cancelled.
- **Web cancellation has a save boundary.** Cancellation is accepted before
  `Writing`; once final saving begins, the API returns HTTP 409 and the UI hides
  the cancel action. An accepted cancellation never commits a mounted-file save.
- **Mounted batches publish atomically.** All input copies are prepared before
  jobs enter the queue in one database transaction. The UI retains a submission
  ID across retries and page reloads in the same tab; retrying the same request
  returns its original job IDs, even if the source directory has since changed.
  History is periodically reconciled, and open details poll the individual job.
- **Local models are checked before use.** Detection validates JSON metadata,
  safetensors headers, tensor offsets and shard lengths. Downloaded models also
  use recorded file sizes and available hashes; damaged files can be downloaded
  again. Startup verification runs in the background so the controller's health
  endpoint stays available; models remain `Verifying` and cannot be loaded or
  deleted until the scan completes. Orinfer handles memory admission and execution-package validation.

## Notes / limitations

- Translations are plain text; inline markup inside a block (e.g. `<strong>`,
  `<a>`) is preserved in the **original** but rendered as plain text in the
  appended translation.
- `<pre>`/`<code>`/`<script>`/`<style>`/`<svg>`/`<head>` content is not translated.
- Table cells (`td`/`th`) and `<div>` wrappers are intentionally not translated
  (to avoid duplicating nested content and to keep XHTML valid).
- Translation batches use a conservative 8192-token input/output planning budget.
  Orinfer derives the output limit from the remaining model context, up to 8192
  output tokens. Context errors and truncated responses trigger bounded splitting
  and retries; incomplete translations are not cached as successful results.

### Shared implementation primitives

- Agent pairing uses the shared RustCrypto HMAC implementation in `src/auth.rs`.
  Its wire protocol remains `HMAC-SHA256(SHA256(token), nonce)`; verification uses
  the library's MAC comparison and token hashes use `subtle`.
- Axum request-part extraction authenticates Web users; an agent route middleware
  protects runtime/model/proxy endpoints while health and pairing remain public.
- `tokio-rusqlite` serializes database work on a dedicated thread. Existing SQL,
  transactions, WAL, ownership and cursor pagination remain authoritative.
- Markdown uses CommonMark events and original byte ranges. Code, link targets,
  reference definitions, HTML and leading metadata stay out of translation;
  translated text nodes are escaped and bilingual output duplicates whole blocks.
  Source whitespace and line endings round-trip when no translation is supplied.
- `backon` supplies exponential retry jitter, and `httpdate` handles HTTP-date
  Retry-After values. Server delays are a floor within an explicit 30-second
  worker-slot budget; the final cap applies after jitter. Error classification,
  cancellation and split/recovery rules remain translation-specific.
- `walkdir`, `rustix`, `strip-ansi-escapes`, `percent-encoding` and `tempfile`
  replace traversal, raw OS wrappers, terminal-sequence filtering, download-name
  encoding and temporary-file lifecycle code. Symlink rules, selection limits,
  atomic no-clobber saves and overwrite checks remain enforced by the application.
