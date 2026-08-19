//! Sizes the vLLM launch from the GPU that is actually present instead of the
//! 64 GiB Jetson the preset fractions were tuned on. The reference preset
//! values are kept verbatim whenever they fit; otherwise the utilization is
//! raised (up to a ceiling), then the KV cache is shrunk, then the preset is
//! rejected with the numbers that made it impossible.

use ferryman::preset::PresetConfig;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Launch parameters after adapting a preset to the detected hardware.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaunchProfile {
    pub gpu_memory_utilization: f32,
    /// `None` when the vLLM build does not accept `--kv-cache-memory-bytes`
    /// and the KV cache must be derived from the utilization budget instead.
    pub kv_cache_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct GpuMemory {
    pub total: u64,
    pub free: u64,
}

/// Non-tunable vLLM footprint (activations, CUDA context, graphs) assumed on
/// top of weights + KV cache. The reference preset budgets already contain
/// this much headroom on the Jetson they were tuned on.
const LAUNCH_OVERHEAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Utilization is quantized upwards to this step so the same machine keeps
/// producing identical flags across launches.
const UTILIZATION_STEP: f64 = 0.05;
/// Never reserve more than this fraction of the GPU; the rest stays for the
/// OS and other tenants.
const MAX_UTILIZATION: f64 = 0.90;
/// A preset that cannot keep at least this much KV cache is rejected instead
/// of being launched into a guaranteed out-of-memory start.
const MIN_VIABLE_KV_BYTES: u64 = 1024 * 1024 * 1024;
const KV_FLAG: &str = "kv-cache-memory-bytes";
const MIB_256: u64 = 256 * 1024 * 1024;
const DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn profile(utilization: f64, kv_bytes: u64, kv_flag_supported: bool) -> LaunchProfile {
    LaunchProfile {
        gpu_memory_utilization: utilization as f32,
        kv_cache_memory_bytes: kv_flag_supported.then_some(kv_bytes),
    }
}

fn quantize_up(ratio: f64) -> f64 {
    (ratio / UTILIZATION_STEP).ceil() * UTILIZATION_STEP
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

/// Derives the launch flags for one preset on one GPU. The decision depends on
/// *total* memory only, so a busy reference GPU keeps launching with the tuned
/// values instead of producing run-to-run different flags; transient free
/// memory only caps the branches that already have to grow or shrink.
pub fn derive_launch_profile(
    cfg: &PresetConfig,
    weights_bytes: u64,
    gpu: &GpuMemory,
    kv_flag_supported: bool,
) -> Result<LaunchProfile, String> {
    let kv_default = cfg.kv_cache_memory_bytes;
    let needed = weights_bytes + kv_default + LAUNCH_OVERHEAD_BYTES;
    let total = gpu.total as f64;

    if needed as f64 <= cfg.gpu_memory_utilization as f64 * total {
        return Ok(profile(cfg.gpu_memory_utilization as f64, kv_default, kv_flag_supported));
    }

    let ceiling = MAX_UTILIZATION.min(gpu.free as f64 / total);
    if needed as f64 <= ceiling * total {
        let ratio = needed as f64 / total;
        return Ok(profile(quantize_up(ratio).min(ceiling), kv_default, kv_flag_supported));
    }

    let budget = (total * ceiling) as u64;
    let kv = budget
        .saturating_sub(weights_bytes + LAUNCH_OVERHEAD_BYTES)
        / MIB_256
        * MIB_256;
    if kv < MIN_VIABLE_KV_BYTES {
        return Err(format!(
            "GPU has {:.1} GiB total ({:.1} GiB free); {} needs {:.1} GiB of weights plus KV cache and launch overhead",
            gib(gpu.total),
            gib(gpu.free),
            cfg.model_dir_name,
            gib(weights_bytes),
        ));
    }
    Ok(profile(ceiling, kv, kv_flag_supported))
}

/// Total and free device memory via the torch runtime of the vLLM image the
/// agent lives in. Returns `None` when CUDA is not usable from this process
/// (nvidia runtime missing, driver mismatch); callers fall back to the fixed
/// preset values.
pub async fn detect_gpu_memory() -> Option<GpuMemory> {
    let output = tokio::time::timeout(
        DETECT_TIMEOUT,
        Command::new("python3")
            .arg("-c")
            .arg("import torch;free,total=torch.cuda.mem_get_info();print(total,free)")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.split_whitespace();
    match (fields.next()?.parse::<u64>(), fields.next()?.parse::<u64>()) {
        (Ok(total), Ok(free)) if total > 0 && free <= total => Some(GpuMemory { total, free }),
        _ => None,
    }
}

/// Whether `vllm serve` accepts `--kv-cache-memory-bytes`. Newer builds group
/// flags per config (`--help=CacheConfig`), so both help forms are consulted.
/// Returns `None` when the probe itself failed; callers then assume the flag
/// exists (the behavior of the reference Jetson image).
async fn vllm_help(vllm_bin: &str, extra: &str) -> Option<String> {
    Command::new(vllm_bin)
        .args(["serve", extra])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn detect_kv_flag_supported(vllm_bin: &str) -> Option<bool> {
    if vllm_help(vllm_bin, "-h").await?.contains(KV_FLAG) {
        return Some(true);
    }
    let grouped = vllm_help(vllm_bin, "--help=CacheConfig").await;
    Some(grouped.map_or(true, |help| help.contains(KV_FLAG)))
}

/// Sum of the `*.safetensors` shards in a downloaded model directory; these
/// repos ship weights as sharded safetensors only. `None` when no shard is
/// found, so callers keep the fixed preset values.
pub async fn weights_bytes(model_path: &Path) -> Option<u64> {
    let mut entries = tokio::fs::read_dir(model_path).await.ok()?;
    let mut total = 0u64;
    let mut found = false;
    while let Some(entry) = entries.next_entry().await.ok()? {
        if entry.file_name().to_string_lossy().ends_with(".safetensors") {
            total += entry.metadata().await.ok()?.len();
            found = true;
        }
    }
    found.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferryman::preset::Preset;

    /// Measured on the AGX Orin 64 GB the presets were tuned on.
    const ORIN_TOTAL: u64 = 65_893_785_600;
    const GIB: u64 = 1024 * 1024 * 1024;

    fn gpu(total: u64, free: u64) -> GpuMemory {
        GpuMemory { total, free }
    }

    #[test]
    fn reference_gpu_keeps_tuned_values_verbatim() {
        let idle = gpu(ORIN_TOTAL, ORIN_TOTAL);
        let seven = Preset::SevenBFp8.config();
        let thirty = Preset::ThirtyBFp8.config();
        assert_eq!(
            derive_launch_profile(&seven, seven.download_bytes, &idle, true).unwrap(),
            LaunchProfile {
                gpu_memory_utilization: 0.30,
                kv_cache_memory_bytes: Some(8 * GIB),
            }
        );
        assert_eq!(
            derive_launch_profile(&thirty, thirty.download_bytes, &idle, true).unwrap(),
            LaunchProfile {
                gpu_memory_utilization: 0.55,
                kv_cache_memory_bytes: Some(3 * GIB),
            }
        );
    }

    #[test]
    fn busy_reference_gpu_keeps_tuned_values_verbatim() {
        // Determinism: the keep decision reads total memory only.
        let busy = gpu(ORIN_TOTAL, ORIN_TOTAL * 2 / 3);
        let seven = Preset::SevenBFp8.config();
        assert_eq!(
            derive_launch_profile(&seven, seven.download_bytes, &busy, true).unwrap(),
            LaunchProfile {
                gpu_memory_utilization: 0.30,
                kv_cache_memory_bytes: Some(8 * GIB),
            }
        );
    }

    #[test]
    fn card_that_fits_default_kv_raises_utilization() {
        // 24 GiB-class discrete card: 7B fits with the default KV, 30B cannot.
        let card = gpu(24_000_000_000, 24_000_000_000);
        let seven = Preset::SevenBFp8.config();
        let thirty = Preset::ThirtyBFp8.config();
        let raised = derive_launch_profile(&seven, seven.download_bytes, &card, true).unwrap();
        assert_eq!(raised.gpu_memory_utilization, 0.80);
        assert_eq!(raised.kv_cache_memory_bytes, Some(8 * GIB));
        assert!(derive_launch_profile(&thirty, thirty.download_bytes, &card, true).is_err());
    }

    #[test]
    fn small_card_shrinks_kv_at_the_ceiling() {
        let card = gpu(16 * GIB, 16 * GIB);
        let seven = Preset::SevenBFp8.config();
        let shrunk = derive_launch_profile(&seven, seven.download_bytes, &card, true).unwrap();
        assert_eq!(shrunk.gpu_memory_utilization, 0.90);
        let kv = shrunk.kv_cache_memory_bytes.unwrap();
        assert!(kv >= 4 * GIB && kv < 8 * GIB);
        assert_eq!(kv % MIB_256, 0);

        // Busy small card: the ceiling follows free memory.
        let busy = gpu(16 * GIB, 13 * GIB);
        let busy_profile =
            derive_launch_profile(&seven, seven.download_bytes, &busy, true).unwrap();
        assert_eq!(busy_profile.gpu_memory_utilization, 0.8125);
        assert_eq!(busy_profile.kv_cache_memory_bytes, Some(3_758_096_384));

        // Too busy to host the weights at all: rejected like a tiny GPU.
        let hostile = gpu(16 * GIB, 9 * GIB);
        assert!(
            derive_launch_profile(&seven, seven.download_bytes, &hostile, true).is_err()
        );
    }

    #[test]
    fn tiny_gpu_rejects_with_context() {
        let tiny = gpu(8 * GIB, 8 * GIB);
        let seven = Preset::SevenBFp8.config();
        let message = derive_launch_profile(&seven, seven.download_bytes, &tiny, true)
            .unwrap_err();
        assert!(message.contains("Hy-MT2-7B-FP8"), "{message}");
        assert!(message.contains("GiB"), "{message}");
    }

    #[test]
    fn unsupported_build_omits_the_kv_flag() {
        let idle = gpu(ORIN_TOTAL, ORIN_TOTAL);
        let seven = Preset::SevenBFp8.config();
        assert_eq!(
            derive_launch_profile(&seven, seven.download_bytes, &idle, false).unwrap(),
            LaunchProfile {
                gpu_memory_utilization: 0.30,
                kv_cache_memory_bytes: None,
            }
        );
    }
}
