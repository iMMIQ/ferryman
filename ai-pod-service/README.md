# Ferryman AI Pod service

`scripts/build-release.sh` cross-builds the MicroServer Web binary and places
the native ARM64 `ferryman-agent` binary in this directory. The LPK ships that
binary with the Compose service, which mounts it into the official vLLM image.

The controller creates and manages these persistent model directories:

```text
${LZC_AGENT_DATA_DIR}/models/Hy-MT2-7B-FP8
${LZC_AGENT_DATA_DIR}/models/Hy-MT2-30B-A3B-FP8
```

The `models` subdirectory is mounted read/write at `/models`; model files and
resumable `.partial` downloads stay there across app upgrades and AI Pod
restarts. Mounting the subdirectory also preserves compatibility with existing
installations that linked this location to a preloaded model library. Existing
complete directories are detected structurally and do not need to be downloaded
again. vLLM, Triton and TorchInductor caches are mounted separately from
`${LZC_AGENT_CACHE_DIR}/vllm` at `/root/.cache`; clearing that cache never removes
model weights.

The Web model manager can download from ModelScope, HF Mirror or Hugging Face.
Automatic mode benchmarks an 8 MiB range from a real model shard on every source,
caches the result for 24 hours, and prefers ModelScope when its throughput is
within 15% of the fastest source. Downloads support pause, HTTP Range resume,
source fallback and SHA-256 validation when the provider publishes hashes.

Only the Rust controller remains resident. It starts one `vllm serve` child at
a time and unloads it after the final lease expires plus the idle timeout.
The controller fixes the FP8 KV cache at 8 GiB for 7B and 3 GiB for 30B so
Jetson UMA and zram activity cannot change cache capacity between launches.
