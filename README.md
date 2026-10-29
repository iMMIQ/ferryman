# Ferryman

> **Ferryman** — a multi-format translation tool (epub / pdf / txt / subtitles / …).
> *"渡船工": ferries content across the river between languages and formats.*
>
> **Status:** EPUB, DOCX, PDF, plain text, Markdown, SRT, VTT, ASS/SSA and LRC
> ship today. Ferryman can run as a CLI or as a Lazycat MicroServer + AI Pod Web app.

Produce a **bilingual (original + Chinese) EPUB** from any EPUB book by translating
its content through an OpenAI-compatible model. The original formatting is preserved
byte-for-byte; after each translated block a styled sibling carrying the
translation is inserted.

The Web application runs as a **single image on the MicroServer**. It discovers
existing model deployments or connects to user-configured OpenAI-compatible Chat
APIs. Ferryman does not deploy, download, start or stop models on the AI Pod.
Translation instructions are versioned: generic chat models use a general
translation prompt, and Index-Translate uses its official Chinese prompt and
instTrans structure constraints.

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
It configures a custom API against a local model fixture, exercises cancellation and deletion, and completes a translation task through the real Rust backend.

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

`ferryman-web` owns the UI, per-user model connections, uploads, document
processing, persistent SQLite job queue, translation requests, cancellation and
result downloads. Up to eight jobs run concurrently, with each connection's
per-task request limit. Jobs writing the same output remain serialized.
Interrupted jobs resume from their stored model selection and checkpoints.

Model discovery runs on the MicroServer through
`http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments`. The package declares
required `lzcapp.user_delegate`; the backend captures the signed-in user's
`X-HC-USER-TICKET` for up to 15 minutes in memory, and sends it only to canonical
`.lzcx` app ingress. Tickets and browser cookies are never forwarded to inference
APIs. Missing/expired authorization is shown in the model manager; custom APIs
continue working. This follows the platform's current
[application delegation contract](https://developer.lazycat.cloud/advanced-app-interconnect.html),
which may evolve toward an explicit authorization flow.

The model manager accepts an API base URL, optional API Key, model ID, prompt
adapter, served context length, output limit and request concurrency. The
“Read model list” action queries `/models` under the chosen base URL; optional
`/health` metadata supplies the served context limit. Models lacking that
metadata default conservatively to 2048 context tokens. Base URLs may include
`/v1`, a provider-specific path, or the full `/chat/completions` suffix.
Discovered models use the service's reported `/health.scheduler.max_active` as
the request concurrency limit. All jobs using the same API URL and model share
that limit, including retries. Existing jobs resolve the current limit when
starting. Missing or invalid limits remain unknown and require a refresh or an
explicitly configured custom API before starting a job; there is no guessed
concurrency fallback. Custom APIs accept any positive concurrency within the
runtime's supported range.

Discovery prioritizes `category: translation` while allowing other chat models;
`runtimeStatus: unknown` and nullable accessibility do not automatically exclude
a deployment. Stopped/offline records remain visible but unavailable. Partial
refreshes show warnings and preserve previously discovered records. Discovery
metadata is a candidate catalogue, not proof of Chat compatibility.

API Keys are stored in the backend's owner-scoped SQLite configuration and are
never returned by catalogue or job APIs. The database is mode `0600`. Job records
pin the model ID, base URL, adapter and token budgets; credentials are resolved
for the same owner and destination. Moving an API to another URL does not reuse
its saved key. Removing a connection or changing its destination requires
resubmitting affected tasks. Adapter identity participates in cache keys.

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
The creation page focuses on the current files and translation settings; history
has its own Tasks page. Selecting files replaces the large upload target with a
compact queue. Connection details, model parameters, and advanced options expand
on demand. Short transitions cover file changes, dialogs, navigation, and real
progress updates, and honor the system's reduced-motion preference.
On mobile, **New translation / Tasks** tabs keep the queue accessible and the
submit action stays above navigation. Uploads accept multiple files, show
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

Run locally:

```bash
npm ci && npm run build
mkdir -p ./ferryman-documents/local-development-user ./ferryman-remotefs/local-development-user
FERRYMAN_ALLOW_LOCAL_USER=true \
cargo run --bin ferryman-web
```

Open `http://127.0.0.1:8080`, then use **模型与 API** to add a model service.
The default build supports custom APIs only. Lazycat discovery and ticket
handling are compiled only with `--features lazycat`, which the LPK build script
enables automatically. The UI shows discovery controls only when the backend
reports that discovery is enabled.
On Lazycat, build only the MicroServer package:

```bash
sh scripts/build-release.sh
lzc-cli project lint .
lzc-cli project release -o dist/ferryman-web.lpk
```

LZCOS 1.5.2+ is required for canonical app delegation. There is no AI Pod resource
LPK or Agent image in this release. Existing self-deployment jobs keep their
history; unfinished tasks without an API selection report a migration message
and must be resubmitted against a configured model. Existing Ferryman Agent
source and CLI development helpers are retained for compatibility, outside the
Web deployment. See [packaging](docs/packaging.md).

### Standalone Docker deployment

The standalone image is built without Lazycat discovery, delegation tickets, or
discovery API Auth Token support. **模型与 API** only offers custom API
configuration; no discovery endpoint or discovery URL setting is available.

```bash
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

The stack runs one Web container and initializes its local library directories. It
requires no local GPU. Configure an external model API in the UI; `.env` controls
the Web port and persistent data/library paths. Leave discovery disabled unless
you have a reachable discovery service. Every visitor shares the
`local-development-user` identity in standalone mode. Multi-user installations
need an authenticating proxy injecting `safe_uid` and per-user library directories.

The CLI and Web share document parsing, translation retries, overflow recovery,
caching, checkpoints and result writing. The Web uses HTTP Chat requests;
translation batching combines document segments within a request and does not
require a JSONL server API.

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

## Notes / limitations

- Translations are plain text; inline markup inside a block (e.g. `<strong>`,
  `<a>`) is preserved in the **original** but rendered as plain text in the
  appended translation.
- `<pre>`/`<code>`/`<script>`/`<style>`/`<svg>`/`<head>` content is not translated.
- Table cells (`td`/`th`) and `<div>` wrappers are intentionally not translated
  (to avoid duplicating nested content and to keep XHTML valid).
- Web translation budgets follow the selected connection's served context and
  output limit. Configure these to match the actual service, rather than the
  model architecture's theoretical maximum. Context errors and truncated responses trigger bounded splitting
  and retries; incomplete translations are not cached as successful results.

### Shared implementation primitives

- Axum request-part extraction authenticates Web users; model sources, credentials
  and jobs are scoped to that identity.
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
