# Packaging Ferryman

Ferryman ships one MicroServer LPK containing a single embedded `ferryman-web`
image, frontend assets, fonts, CA certificates and its glibc runtime. Models run
on existing external services. No AI Pod resource package, GPU runtime, model
weights or Agent image is built or installed.

## Build prerequisites

Use Rustup (version pinned by `rust-toolchain.toml`), the Node version in `.nvmrc`,
Python 3.10+, `@lazycatcloud/lzc-cli@2.0.9`, and the x86 compiler/runtime:

```sh
sudo apt-get install -y gcc-x86-64-linux-gnu fonts-noto-cjk fonts-noto-core fonts-dejavu-core ca-certificates
python3 -m pip install -r scripts/requirements-build.txt
nvm install && nvm use
npm install -g @lazycatcloud/lzc-cli@2.0.9
```

`build-release.sh` compiles `ferryman-web` for `x86_64-unknown-linux-gnu` with the
`lazycat` Cargo feature, builds
the frontend, bundles fonts and required runtime libraries, verifies all inputs,
and atomically publishes `build/web`. Cargo/npm caches remain reusable.
Set `SOURCE_DATE_EPOCH` when building from an archive without Git.

```sh
lzc-cli project release -o dist/ferryman-web.lpk
python3 scripts/check-artifacts.py lpk web dist/ferryman-web.lpk

# Local Buildx assembly without a connected MicroServer:
sh scripts/lzc-ci-release.sh -o dist/ferryman-web.lpk
```

Neither command installs or upgrades an existing application. The verifier
checks the single-image/service contract, package permissions, absence of AI Pod
resources, architecture, shared libraries, fonts, certificates, payload hashes,
compressed OCI layers and uncompressed diff IDs.

`lzc-ci-release.sh` retains the CLI 2.0.9 scratch-inspection and tar-consumer
workarounds in an isolated temporary CLI copy. The normal remote release path
does not need them. Build tools and base images are pinned; host apt inputs are
not snapshot-pinned, so builds are not bit-for-bit reproducible.

## Model discovery and authorization

The package requires LZCOS 1.5.2+ and LAN/Internet access for model APIs. It
declares required `lzcapp.user_delegate` for automatic discovery through
`app.cloud.lazycat.app.ai.lzcx`. `FERRYMAN_MODEL_DISCOVERY_URL` overrides the
endpoint; an empty value disables discovery. A user's ingress ticket is forwarded
only to canonical app ingress, kept in memory briefly, and never attached to
inference requests. No API auth token is generated during installation.

The current [platform delegation contract](https://developer.lazycat.cloud/advanced-app-interconnect.html)
does not guarantee ticket injection on every version. The model manager reports
missing authorization and keeps custom API configuration available. Real granted
cross-app discovery is an integration check on the MicroServer after upgrade.

For administrator-assisted debugging, the backend can read
`$FERRYMAN_DATA_DIR/discovery-api-auth.json` with fields `uid`, `url`, and
`api_auth_token`. Provision this file with mode `0600`; use an HTTPS discovery
URL and the exact UID bound to the API Auth Token. Only that signed-in user uses
this override; other users continue using normal delegation. The backend marks
the token as sensitive and sends it only to this URL, without forwarding browser
cookies or delegation tickets. Redirects are disabled. Neither the token nor the
file is included in browser responses, inference requests, or release packages.
Removing the file restores normal delegation without restarting the app; revoke
the token with `hc api_auth_token rm <token>` when debugging ends. Installation
does not create tokens or this configuration file.

## Standalone image

`docker/Dockerfile --target web` builds the application with a musl binary and
`--no-default-features`. This build omits the discovery endpoint, Lazycat ticket
capture, and discovery API Auth Token handling. It reports
`discovery_enabled: false`, so the UI only shows custom API configuration and
does not send discovery requests. Discovery URLs and debug credential files
cannot enable discovery in this build. Historical discovered connections are
excluded from model selection; custom connections remain available.
The Compose file needs no GPU and runs one Web service. Local library
directories are initialized by Web on startup:

```sh
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

Data and libraries persist in the configured host directories. Add model APIs
through the UI. API Keys are private backend configuration, not image contents.
The retained CLI/Agent development sources are not included in release images.
