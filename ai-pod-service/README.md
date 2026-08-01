# Ferryman AI Pod service

`scripts/build-release.sh` cross-builds the MicroServer Web binary and places
the native ARM64 `ferryman-agent` binary in this directory. The LPK ships that
binary with the Compose service, which mounts it into the official vLLM image.

The service expects these persistent model directories:

```text
${LZC_AGENT_DATA_DIR}/models/Hy-MT2-7B-FP8
${LZC_AGENT_DATA_DIR}/models/Hy-MT2-30B-A3B-FP8
```

Only the Rust controller remains resident. It starts one `vllm serve` child at
a time and unloads it after the final lease expires plus the idle timeout.
The controller fixes the FP8 KV cache at 8 GiB for 7B and 3 GiB for 30B so
Jetson UMA and zram activity cannot change cache capacity between launches.
