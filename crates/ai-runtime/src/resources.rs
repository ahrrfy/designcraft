//! Admission controls and observability for local inference. These do not
//! pretend to be an OS memory/VRAM sandbox around a separately owned server.
use designcraft_ai_types::{GpuSnapshot, LocalResourcePolicy, ModelSelection, ProviderKind, ResourceSnapshot};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static ACTIVE_GPU_ADAPTER: Mutex<Option<GpuSnapshot>> = Mutex::new(None);

/// Record an active graphics adapter hint (for example from `wgpu::AdapterInfo` at UI startup)
/// without introducing any L5 -> L6 crate dependency.
pub fn set_active_gpu_adapter(name: impl Into<String>, vram_mb: u64) {
    let trimmed = name.into().trim().to_string();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return;
    }
    let mut guard = ACTIVE_GPU_ADAPTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if vram_mb == 0
        && let Some(existing) = guard.as_ref()
        && existing.name.trim().eq_ignore_ascii_case(&trimmed)
        && existing.total_vram_mb > 0
    {
        return;
    }
    *guard = Some(GpuSnapshot { name: trimmed, total_vram_mb: vram_mb, free_vram_mb: 0 });
}

/// Clear any previously recorded active graphics adapter hint.
pub fn clear_active_gpu_adapter() {
    let mut guard = ACTIVE_GPU_ADAPTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

// A scoped unit-test input, never compiled into the application. Worker tests capture
// this explicit input when spawning; independent test threads cannot affect one another.
#[cfg(test)]
thread_local! {
    static TEST_ADMISSION_SNAPSHOT: std::cell::RefCell<Option<ResourceSnapshot>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestAdmissionGuard {
    previous: Option<ResourceSnapshot>,
    // Restoring a thread-local value must happen on the thread that installed it.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
impl TestAdmissionGuard {
    pub(crate) fn install(snapshot: ResourceSnapshot) -> Self {
        Self { previous: TEST_ADMISSION_SNAPSHOT.with(|current| current.replace(Some(snapshot))), _thread_bound: std::marker::PhantomData }
    }

    pub(crate) fn healthy() -> Self {
        Self::install(ResourceSnapshot { cpu_threads_available: 8, total_ram_mb: 32_768, available_ram_mb: 24_576, ..Default::default() })
    }
}

#[cfg(test)]
impl Drop for TestAdmissionGuard {
    fn drop(&mut self) {
        TEST_ADMISSION_SNAPSHOT.with(|current| {
            let _ = current.replace(self.previous.take());
        });
    }
}

#[cfg(test)]
pub(crate) fn test_admission_snapshot() -> Option<ResourceSnapshot> {
    TEST_ADMISSION_SNAPSHOT.with(|current| current.borrow().clone())
}

/// Held throughout one local inference request (including Comfy polling and any detached HTTP
/// worker unwinding after cancellation). One process-wide slot prevents concurrent local generation.
pub struct LocalRequestGuard {
    _lease: designcraft_ai_providers::LocalInferenceLease,
}

impl LocalRequestGuard {
    pub fn acquire(selection: &ModelSelection, policy: &LocalResourcePolicy) -> Result<Self, String> {
        policy.validate()?;
        if !selection.provider.is_local() {
            return Ok(Self { _lease: designcraft_ai_providers::LocalInferenceLease::inactive() });
        }
        if selection.provider == ProviderKind::Ollama && is_cloud_model(&selection.model) {
            return Err("Cloud-tagged Ollama models are disabled in local generation; choose a downloaded local model".into());
        }
        let snapshot = admission_memory_snapshot();
        admit_memory(policy, &snapshot)?;
        Ok(Self { _lease: designcraft_ai_providers::LocalInferenceLease::acquire()? })
    }
}

fn admission_memory_snapshot() -> ResourceSnapshot {
    #[cfg(test)]
    if let Some(snapshot) = test_admission_snapshot() {
        return snapshot;
    }
    memory_snapshot()
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
    let mut fallback_note = String::new();
    let nvidia_gpus = match probe_nvidia() {
        Ok(gpus) => gpus,
        Err(message) => {
            fallback_note = message;
            Vec::new()
        }
    };
    let platform_gpus = probe_platform_gpus(&nvidia_gpus, snapshot.total_ram_mb, snapshot.available_ram_mb);
    let merged = merge_gpu_snapshots(nvidia_gpus, platform_gpus);
    snapshot.gpus = merge_gpu_snapshots(merged, active_or_env_gpu_hints());
    if snapshot.gpus.is_empty() && !fallback_note.is_empty() {
        snapshot.message = fallback_note;
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

fn active_or_env_gpu_hints() -> Vec<GpuSnapshot> {
    let mut hints = Vec::new();
    if let Some(active) = ACTIVE_GPU_ADAPTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
        hints.push(active);
    }
    if let Ok(raw_name) = std::env::var("DESIGNCRAFT_GPU_ADAPTER") {
        let trimmed = raw_name.trim();
        if !trimmed.is_empty() && trimmed.len() <= 256 {
            let vram_mb = std::env::var("DESIGNCRAFT_GPU_VRAM_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
            hints.push(GpuSnapshot { name: trimmed.to_string(), total_vram_mb: vram_mb, free_vram_mb: 0 });
        }
    }
    hints
}

fn merge_gpu_snapshots(primary: Vec<GpuSnapshot>, secondary: Vec<GpuSnapshot>) -> Vec<GpuSnapshot> {
    let mut merged = Vec::new();
    for candidate in primary.into_iter().chain(secondary) {
        let trimmed = candidate.name.trim();
        if trimmed.is_empty() || trimmed.len() > 256 {
            continue;
        }
        if let Some(existing) = merged.iter_mut().find(|existing: &&mut GpuSnapshot| existing.name.trim().eq_ignore_ascii_case(trimmed)) {
            if existing.total_vram_mb == 0 && candidate.total_vram_mb > 0 {
                existing.total_vram_mb = candidate.total_vram_mb;
                existing.free_vram_mb = candidate.free_vram_mb.min(candidate.total_vram_mb);
            } else if existing.free_vram_mb == 0 && candidate.free_vram_mb > 0 && candidate.free_vram_mb <= existing.total_vram_mb {
                existing.free_vram_mb = candidate.free_vram_mb;
            }
            continue;
        }
        if merged.len() >= 8 {
            break;
        }
        merged.push(GpuSnapshot {
            name: trimmed.to_string(),
            total_vram_mb: candidate.total_vram_mb,
            free_vram_mb: candidate.free_vram_mb.min(candidate.total_vram_mb),
        });
    }
    merged
}

fn run_bounded_command(mut command: Command, timeout: Duration, max_bytes: usize, spawn_err: &str) -> Result<String, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|_| spawn_err.to_string())?;
    let stdout = child.stdout.take().ok_or("Missing GPU telemetry")?;
    let cap = u64::try_from(max_bytes).unwrap_or(65_536).saturating_add(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(cap).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("GPU telemetry command failed".into()),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("GPU telemetry timed out".into());
            }
        }
    }
    let bytes = reader.join().map_err(|_| "GPU telemetry reader failed")?.map_err(|_| "Cannot read GPU telemetry")?;
    if bytes.len() > max_bytes {
        return Err("GPU telemetry exceeds the supported size".into());
    }
    std::str::from_utf8(&bytes).map(str::to_owned).map_err(|_| "Invalid GPU telemetry encoding".into())
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
    command.args(["--query-gpu=name,memory.total,memory.free", "--format=csv,noheader,nounits"]);
    let text = run_bounded_command(
        command,
        Duration::from_secs(2),
        8192,
        "NVIDIA telemetry unavailable; GPU support may still be available through your local service",
    )?;
    parse_gpu_csv(&text)
}

#[cfg(target_os = "windows")]
fn probe_platform_gpus(_nvidia_gpus: &[GpuSnapshot], _total_ram_mb: u64, _available_ram_mb: u64) -> Vec<GpuSnapshot> {
    let Ok(system_root) = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).ok_or(()) else {
        return Vec::new();
    };
    let registry_gpus = probe_windows_registry_gpus(&system_root);
    let powershell = system_root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    let mut command = Command::new(powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Get-CimInstance Win32_VideoController | Select-Object Name, AdapterRAM | ConvertTo-Json -Compress",
    ]);
    let cim_gpus = run_bounded_command(command, Duration::from_secs(3), 32_768, "Windows GPU telemetry unavailable")
        .and_then(|json_text| parse_windows_cim_gpus(&json_text))
        .unwrap_or_default();
    merge_gpu_snapshots(registry_gpus, cim_gpus)
}

#[cfg(target_os = "windows")]
fn probe_windows_registry_gpus(system_root: &Path) -> Vec<GpuSnapshot> {
    let reg_exe = system_root.join("System32").join("reg.exe");
    let class_roots = [
        r"HKLM\SYSTEM\ControlSet001\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
    ];
    for class_root in class_roots {
        let mut list_cmd = Command::new(&reg_exe);
        list_cmd.args(["query", class_root]);
        let Ok(list_out) = run_bounded_command(list_cmd, Duration::from_secs(1), 16_384, "Windows registry GPU query unavailable") else {
            continue;
        };
        let subkeys = parse_windows_registry_subkeys(&list_out);
        if subkeys.is_empty() {
            continue;
        }
        let mut candidates = Vec::new();
        for subkey in subkeys.into_iter().take(8) {
            let mut desc_cmd = Command::new(&reg_exe);
            desc_cmd.args(["query", &subkey, "/v", "DriverDesc"]);
            let Ok(desc_out) = run_bounded_command(desc_cmd, Duration::from_millis(500), 4096, "Windows registry DriverDesc unavailable") else {
                continue;
            };
            let mut vram_cmd = Command::new(&reg_exe);
            vram_cmd.args(["query", &subkey, "/v", "HardwareInformation.qwMemorySize"]);
            let vram_out =
                run_bounded_command(vram_cmd, Duration::from_millis(500), 4096, "Windows registry qwMemorySize unavailable").unwrap_or_default();
            if let Some(gpu) = parse_windows_registry_gpu_entry(&desc_out, &vram_out) {
                candidates.push((is_software_or_virtual_adapter(&gpu.name), gpu));
            }
        }
        if !candidates.is_empty() {
            let has_real_gpu = candidates.iter().any(|(is_virtual, _)| !*is_virtual);
            let filtered = candidates.into_iter().filter(|(is_virtual, _)| !has_real_gpu || !*is_virtual).map(|(_, gpu)| gpu).collect();
            return merge_gpu_snapshots(filtered, Vec::new());
        }
    }
    Vec::new()
}

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn parse_windows_registry_subkeys(list_output: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for line in list_output.lines() {
        let trimmed = line.trim();
        let Some((_, tail)) = trimmed.rsplit_once('\\') else {
            continue;
        };
        if tail.len() == 4 && tail.bytes().all(|b| b.is_ascii_digit()) {
            let normalized =
                if let Some(rest) = trimmed.strip_prefix("HKEY_LOCAL_MACHINE\\") { format!("HKLM\\{rest}") } else { trimmed.to_string() };
            keys.push(normalized);
        }
    }
    keys
}

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn parse_windows_registry_gpu_entry(desc_output: &str, qw_vram_output: &str) -> Option<GpuSnapshot> {
    let mut name: Option<String> = None;
    for line in desc_output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("DriverDesc")
            && let Some((_, after_type)) = trimmed.split_once("REG_SZ")
        {
            let candidate = after_type.trim();
            if !candidate.is_empty() && candidate.len() <= 256 {
                name = Some(candidate.to_string());
                break;
            }
        }
    }
    let name = name?;
    let mut vram_bytes: u64 = 0;
    for line in qw_vram_output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("HardwareInformation.qwMemorySize") {
            continue;
        }
        if let Some((_, val_str)) = trimmed.split_once("REG_QWORD") {
            let raw = val_str.trim();
            let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).ok()
            } else {
                raw.parse::<u64>().ok()
            };
            if let Some(bytes) = parsed {
                vram_bytes = bytes;
                break;
            }
        } else if let Some((_, bin_str)) = trimmed.split_once("REG_BINARY") {
            let hex: String = bin_str.chars().filter(char::is_ascii_hexdigit).collect();
            if hex.len() == 16 {
                let mut le_bytes = [0u8; 8];
                let mut ok = true;
                for (idx, slot) in le_bytes.iter_mut().enumerate() {
                    let start = idx.saturating_mul(2);
                    if let Some(pair) = hex.get(start..start.saturating_add(2))
                        && let Ok(b) = u8::from_str_radix(pair, 16)
                    {
                        *slot = b;
                    } else {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    vram_bytes = u64::from_le_bytes(le_bytes);
                    break;
                }
            }
        }
    }
    Some(GpuSnapshot { name, total_vram_mb: vram_bytes / 1_048_576, free_vram_mb: 0 })
}

#[cfg(target_os = "macos")]
fn probe_platform_gpus(nvidia_gpus: &[GpuSnapshot], total_ram_mb: u64, available_ram_mb: u64) -> Vec<GpuSnapshot> {
    if !nvidia_gpus.is_empty() {
        return Vec::new();
    }
    let mut command = Command::new("/usr/sbin/system_profiler");
    command.args(["SPDisplaysDataType", "-json"]);
    run_bounded_command(command, Duration::from_secs(3), 65_536, "macOS GPU telemetry unavailable")
        .and_then(|json_text| parse_macos_system_profiler_gpus(&json_text, total_ram_mb, available_ram_mb))
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn probe_platform_gpus(nvidia_gpus: &[GpuSnapshot], _total_ram_mb: u64, _available_ram_mb: u64) -> Vec<GpuSnapshot> {
    if !nvidia_gpus.is_empty() {
        return Vec::new();
    }
    parse_linux_drm_gpus_from_dir(Path::new("/sys/class/drm"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn probe_platform_gpus(_nvidia_gpus: &[GpuSnapshot], _total_ram_mb: u64, _available_ram_mb: u64) -> Vec<GpuSnapshot> {
    Vec::new()
}

fn is_software_or_virtual_adapter(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("microsoft basic render")
        || lower.contains("microsoft basic display")
        || lower.contains("microsoft hyper-v video")
        || lower.contains("microsoft remote display")
        || lower.contains("remote desktop")
        || lower.contains("citrix indirect")
}

pub(crate) fn parse_windows_cim_gpus(json_text: &str) -> Result<Vec<GpuSnapshot>, String> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(trimmed).map_err(|_| "Invalid Windows GPU telemetry JSON")?;
    let items: Vec<&Value> = match &parsed {
        Value::Array(arr) => arr.iter().collect(),
        Value::Object(_) => vec![&parsed],
        _ => return Ok(Vec::new()),
    };
    let mut candidates = Vec::new();
    for item in items {
        let Some(name) = item.get("Name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 256) else {
            continue;
        };
        let total_vram_mb = item
            .get("AdapterRAM")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok())))
            .map(|bytes| bytes / 1_048_576)
            .unwrap_or(0);
        candidates.push((is_software_or_virtual_adapter(name), GpuSnapshot { name: name.to_string(), total_vram_mb, free_vram_mb: 0 }));
    }
    let has_real_gpu = candidates.iter().any(|(is_virtual, _)| !*is_virtual);
    let filtered = candidates.into_iter().filter(|(is_virtual, _)| !has_real_gpu || !*is_virtual).map(|(_, gpu)| gpu).collect();
    Ok(merge_gpu_snapshots(filtered, Vec::new()))
}

fn parse_memory_string_mb(raw: &str) -> Option<u64> {
    let lower = raw.trim().to_ascii_lowercase();
    let digits_end = lower.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(lower.len());
    if digits_end == 0 {
        return None;
    }
    let number: f64 = lower.get(..digits_end)?.trim().parse().ok()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    let unit = lower.get(digits_end..)?.trim();
    let mb = if unit.starts_with("gb") || unit.starts_with("gib") {
        number * 1024.0
    } else if unit.starts_with("mb") || unit.starts_with("mib") || unit.is_empty() {
        number
    } else {
        return None;
    };
    if mb > 0.0 && mb <= 1_048_576.0 { Some(mb.round() as u64) } else { None }
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn parse_macos_system_profiler_gpus(
    json_text: &str,
    host_total_ram_mb: u64,
    host_available_ram_mb: u64,
) -> Result<Vec<GpuSnapshot>, String> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(trimmed).map_err(|_| "Invalid macOS GPU telemetry JSON")?;
    let Some(items) = parsed.get("SPDisplaysDataType").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut gpus = Vec::new();
    for item in items {
        let Some(name) =
            item.get("sppci_model").or_else(|| item.get("_name")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 256)
        else {
            continue;
        };
        let explicit_vram =
            item.get("spdisplays_vram").or_else(|| item.get("spdisplays_vram_shared")).and_then(Value::as_str).and_then(parse_memory_string_mb);
        let is_apple_unified =
            name.to_ascii_lowercase().starts_with("apple ") || item.get("sppci_bus").and_then(Value::as_str) == Some("spdisplays_builtin");
        let (total_vram_mb, free_vram_mb) = match explicit_vram {
            Some(vram_mb) => (vram_mb, 0),
            None if is_apple_unified && host_total_ram_mb > 0 => (host_total_ram_mb, host_available_ram_mb.min(host_total_ram_mb)),
            None => (0, 0),
        };
        gpus.push(GpuSnapshot { name: name.to_string(), total_vram_mb, free_vram_mb });
    }
    Ok(merge_gpu_snapshots(gpus, Vec::new()))
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn parse_linux_drm_gpus_from_dir(drm_root: &Path) -> Vec<GpuSnapshot> {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return Vec::new();
    };
    let mut card_dirs: Vec<(String, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let digits = name.strip_prefix("card")?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((name, entry.path().join("device")))
        })
        .collect();
    card_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    let read_trimmed = |path: &Path| -> Option<String> {
        let file = std::fs::File::open(path).ok()?;
        let mut buf = String::new();
        file.take(256).read_to_string(&mut buf).ok()?;
        let trimmed = buf.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };

    let mut gpus = Vec::new();
    for (_card_name, dev_dir) in card_dirs {
        let total_bytes = read_trimmed(&dev_dir.join("mem_info_vram_total")).and_then(|s| s.parse::<u64>().ok());
        let used_bytes = read_trimmed(&dev_dir.join("mem_info_vram_used")).and_then(|s| s.parse::<u64>().ok());
        let vendor = read_trimmed(&dev_dir.join("vendor")).map(|v| v.to_ascii_lowercase());
        let device_id = read_trimmed(&dev_dir.join("device")).map(|d| d.to_ascii_lowercase());
        let explicit_name = read_trimmed(&dev_dir.join("product_name")).or_else(|| read_trimmed(&dev_dir.join("label")));
        let vendor_label = match vendor.as_deref() {
            Some("0x1002") => Some("AMD Radeon GPU"),
            Some("0x8086") => Some("Intel Graphics GPU"),
            Some("0x10de") => Some("NVIDIA GPU"),
            _ => None,
        };
        if total_bytes.is_none() && explicit_name.is_none() && vendor_label.is_none() {
            continue;
        }
        let name = explicit_name.unwrap_or_else(|| match (vendor_label, device_id.as_deref()) {
            (Some(v), Some(d)) => format!("{v} ({d})"),
            (Some(v), None) => v.to_string(),
            _ => "DRM Graphics Adapter".to_string(),
        });
        let total_vram_mb = total_bytes.map(|b| b / 1_048_576).unwrap_or(0);
        let free_vram_mb = match (total_bytes, used_bytes) {
            (Some(t), Some(u)) if t >= u => (t - u) / 1_048_576,
            _ => 0,
        };
        gpus.push(GpuSnapshot { name, total_vram_mb, free_vram_mb: free_vram_mb.min(total_vram_mb) });
    }
    merge_gpu_snapshots(gpus, Vec::new())
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
    fn synthetic_admission_input_is_scoped_and_nested_inputs_restore() {
        assert!(test_admission_snapshot().is_none());
        {
            let _healthy = TestAdmissionGuard::healthy();
            assert_eq!(test_admission_snapshot().unwrap().available_ram_mb, 24_576);
            assert!(std::thread::spawn(|| test_admission_snapshot().is_none()).join().unwrap(), "Unrelated threads must not inherit test telemetry");
            {
                let _low = TestAdmissionGuard::install(ResourceSnapshot { total_ram_mb: 32_768, available_ram_mb: 1024, ..Default::default() });
                assert_eq!(test_admission_snapshot().unwrap().available_ram_mb, 1024);
            }
            assert_eq!(test_admission_snapshot().unwrap().available_ram_mb, 24_576);
        }
        assert!(test_admission_snapshot().is_none());
    }

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

    #[test]
    fn multi_vendor_gpu_parsers_and_deduplication_handle_amd_intel_apple_and_adapter_hints() {
        // 1. Windows CIM single object and array JSON, filtering Microsoft Basic Render Driver when real GPUs exist.
        let win_array = r#"[
            {"Name": "AMD Radeon RX 7900 XTX", "AdapterRAM": 4293918720},
            {"Name": "Intel(R) Arc(TM) A770 Graphics", "AdapterRAM": 17179869184},
            {"Name": "Microsoft Basic Render Driver", "AdapterRAM": 0}
        ]"#;
        let win_gpus = parse_windows_cim_gpus(win_array).unwrap();
        assert_eq!(win_gpus.len(), 2);
        assert_eq!(win_gpus[0].name, "AMD Radeon RX 7900 XTX");
        assert_eq!(win_gpus[0].total_vram_mb, 4095);
        assert_eq!(win_gpus[1].name, "Intel(R) Arc(TM) A770 Graphics");
        assert_eq!(win_gpus[1].total_vram_mb, 16_384);

        let win_single = r#"{"Name": "Intel(R) Iris(R) Xe Graphics", "AdapterRAM": null}"#;
        let single_gpus = parse_windows_cim_gpus(win_single).unwrap();
        assert_eq!(single_gpus.len(), 1);
        assert_eq!(single_gpus[0].name, "Intel(R) Iris(R) Xe Graphics");
        assert_eq!(single_gpus[0].total_vram_mb, 0);

        // 2. macOS system_profiler SPDisplaysDataType (Apple Silicon unified memory + discrete Radeon).
        let mac_json = r#"{
            "SPDisplaysDataType": [
                {"sppci_model": "Apple M3 Max", "sppci_bus": "spdisplays_builtin"},
                {"sppci_model": "AMD Radeon Pro W6800X", "spdisplays_vram": "32 GB"}
            ]
        }"#;
        let mac_gpus = parse_macos_system_profiler_gpus(mac_json, 65_536, 48_000).unwrap();
        assert_eq!(mac_gpus.len(), 2);
        assert_eq!(mac_gpus[0].name, "Apple M3 Max");
        assert_eq!(mac_gpus[0].total_vram_mb, 65_536);
        assert_eq!(mac_gpus[0].free_vram_mb, 48_000);
        assert_eq!(mac_gpus[1].name, "AMD Radeon Pro W6800X");
        assert_eq!(mac_gpus[1].total_vram_mb, 32_768);

        // 3. Linux /sys/class/drm sysfs layout (card0 AMD with mem_info_vram_total/used, ignoring card0-DP-1 connector).
        let drm_dir = tempfile::tempdir().unwrap();
        let card0_dev = drm_dir.path().join("card0").join("device");
        std::fs::create_dir_all(&card0_dev).unwrap();
        std::fs::write(
            card0_dev.join("vendor"),
            "0x1002
",
        )
        .unwrap();
        std::fs::write(
            card0_dev.join("product_name"),
            "AMD Radeon RX 7800 XT
",
        )
        .unwrap();
        std::fs::write(
            card0_dev.join("mem_info_vram_total"),
            format!(
                "{}
",
                16_384_u64 * 1_048_576
            ),
        )
        .unwrap();
        std::fs::write(
            card0_dev.join("mem_info_vram_used"),
            format!(
                "{}
",
                4_096_u64 * 1_048_576
            ),
        )
        .unwrap();
        std::fs::create_dir_all(drm_dir.path().join("card0-DP-1")).unwrap();
        let linux_gpus = parse_linux_drm_gpus_from_dir(drm_dir.path());
        assert_eq!(linux_gpus.len(), 1);
        assert_eq!(linux_gpus[0].name, "AMD Radeon RX 7800 XT");
        assert_eq!(linux_gpus[0].total_vram_mb, 16_384);
        assert_eq!(linux_gpus[0].free_vram_mb, 12_288);

        // 4. Deduplication keeps richer nvidia-smi telemetry over zero-free fallback + honors active adapter hint.
        let nvidia = vec![GpuSnapshot { name: "NVIDIA GeForce RTX 4080".into(), total_vram_mb: 16_384, free_vram_mb: 14_000 }];
        let cim = vec![
            GpuSnapshot { name: "nvidia geforce rtx 4080".into(), total_vram_mb: 4095, free_vram_mb: 0 },
            GpuSnapshot { name: "AMD Radeon(TM) Graphics".into(), total_vram_mb: 512, free_vram_mb: 0 },
        ];
        let deduped = merge_gpu_snapshots(nvidia, cim);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].total_vram_mb, 16_384);
        assert_eq!(deduped[0].free_vram_mb, 14_000);
        assert_eq!(deduped[1].name, "AMD Radeon(TM) Graphics");

        set_active_gpu_adapter("Intel(R) Arc(TM) B580 Graphics", 12_288);
        let hints = active_or_env_gpu_hints();
        clear_active_gpu_adapter();
        assert!(hints.iter().any(|g| g.name == "Intel(R) Arc(TM) B580 Graphics" && g.total_vram_mb == 12_288));
    }

    #[test]
    fn cancelled_local_http_retains_request_guard_admission_until_worker_exits() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _serial = crate::worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _healthy = TestAdmissionGuard::healthy();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let release_stall = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let server_release = std::sync::Arc::clone(&release_stall);
        let server = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf);
                let (lock, cvar) = &*server_release;
                let guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = cvar.wait_timeout_while(guard, Duration::from_secs(10), |released| !*released);
            }
        });

        let policy = LocalResourcePolicy::default();
        let ollama = ModelSelection { provider: ProviderKind::Ollama, model: "llama3.2:latest".into(), endpoint: Some(endpoint) };
        let comfy = ModelSelection { provider: ProviderKind::ComfyUi, model: "sdxl.safetensors".into(), endpoint: None };
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_trigger = std::sync::Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            cancel_trigger.store(true, Ordering::Relaxed);
        });

        let req = designcraft_ai_providers::GenerationRequest {
            system: "sys".into(),
            prompt: "hello".into(),
            json_schema: serde_json::json!({"type": "object"}),
            max_output_tokens: 64,
            images: Vec::new(),
            resources: policy.clone(),
        };
        {
            let _guard = LocalRequestGuard::acquire(&ollama, &policy).unwrap();
            let err = designcraft_ai_providers::generate(&ollama, None, &req, &cancel).unwrap_err();
            assert_eq!(err, "AI request cancelled");
        }
        canceller.join().unwrap();

        // Even after LocalRequestGuard dropped on the cancelled caller, the process-wide local
        // admission slot must stay held while the detached HTTP worker is still blocked.
        let blocked = LocalRequestGuard::acquire(&comfy, &policy).err().expect("expected local admission to stay busy");
        assert!(blocked.contains("Another local inference is running"), "{blocked}");

        let (lock, cvar) = &*release_stall;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
        server.join().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let reacquired = loop {
            match LocalRequestGuard::acquire(&comfy, &policy) {
                Ok(guard) => break guard,
                Err(err) => {
                    assert!(Instant::now() < deadline, "local admission slot did not release after HTTP worker exited: {err}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        drop(reacquired);

        // Defense-in-depth: a cancelled non-inference HTTP worker (`origin_lease.is_none()`, same
        // branch taken by cloud Gemini/OpenAI calls) spawned while an outer LocalRequestGuard is
        // held on the same thread must NEVER inherit the local admission token.
        let probe_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let probe_endpoint = format!("http://{}/", probe_listener.local_addr().unwrap());
        let probe_release = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let probe_server_release = std::sync::Arc::clone(&probe_release);
        let probe_server = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = probe_listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
                let mut hello = [0u8; 1024];
                let _ = socket.read(&mut hello);
                let (lock, cvar) = &*probe_server_release;
                let guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = cvar.wait_timeout_while(guard, Duration::from_secs(10), |released| !*released);
            }
        });
        let probe_cancel = std::sync::Arc::new(AtomicBool::new(false));
        let probe_cancel_trigger = std::sync::Arc::clone(&probe_cancel);
        let probe_canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            probe_cancel_trigger.store(true, Ordering::Relaxed);
        });
        let discovery = ModelSelection { provider: ProviderKind::Ollama, model: "llama3.2:latest".into(), endpoint: Some(probe_endpoint) };
        {
            let _outer_local = LocalRequestGuard::acquire(&comfy, &policy).unwrap();
            let err = designcraft_ai_providers::list_models(&discovery, None, &probe_cancel).unwrap_err();
            assert_eq!(err, "AI request cancelled");
        }
        probe_canceller.join().unwrap();

        // While the non-inference HTTP socket is still stalled, local admission must already be free!
        let immediate_local = LocalRequestGuard::acquire(&comfy, &policy).expect("non-inference HTTP worker must not retain local admission");
        drop(immediate_local);

        let (lock, cvar) = &*probe_release;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
        probe_server.join().unwrap();
    }

    #[test]
    fn bug002_windows_registry_64bit_qword_and_binary_vram_outweighs_4gib_cim_cap() {
        let reg_list = "\
HKEY_LOCAL_MACHINE\\SYSTEM\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0000\n\
HKEY_LOCAL_MACHINE\\SYSTEM\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0001\n\
HKEY_LOCAL_MACHINE\\SYSTEM\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\Settings\n";
        let subkeys = parse_windows_registry_subkeys(reg_list);
        assert_eq!(subkeys.len(), 2);
        assert!(subkeys[0].ends_with("\\0000"));
        assert!(subkeys[1].ends_with("\\0001"));

        // 12 GiB = 12 * 1024^3 = 0x300000000 = 12,288 MB (previously capped at 4,095 MB via 32-bit AdapterRAM).
        let desc_12g = "HKEY_LOCAL_MACHINE\\...\\0000\n    DriverDesc    REG_SZ    NVIDIA GeForce RTX 4070 Ti\n";
        let qword_12g = "HKEY_LOCAL_MACHINE\\...\\0000\n    HardwareInformation.qwMemorySize    REG_QWORD    0x300000000\n";
        let gpu_qword = parse_windows_registry_gpu_entry(desc_12g, qword_12g).expect("parsed 12 GiB REG_QWORD GPU");
        assert_eq!(gpu_qword.name, "NVIDIA GeForce RTX 4070 Ti");
        assert_eq!(gpu_qword.total_vram_mb, 12_288);

        // 16 GiB = 0x400000000 encoded as 8-byte little-endian REG_BINARY: 00 00 00 00 04 00 00 00.
        let desc_16g = "HKEY_LOCAL_MACHINE\\...\\0001\n    DriverDesc    REG_SZ    AMD Radeon RX 7800 XT\n";
        let bin_16g = "HKEY_LOCAL_MACHINE\\...\\0001\n    HardwareInformation.qwMemorySize    REG_BINARY    0000000004000000\n";
        let gpu_bin = parse_windows_registry_gpu_entry(desc_16g, bin_16g).expect("parsed 16 GiB REG_BINARY GPU");
        assert_eq!(gpu_bin.name, "AMD Radeon RX 7800 XT");
        assert_eq!(gpu_bin.total_vram_mb, 16_384);

        // Merging 64-bit registry VRAM with 32-bit capped Win32_VideoController (0xFFFF_FFFF = 4095 MB) preserves 12,288 MB!
        let cim_capped = parse_windows_cim_gpus(r#"[{"Name":"NVIDIA GeForce RTX 4070 Ti","AdapterRAM":4294967295}]"#).unwrap();
        assert_eq!(cim_capped[0].total_vram_mb, 4095);
        let merged = merge_gpu_snapshots(vec![gpu_qword], cim_capped);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].total_vram_mb, 12_288);
    }
}
