//! Admission controls and observability for local inference. These do not
//! pretend to be an OS memory/VRAM sandbox around a separately owned server.
use designcraft_ai_types::{GpuSnapshot, LocalResourcePolicy, ModelSelection, ProviderKind, ResourceSnapshot};
use serde_json::Value;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static LOCAL_INFERENCE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Held throughout one local inference request (including Comfy polling).
/// One process-wide slot prevents concurrent local generation from profiles.
pub struct LocalRequestGuard {
    held: bool,
}
impl Drop for LocalRequestGuard {
    fn drop(&mut self) {
        if self.held {
            LOCAL_INFERENCE_ACTIVE.store(false, Ordering::Release);
        }
    }
}

impl LocalRequestGuard {
    pub fn acquire(selection: &ModelSelection, policy: &LocalResourcePolicy) -> Result<Self, String> {
        policy.validate()?;
        if !matches!(selection.provider, ProviderKind::Ollama | ProviderKind::ComfyUi) {
            return Ok(Self { held: false });
        }
        if selection.provider == ProviderKind::Ollama && is_cloud_model(&selection.model) {
            return Err("Cloud-tagged Ollama models are disabled in local generation; choose a downloaded local model".into());
        }
        let snapshot = memory_snapshot();
        admit_memory(policy, &snapshot)?;
        LOCAL_INFERENCE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Another local inference is running; wait before sending another request".to_string())?;
        Ok(Self { held: true })
    }
}

fn is_cloud_model(model: &str) -> bool {
    let name = model.to_ascii_lowercase();
    name.ends_with(":cloud") || name.ends_with("-cloud") || name.contains(":cloud-")
}

pub fn admit_memory(policy: &LocalResourcePolicy, host: &ResourceSnapshot) -> Result<(), String> {
    policy.validate()?;
    if host.total_ram_mb == 0 || host.available_ram_mb > host.total_ram_mb {
        return Err("Cannot verify available host memory; local generation was not started".into());
    }
    let used = host.total_ram_mb.saturating_sub(host.available_ram_mb);
    if host.available_ram_mb < u64::from(policy.min_free_ram_mb)
        || u128::from(used) * 100 > u128::from(host.total_ram_mb) * u128::from(policy.max_memory_percent)
    {
        return Err(format!(
            "Local generation paused for memory protection: {} MiB available; reserve {} MiB and maximum used {}%",
            host.available_ram_mb, policy.min_free_ram_mb, policy.max_memory_percent
        ));
    }
    Ok(())
}

fn memory_snapshot() -> ResourceSnapshot {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    ResourceSnapshot {
        cpu_threads_available: std::thread::available_parallelism().map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX)).unwrap_or(1),
        total_ram_mb: system.total_memory() / 1_048_576,
        available_ram_mb: system.available_memory() / 1_048_576,
        observed_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0),
        ..Default::default()
    }
}

/// Run on the background worker. No models/services are installed or stopped.
pub fn probe_host() -> ResourceSnapshot {
    let mut snapshot = memory_snapshot();
    match probe_nvidia() {
        Ok(gpus) => snapshot.gpus = gpus,
        Err(message) => snapshot.message = message,
    }
    let scope = "One local request at a time; CPU/context/image limits and RAM admission apply to this client. Shared services manage their own VRAM and other clients.";
    if snapshot.message.is_empty() {
        snapshot.message = scope.into();
    } else {
        snapshot.message.push_str(". ");
        snapshot.message.push_str(scope);
    }
    snapshot
}

fn probe_nvidia() -> Result<Vec<GpuSnapshot>, String> {
    #[cfg(windows)]
    let executable = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .ok_or("Windows system directory unavailable")?
        .join("System32")
        .join("nvidia-smi.exe");
    #[cfg(not(windows))]
    let executable = std::path::PathBuf::from("/usr/bin/nvidia-smi");
    let mut command = Command::new(executable);
    command
        .args(["--query-gpu=name,memory.total,memory.free", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child =
        command.spawn().map_err(|_| "NVIDIA telemetry unavailable; GPU support may still be available through your local service".to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("NVIDIA telemetry command failed".into()),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("NVIDIA telemetry timed out".into());
            }
        }
    }
    let mut bytes = Vec::new();
    child.stdout.take().ok_or("Missing GPU telemetry")?.take(8193).read_to_end(&mut bytes).map_err(|_| "Cannot read GPU telemetry")?;
    if bytes.len() > 8192 {
        return Err("GPU telemetry exceeds the supported size".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid GPU telemetry encoding")?;
    parse_gpu_csv(text)
}

fn parse_gpu_csv(text: &str) -> Result<Vec<GpuSnapshot>, String> {
    let mut result = Vec::new();
    for row in text.lines().filter(|row| !row.trim().is_empty()) {
        if result.len() >= 8 {
            return Err("More than eight GPUs are not supported by the resource panel".into());
        }
        let mut fields = row.rsplitn(3, ',').map(str::trim);
        let free = fields.next().ok_or("Invalid GPU free memory")?.parse::<u64>().map_err(|_| "Invalid GPU free memory")?;
        let total = fields.next().ok_or("Invalid GPU total memory")?.parse::<u64>().map_err(|_| "Invalid GPU total memory")?;
        let name = fields.next().ok_or("Invalid GPU name")?;
        if name.is_empty() || name.len() > 256 || total == 0 || free > total {
            return Err("Inconsistent GPU telemetry".into());
        }
        result.push(GpuSnapshot { name: name.into(), total_vram_mb: total, free_vram_mb: free });
    }
    Ok(result)
}

/// Reject oversized known allocation inputs before submitting a Comfy job.
/// Custom nodes can have additional allocations; the service is not sandboxed.
pub fn check_comfy_workflow(workflow: &Value, policy: &LocalResourcePolicy) -> Result<(), String> {
    policy.validate()?;
    let nodes = workflow.as_object().ok_or("Comfy workflow must be an API-format object")?;
    if nodes.len() > 128 {
        return Err("Comfy workflow exceeds 128 nodes".into());
    }
    let mut sampling_steps = 0u64;
    for node in nodes.values() {
        if let Some(inputs) = node.get("inputs").and_then(Value::as_object) {
            if let Some(value) = inputs.get("steps") {
                let steps = value.as_u64().filter(|n| (1..=100).contains(n)).ok_or("Comfy sampling steps must be a literal number from 1 to 100")?;
                sampling_steps = sampling_steps.saturating_add(steps);
                if sampling_steps > 200 {
                    return Err("Comfy workflow exceeds 200 combined sampling steps".into());
                }
            }
            for name in ["width", "height"] {
                if let Some(value) = inputs.get(name) {
                    let size = value.as_u64().ok_or("Resource guard requires literal width/height values in Comfy workflows")?;
                    if size == 0 || size > u64::from(policy.max_image_edge) {
                        return Err(format!("Comfy {name} exceeds the configured image limit"));
                    }
                }
            }
            if let Some(value) = inputs.get("batch_size")
                && value.as_u64() != Some(1)
            {
                return Err("Comfy image batches must contain exactly one image".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_ram_pressure_missing_telemetry_and_arithmetic_overflow() {
        let policy = LocalResourcePolicy::default();
        let host = ResourceSnapshot { total_ram_mb: 32_768, available_ram_mb: 8192, ..Default::default() };
        assert!(admit_memory(&policy, &host).is_ok());
        assert!(admit_memory(&policy, &ResourceSnapshot { available_ram_mb: 1024, ..host.clone() }).is_err());
        assert!(admit_memory(&policy, &ResourceSnapshot { available_ram_mb: 3000, ..host }).is_err());
        assert!(admit_memory(&policy, &ResourceSnapshot::default()).is_err());
        assert!(admit_memory(&policy, &ResourceSnapshot { total_ram_mb: 1, available_ram_mb: u64::MAX, ..Default::default() }).is_err());
    }

    #[test]
    fn gpu_csv_validates_instead_of_inventing_memory() {
        let parsed = parse_gpu_csv("NVIDIA GeForce RTX 3050, 8192, 6123\n").unwrap();
        assert_eq!(parsed[0].total_vram_mb, 8192);
        assert_eq!(parsed[0].free_vram_mb, 6123);
        assert!(parse_gpu_csv("GPU, 100, 101").is_err());
        assert!(parse_gpu_csv("GPU, N/A, N/A").is_err());
    }

    #[test]
    fn comfy_allocations_and_cloud_tags_are_guarded() {
        let policy = LocalResourcePolicy::default();
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"width":1024,"height":1024,"batch_size":1}}}), &policy).is_ok());
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"width":8192}}}), &policy).is_err());
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"batch_size":2}}}), &policy).is_err());
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"width":["2",0]}}}), &policy).is_err());
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"steps":1_000_000}}}), &policy).is_err());
        assert!(check_comfy_workflow(&json!({"1":{"inputs":{"steps":["2",0]}}}), &policy).is_err());
        assert!(
            check_comfy_workflow(&json!({"1":{"inputs":{"steps":100}},"2":{"inputs":{"steps":100}},"3":{"inputs":{"steps":1}}}), &policy).is_err()
        );
        assert!(is_cloud_model("qwen:cloud"));
        assert!(is_cloud_model("qwen-cloud"));
        assert!(!is_cloud_model("qwen:8b"));
    }
}
