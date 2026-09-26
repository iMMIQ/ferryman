#!/bin/sh
# Isolate known lzc-cli 2.0.9 local-builder fixes from the global CLI.
set -eu
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$REPO_ROOT"
mkdir -p build dist
CLI_SOURCE="$(npm root -g)/@lazycatcloud/lzc-cli"
CLI_STAGE=$(mktemp -d "$REPO_ROOT/build/.lzc-ci.XXXXXX")
trap 'rm -rf "$CLI_STAGE"' EXIT
trap 'exit 1' HUP INT TERM
python3 - "$CLI_SOURCE" "$CLI_STAGE" <<'PY'
import json
from pathlib import Path
import shutil
import sys

source, stage = map(Path, sys.argv[1:])
version = json.loads((source / 'package.json').read_text())['version']
if version != '2.0.9':
    raise SystemExit('This compatibility fix requires @lazycatcloud/lzc-cli@2.0.9; revalidate on upgrades')
shutil.copytree(source, stage, dirs_exist_ok=True, ignore=shutil.ignore_patterns('node_modules'))
(stage / 'node_modules').symlink_to(source / 'node_modules', target_is_directory=True)
module = stage / 'lib/app/lpk_build_images_local.js'
text = module.read_text()
old = "const baseRef = resolveFinalExternalImageFromDockerfile(dockerfilePath);\n\tif (!baseRef) {"
new = "const baseRef = resolveFinalExternalImageFromDockerfile(dockerfilePath);\n\tif (!baseRef || baseRef.toLowerCase() === 'scratch') {"
if text.count(old) != 1:
    raise SystemExit('CLI scratch compatibility patch no longer matches; inspect upstream before changing it')
module.write_text(text.replace(old, new))
# tar.t otherwise resumes each entry before the async gzip probe/consumer is
# attached, dropping chunks from large image layers while still producing a
# valid gzip checksum. Let the existing consumers control the entry streams.
module = stage / 'lib/app/lpk_build_images_pack_local.js'
text = module.read_text()
old = 'await tar.t({\n\t\tfile: archivePath,'
new = 'await tar.t({\n\t\tnoResume: true,\n\t\tfile: archivePath,'
if text.count(old) != 1:
    raise SystemExit('CLI archive-stream compatibility patch no longer matches')
module.write_text(text.replace(old, new))
# Do not reuse OCI packages produced before the stream fix. Keep ordinary
# remote-builder caches intact; this only namespaces the isolated CLI's cache.
module = stage / 'lib/app/lpk_build_images.js'
text = module.read_text()
old = 'const IMAGE_PACKAGE_CACHE_VERSION = 1;'
new = "const IMAGE_PACKAGE_CACHE_VERSION = 'ferryman-ci-2.0.9-stream-fix';"
if text.count(old) != 1:
    raise SystemExit('CLI package-cache compatibility patch no longer matches')
module.write_text(text.replace(old, new))
PY
node "$CLI_STAGE/scripts/cli.js" project release -f lzc-build.ci.yml "$@"
