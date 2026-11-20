# Lazycat MicroServer Packaging & Integration

Ferryman ships as a single Lazycat MicroServer LPK (`cloud.lazycat.app.ferryman`) containing one embedded `ferryman-web` image built from `scratch` with its `x86_64` glibc runtime libraries, Vue 3 static assets, Noto/DejaVu fonts, CA certificates, and `nsswitch.conf`.

Inference runs on external OpenAI-compatible services or existing AI deployments—no AI Pod resource package, GPU runtime, or model weights are bundled.

For non-Lazycat Docker deployments (`docker/Dockerfile --target web`, compiled with `--no-default-features`), see [docs/deployment.md](deployment.md).

---

## Build Prerequisites

- Rustup matching [`rust-toolchain.toml`](../rust-toolchain.toml)
- Bun matching [`.bun-version`](../.bun-version)
- Python 3.10+ with `scripts/requirements-build.txt`
- `@lazycatcloud/lzc-cli` (installed via `bun install --frozen-lockfile`)
- `x86_64` cross-compiler, bundled fonts, and CA certificates (Debian/Ubuntu):

```sh
sudo apt-get install -y gcc-x86-64-linux-gnu fonts-noto-cjk fonts-noto-core fonts-dejavu-core ca-certificates
python3 -m pip install -r scripts/requirements-build.txt
bun --version  # must match .bun-version
bun install --frozen-lockfile
```

---

## Building & Verifying the LPK

[`scripts/build-release.sh`](../scripts/build-release.sh) compiles `ferryman-web` for `x86_64-unknown-linux-gnu` with `--no-default-features --features lazycat`, builds the frontend, stages runtime libraries, CA certificates, and bundled Noto/DejaVu fonts (including license files), generates `artifacts.json`, and atomically publishes `build/web`. Set `SOURCE_DATE_EPOCH` when building from a source archive without Git metadata.

### Remote MicroServer Build

```sh
bun --bun lzc-cli project lint .
bun --bun lzc-cli project release -o dist/ferryman-web.lpk
python3 scripts/check-artifacts.py lpk web dist/ferryman-web.lpk
```

### Local Buildx Assembly (No Connected MicroServer)

```sh
sh scripts/lzc-ci-release.sh -o dist/ferryman-web.lpk
python3 scripts/check-artifacts.py lpk web dist/ferryman-web.lpk

# Optional container smoke check against the staged scratch image:
docker build --platform linux/amd64 -f docker/Dockerfile.lzc -t ferryman-web:package-check .
python3 scripts/smoke-image.py web ferryman-web:package-check
```

Neither build command installs or upgrades an existing application on a MicroServer.

- **`scripts/check-artifacts.py`** verifies the single-image/service contract, package permissions, absence of AI Pod resources, ELF architecture, shared libraries, fonts, CA certificates, payload hashes, compressed OCI layers, and uncompressed diff IDs.
- **`scripts/lzc-ci-release.sh`** isolates known `@lazycatcloud/lzc-cli@2.0.9` local-builder fixes (`FROM scratch` base inspection and `tar.t` stream consumption) in a temporary CLI copy and builds with [`lzc-build.ci.yml`](../lzc-build.ci.yml). Normal remote builds do not need these patches.
- Toolchains and Docker base images are pinned, while host `apt` packages are not snapshot-pinned, so builds are not bit-for-bit reproducible.

---

## Platform Contract

Declared in [`package.yml`](../package.yml) and [`lzc-manifest.yml`](../lzc-manifest.yml):

- **Minimum OS**: LZCOS `1.5.2+` (required for canonical app delegation).
- **Required permissions**: `net.internet`, `net.lan`, `document.read`, `document.write`, `media.read`, `media.write`, `lzcapp.user_delegate`.
- **Mounts & paths**:
  - `/lzcapp/run/mnt/home` → `FERRYMAN_USER_DOCUMENTS_DIR` (one subdirectory per UID)
  - `/lzcapp/media/RemoteFS` → `FERRYMAN_REMOTE_FS_DIR` (one subdirectory per UID)
  - `/lzcapp/var/ferryman` → `FERRYMAN_DATA_DIR` (internal SQLite DB, jobs, and caches)

---

## Model Discovery & Delegated Authorization

Model discovery is compiled **only** with `--features lazycat` (enabled automatically by `scripts/build-release.sh`). When disabled, `GET /api/models` reports `discovery_enabled: false` and the UI hides discovery controls.

- **Endpoint**: defaults to `http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments`. Override via `FERRYMAN_MODEL_DISCOVERY_URL` or set it empty to disable discovery.
- **Delegated tickets**: `ferryman-web` captures the signed-in user's `X-HC-USER-TICKET` in memory for up to 15 minutes (never persisted to disk) and forwards it **only** to canonical `app.*.lzcx` ingress hosts. Tickets and browser cookies are never sent to inference APIs. Missing or expired authorization is surfaced in the model manager while custom APIs continue working. This follows the [Lazycat application delegation contract](https://developer.lazycat.cloud/advanced-app-interconnect.html).
- **Catalogue semantics**:
  - Prioritizes `category: translation` while allowing other chat models (`runtimeStatus: unknown` and null accessibility do not exclude a model; `stopped`/`offline`/`failed` records stay visible as unavailable).
  - Auto-selects the `index-translate` prompt adapter when the model ID starts with `index-translate-` (case-insensitive after the last `/`), and `generic` otherwise.
  - Partial refreshes preserve previously discovered records and display warnings.
- **Concurrency requirement**: discovered models require a positive integer at `/health.scheduler.max_active`. If `/health` does not report a valid limit, the model cannot start jobs until a refresh succeeds or the user configures a custom API with an explicit concurrency limit; Ferryman never guesses a fallback concurrency for discovered models.

### Administrator Debug Override

For administrator-assisted debugging on a MicroServer, `ferryman-web` (with `--features lazycat`) optionally reads `$FERRYMAN_DATA_DIR/discovery-api-auth.json`:

```json
{
  "uid": "<exact-signed-in-uid>",
  "url": "https://<discovery-host>/api/v1/model-deployments",
  "api_auth_token": "<lzc-api-auth-token>"
}
```

Security rules enforced by the backend:

- Provision with mode `0600` (never included in packages, responses, or inference requests).
- Requires an HTTPS URL without embedded credentials or fragments and matches only the exact bound `uid`; other users continue using normal delegation.
- Marks `Lzc-Api-Auth-Token` as sensitive, disables HTTP redirects, and strips browser cookies and delegation tickets.
- Removing the file restores normal delegation without restarting the app; revoke the token with `hc api_auth_token rm <token>` when finished.
