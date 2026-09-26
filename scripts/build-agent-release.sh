#!/bin/sh
set -eu
. "$(dirname -- "$0")/build-common.sh"
PRODUCT=aipod
BINARY=ferryman-agent
RUST_TARGET=aarch64-unknown-linux-gnu
CROSS_PREFIX=aarch64-linux-gnu
init_build
cp -R ai-pod-service/cloud.lazycat.aipod.ferryman/config "$STAGE/config"
mkdir -p "$STAGE/agxorin"
cp ai-pod-service/cloud.lazycat.aipod.ferryman/agxorin/docker-compose.yml "$STAGE/agxorin/"
build_binary "$STAGE/agxorin/ferryman-agent"
python3 scripts/check-artifacts.py write aipod "$STAGE"
publish_stage
