# Packaging Ferryman

Ferryman has two independent Lazycat delivery products:

| Product | Compilation target | Contents | Execution location |
| --- | --- | --- | --- |
| Web LPK | `x86_64-unknown-linux-gnu` | scratch image with Web, assets, fonts, CA certificates and glibc runtime | MicroServer |
| AI Pod resource LPK | `aarch64-unknown-linux-gnu` | Compose, Agent, Orinfer 0.2.0 engine release, three installed Index execution packages, `config/aipod.yml` | AGX Orin AI Pod |

The Agent resource package requires **LZCOS 1.5.2 or newer**. Thor is not supported
by this release. The AI Pod uses a digest-pinned Debian runtime and the host's
NVIDIA CUDA driver. `FERRYMAN_RUNTIME_IMAGE` can select an already verified local
image for offline deployment; its default remains the pinned Debian release. The engine and Index 2B/9B/35B execution packages are bundled
in the resource LPK; weights are downloaded separately into
`${LZC_AGENT_DATA_DIR}/models`. Agent `/healthz` reports controller availability.

`bundle-orinfer.py` verifies the four archives against `orinfer-release.json`,
extracts the engine and runs `orinfer package install` during the build. Runtime
reads `/opt/orinfer/packages` directly from the read-only resource. The bundle
retains release archives, sources, licenses and checksums. Set
`ORINFER_RELEASE_DIR=/path/to/orinfer/artifacts/releases/v0.2.0` to use local
release assets; otherwise the builder downloads the exact GitHub release assets.
On x86 builders, `qemu-aarch64` and `/usr/aarch64-linux-gnu` run the ARM64 installer.

## Build prerequisites

Use a Linux development host with Rustup, the Node version in `.nvmrc`, Python
3.10+, and `@lazycatcloud/lzc-cli@2.0.9`. `rust-toolchain.toml` pins Rust. If a
Rustup mirror does not carry that version, use the official distribution server
with `RUSTUP_DIST_SERVER=https://static.rust-lang.org`.

On Ubuntu 22.04 (the release CI environment):

```sh
sudo apt-get update
sudo apt-get install -y gcc-x86-64-linux-gnu gcc-aarch64-linux-gnu qemu-user \
  fonts-noto-cjk fonts-noto-core fonts-dejavu-core ca-certificates
python3 -m pip install -r scripts/requirements-build.txt
nvm install
nvm use
npm install -g @lazycatcloud/lzc-cli@2.0.9
```

The Web build resolves runtime libraries using the selected compiler's
`-print-file-name`, supporting both native x86 and ARM64 cross builds. Font paths
are explicit Debian/Ubuntu package paths; all
seven fonts and their licenses are required. Missing runtime inputs fail before
compilation. Agent builds using a newer host/sysroot are rejected if they require
glibc symbols newer than the release baseline glibc 2.35.

Toolchains, lockfiles and container base digests are pinned. Host apt packages
are not snapshot-pinned, so this is **not a bit-for-bit reproducible build**.
The per-product `artifacts.json` records compiler version, architecture, shared
library requirements and SHA-256 hashes of every staged payload file.

## Build and validate Lazycat packages

```sh
# Standard release: buildscript runs here; embedded image assembly uses the
# configured MicroServer builder. Neither release command installs an app.
lzc-cli project release -o dist/ferryman-web.lpk
lzc-cli project release -f lzc-build.aipod.yml -o dist/ferryman-aipod.lpk
python3 scripts/check-artifacts.py lpk web dist/ferryman-web.lpk
python3 scripts/check-artifacts.py lpk aipod dist/ferryman-aipod.lpk

# CI / no connected MicroServer: same payload, local Buildx image assembly.
sh scripts/lzc-ci-release.sh -o dist/ferryman-web.lpk
```

For compilation without packaging, use `sh scripts/build-release.sh` for Web or
`sh scripts/build-agent-release.sh` for Agent. They do not build each other.
Web outputs go to `build/web/`; resource outputs go to `build/aipod/`. Each uses
a fresh temporary staging directory and replaces only its own output after
validation succeeds. Do not run two builds of the **same** product concurrently.
Cargo's `target/`, npm cache and `.lzc-cli-cache/` remain reusable. Never clean
those caches as part of ordinary release staging. `CARGO_TARGET_DIR` is supported.
Staged timestamps use `SOURCE_DATE_EPOCH`, or the latest Git commit timestamp,
keeping artifact timestamps stable across otherwise identical rebuilds.
Set `SOURCE_DATE_EPOCH` explicitly when building from a source archive without Git.

CLI 2.0.9's local builder incorrectly runs `docker image inspect scratch` while
looking for upstream layers. Its tar reader also auto-resumes entries before
the asynchronous gzip consumer is attached, which can truncate large layers.
`lzc-ci-release.sh` copies that exact CLI version to a temporary directory, fixes
the `scratch` guard and disables tar's auto-resume, then runs the official
packager with a separate package-cache version. It never edits the global
installation, and refuses unknown versions/source text. The verifier checks
both compressed layer digests and uncompressed OCI diff IDs to catch corruption.
Remove these workarounds after verifying upstream fixes.
The ordinary remote release path does not require it.

The AI resource source deliberately contains the `agxorin` and `config`
directories directly. The CLI exports these as resource IDs so the platform
projects them under `/lzcapp/run/resources/aipod/<package-id>/`. The nonexistent
`aipod-resource-only.yml` manifest path selects the CLI's resource-only mode;
do not add a placeholder manifest at that path.

The verifier reads tar/zip LPKs without extracting them into the workspace. It
checks package identity, minimum OS version, resource structure, executable
architecture/permissions, image layer hashes, fonts, certificates, shared
libraries and the artifact checksums. CI also starts the scratch Web image and
requests `/api/config` and the frontend. Real MicroServer installation, AI Pod
resource consumption and GPU inference remain hardware integration checks.

## Standalone Docker

`docker/Dockerfile` has `web` and `agent` targets, selected by the standalone
Compose file. It builds musl binaries as before. BuildKit mounts reuse Cargo and
npm caches; Cargo target caches are architecture-specific. Each target compiles
only its requested binary, and frontend/documentation changes do not invalidate
the Rust source stage. `docker/Dockerfile.dockerignore` limits this build context
to source inputs. The root `.dockerignore` preserves Lazycat staged artifacts.

```sh
docker compose -f docker-compose.standalone.yml build
# Or validate Web without configuring Compose secrets:
docker buildx build -f docker/Dockerfile --target web --load -t ferryman-web:check .
python3 scripts/smoke-image.py web ferryman-web:check
```

The Agent image targets ARM64 AGX Orin / SM87. Its build installs the same
fixed engine and execution packages as the AI Pod LPK. Ferryman starts and
stops its own model child process, which listens on container loopback
`127.0.0.1:18088`. The deployment publishes no inference port and never connects
to a host inference service. Model downloads persist under `/models`.

## Measure changes

Record cold and warm build duration separately (for example, `/usr/bin/time -p`
around the release command). Record final LPK bytes with `wc -c dist/*.lpk` and
image size with `docker image inspect`. Use an isolated builder/cache for cold
measurements instead of deleting the developer's caches. First-install network
bytes and warm-update bytes must be measured on the actual MicroServer/AI Pod:
LPK size alone excludes upstream images, model weights and existing layer caches.

Official references: [LPK mechanism](https://developer.lazycat.cloud/getting-started/lpk-how-it-works.html),
[build configuration](https://developer.lazycat.cloud/spec/build.html),
[resource projection](https://developer.lazycat.cloud/spec/resource-export.html),
[AI Pod applications](https://developer.lazycat.cloud/aipod/package/spec.html),
[Docker build cache](https://docs.docker.com/build/cache/optimize/).
