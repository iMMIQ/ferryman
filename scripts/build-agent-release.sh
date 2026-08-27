#!/bin/sh
set -eu

cargo build --release --bin ferryman-agent
mkdir -p build ai-pod-service/cloud.lazycat.aipod.ferryman/agxorin
cp target/release/ferryman-agent build/ferryman-agent
strip build/ferryman-agent
cp build/ferryman-agent ai-pod-service/cloud.lazycat.aipod.ferryman/agxorin/ferryman-agent
