//! Launcher preview update state, independent of AI requests and document state.

use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::{Instant, SystemTime};

use serde_json::Value;
#[cfg(target_os = "windows")]
use serde_json::json;

#[cfg(any(target_os = "windows", test))]
const MAX_MANIFEST: u64 = 65_536;
#[cfg(any(target_os = "windows", test))]
const MAX_OUTPUT: u64 = 65_536;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(target_os = "windows")]
type Completion = Arc<Mutex<Option<Result<UpdateResult, String>>>>;

#[derive(Clone, Debug)]
struct Manifest {
    active_commit: String,
    staged_commit: Option<String>,
    status: String,
    #[cfg(target_os = "windows")]
    script: PathBuf,
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn short_commit(value: &str) -> &str {
    if valid_commit(value) { value.get(..8).unwrap_or("unknown") } else { "unknown" }
}

#[cfg(any(target_os = "windows", test))]
fn absolute_path(value: &str) -> bool {
    !value.contains(['\0', '\r', '\n']) && std::path::Path::new(value).is_absolute()
}

#[cfg(any(target_os = "windows", test))]
fn known_status(value: &str) -> bool {
    matches!(value, "up_to_date" | "no_published_update" | "staged_waiting_for_app_close" | "installed_for_next_launch")
}

#[cfg(any(target_os = "windows", test))]
fn valid_ci_url(value: &str) -> bool {
    if let Some(run) = value.strip_prefix("https://github.com/ahrrfy/designcraft/actions/runs/") {
        !run.is_empty() && run.bytes().all(|byte| byte.is_ascii_digit())
    } else if let Some(sha) = value.strip_prefix("https://github.com/ahrrfy/designcraft/commit/") {
        valid_commit(sha)
    } else {
        false
    }
}

impl Manifest {
    #[cfg(any(target_os = "windows", test))]
    fn parse(source: &str) -> Result<Self, String> {
        if source.len() as u64 > MAX_MANIFEST {
            return Err("Launcher manifest exceeds 64 KiB.".into());
        }
        let value: Value =
            serde_json::from_str(source.trim_start_matches('\u{feff}')).map_err(|error| format!("Invalid launcher manifest: {error}"))?;
        let active_commit = value
            .get("published_source_commit")
            .and_then(Value::as_str)
            .filter(|value| valid_commit(value))
            .ok_or("Launcher active commit is missing or invalid.")?
            .to_owned();
        let _script = value
            .get("update_script_path")
            .and_then(Value::as_str)
            .filter(|value| absolute_path(value))
            .ok_or("Launcher updater path is missing or not absolute.")?;
        let status = match value.get("update_status") {
            None | Some(Value::Null) => "not_checked",
            Some(Value::String(value)) if known_status(value) => value,
            _ => return Err("Launcher update status is invalid.".into()),
        }
        .to_owned();
        let staged_commit = if status == "staged_waiting_for_app_close" {
            let commit = value
                .get("staged_source_commit")
                .and_then(Value::as_str)
                .filter(|value| valid_commit(value))
                .ok_or("Staged update commit is missing or invalid.")?;
            if !value.get("staged_executable").and_then(Value::as_str).is_some_and(absolute_path) {
                return Err("Staged update executable is missing or not absolute.".into());
            }
            Some(commit.to_owned())
        } else {
            None
        };
        Ok(Self {
            active_commit,
            staged_commit,
            status,
            #[cfg(target_os = "windows")]
            script: PathBuf::from(_script),
        })
    }

    fn staged(&self) -> bool {
        self.status == "staged_waiting_for_app_close"
    }
}

#[derive(Clone, Debug)]
struct UpdateResult {
    status: String,
    commit: Option<String>,
}

#[cfg(any(target_os = "windows", test))]
impl UpdateResult {
    fn parse(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<Self, String> {
        if !success {
            let detail: String = String::from_utf8_lossy(stderr).chars().filter(|ch| !ch.is_control() || *ch == ' ').take(1000).collect();
            return Err(format!("Update check failed. {detail}"));
        }
        if stdout.len() as u64 > MAX_OUTPUT || stderr.len() as u64 > MAX_OUTPUT {
            return Err("Updater output exceeds 64 KiB.".into());
        }
        let value: Value = serde_json::from_slice(stdout).map_err(|error| format!("Invalid updater result: {error}"))?;
        let object = value.as_object().ok_or("Updater result must be one JSON object.")?;
        if object.len() != 4 || !["status", "selectedCommit", "targetExe", "ciUrl"].iter().all(|key| object.contains_key(*key)) {
            return Err("Updater result fields are invalid.".into());
        }
        let status = value.get("status").and_then(Value::as_str).filter(|value| known_status(value)).ok_or("Updater status is invalid.")?;
        let optional = |key| -> Result<Option<&str>, String> {
            match value.get(key) {
                Some(Value::Null) => Ok(None),
                Some(Value::String(value)) => Ok(Some(value)),
                _ => Err(format!("Updater {key} is invalid.")),
            }
        };
        let commit = optional("selectedCommit")?;
        if commit.is_some_and(|value| !valid_commit(value)) {
            return Err("Updater commit is invalid.".into());
        }
        let target = optional("targetExe")?;
        if target.is_some_and(|value| !absolute_path(value)) {
            return Err("Updater executable is not absolute.".into());
        }
        if optional("ciUrl")?.is_some_and(|value| !valid_ci_url(value)) {
            return Err("Updater CI proof URL is invalid.".into());
        }
        if matches!(status, "staged_waiting_for_app_close" | "installed_for_next_launch") && (commit.is_none() || target.is_none()) {
            return Err("Updater omitted staged update identity.".into());
        }
        Ok(Self { status: status.into(), commit: commit.map(str::to_owned) })
    }
}

#[derive(Clone, Default)]
pub(crate) struct PreviewUpdate {
    manifest_path: Option<PathBuf>,
    manifest: Option<Manifest>,
    #[cfg(target_os = "windows")]
    fingerprint: Option<(SystemTime, u64)>,
    #[cfg(target_os = "windows")]
    last_poll: Option<Instant>,
    #[cfg(target_os = "windows")]
    initialized: bool,
    running: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    completion: Completion,
    result: Option<UpdateResult>,
    error: Option<String>,
    manifest_error: Option<String>,
}

impl PreviewUpdate {
    #[cfg(any(target_os = "windows", test))]
    fn reserve(&self) -> Result<(), String> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "An update check is already running.".into())
    }

    pub(crate) fn enabled(&self) -> bool {
        cfg!(target_os = "windows") && !self.running.load(Ordering::Acquire) && self.manifest_path.is_some() && self.manifest.is_some()
    }

    // std::time::Instant is unsupported by the browser's std-only WASM target.
    // Unsupported hosts never read launcher files, inspect workers or consult a clock.
    #[cfg(not(target_os = "windows"))]
    fn poll(&mut self) {}

    #[cfg(target_os = "windows")]
    fn poll(&mut self) {
        let completed = self.completion.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(completed) = completed {
            self.running.store(false, Ordering::Release);
            match completed {
                Ok(result) => {
                    self.result = Some(result);
                    self.error = None;
                }
                Err(error) => {
                    self.error = Some(error);
                    self.result = None;
                }
            }
            self.last_poll = None;
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.fingerprint = None;
            }
        }
        if self.last_poll.is_some_and(|last| last.elapsed() < POLL_INTERVAL) {
            return;
        }
        self.last_poll = Some(Instant::now());
        if !self.initialized {
            self.initialized = true;
            #[cfg(target_os = "windows")]
            {
                self.manifest_path = std::env::var_os("DESIGNCRAFT_PREVIEW_MANIFEST").map(PathBuf::from).filter(|path| path.is_absolute());
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = &self.manifest_path {
            let loaded = (|| -> Result<Option<Manifest>, String> {
                let metadata = designcraft_engine::io::metadata(path).map_err(|error| format!("Cannot inspect launcher manifest: {error}"))?;
                if metadata.len() > MAX_MANIFEST {
                    return Err("Launcher manifest exceeds 64 KiB.".into());
                }
                let fingerprint = (metadata.modified().map_err(|error| error.to_string())?, metadata.len());
                if self.fingerprint == Some(fingerprint) {
                    return Ok(None);
                }
                let bytes =
                    designcraft_engine::io::read_capped(path, MAX_MANIFEST).map_err(|error| format!("Cannot read launcher manifest: {error}"))?;
                let source = String::from_utf8(bytes).map_err(|error| format!("Launcher manifest is not UTF-8: {error}"))?;
                let manifest = Manifest::parse(&source)?;
                self.fingerprint = Some(fingerprint);
                Ok(Some(manifest))
            })();
            match loaded {
                Ok(Some(manifest)) => {
                    self.manifest = Some(manifest);
                    self.manifest_error = None;
                }
                Ok(None) => {}
                Err(error) => {
                    self.manifest = None;
                    self.manifest_error = Some(error);
                    self.result = None;
                }
            }
        }
    }

    pub(crate) fn command(&mut self, params: &Value) -> Result<Value, String> {
        if designcraft_engine::file_access::current().is_some() {
            return Err("Preview updates are unavailable under automation confinement; use the unrestricted desktop launcher.".into());
        }
        if !params.as_object().is_some_and(serde_json::Map::is_empty) {
            return Err("Check preview update accepts only {}. Script paths and URLs cannot be supplied.".into());
        }
        self.poll();
        #[cfg(not(target_os = "windows"))]
        {
            Err("Managed preview updates are supported by the Windows launcher only.".into())
        }
        #[cfg(target_os = "windows")]
        {
            let manifest = self
                .manifest
                .clone()
                .ok_or_else(|| self.manifest_error.clone().unwrap_or_else(|| "Launch through the preview launcher to enable updates.".into()))?;
            let path = self.manifest_path.clone().ok_or("Launcher manifest is unavailable.")?;
            self.reserve()?;
            let completion = self.completion.clone();
            let worker = std::thread::Builder::new().name("preview-update".into()).spawn(move || {
                let result = execute_updater(&path, &manifest.script);
                *completion.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            });
            if let Err(error) = worker {
                self.running.store(false, Ordering::Release);
                return Err(format!("Cannot start updater worker: {error}"));
            }
            self.error = None;
            self.result = None;
            Ok(json!({"status": "checking"}))
        }
    }

    fn staged_commit(&self) -> Option<&str> {
        self.manifest.as_ref().filter(|manifest| manifest.staged()).and_then(|manifest| manifest.staged_commit.as_deref()).or_else(|| {
            self.result.as_ref().filter(|result| result.status == "staged_waiting_for_app_close").and_then(|result| result.commit.as_deref())
        })
    }

    /// Called only by the visible panel; no AI polling or background manifest watcher.
    pub(crate) fn show(&mut self, ui: &mut egui::Ui, arabic: bool) -> bool {
        self.poll();
        ui.ctx().request_repaint_after(POLL_INTERVAL);
        let tokens = crate::theme::Tokens::get(ui.ctx());
        let text = |english, arabic_text| if arabic { arabic_text } else { english };
        let mut check = false;
        egui::Frame::new().stroke(egui::Stroke::new(1.0, tokens.field_border)).inner_margin(4.0).show(ui, |ui| {
            let active = self.manifest.as_ref().map_or("unknown", |manifest| short_commit(&manifest.active_commit));
            let checking = self.running.load(Ordering::Acquire);
            let status = if checking {
                "checking"
            } else if self.error.is_some() || self.manifest_error.is_some() {
                "error"
            } else if self.manifest.as_ref().is_some_and(Manifest::staged) {
                "staged_waiting_for_app_close"
            } else {
                self.result
                    .as_ref()
                    .map(|result| result.status.as_str())
                    .or_else(|| self.manifest.as_ref().map(|manifest| manifest.status.as_str()))
                    .unwrap_or("unavailable")
            };
            ui.horizontal_wrapped(|ui| {
                crate::rtl::label(ui, format!("{} v{} · {active}", text("Updates", "التحديثات"), env!("CARGO_PKG_VERSION")))
                    .on_hover_text(text("Updates and version · active source commit", "التحديثات والإصدار · معرّف المصدر النشط"));
                crate::rtl::label(
                    ui,
                    match status {
                        "checking" => text("Checking…", "جارٍ الفحص…"),
                        "up_to_date" => text("Up to date", "محدّث"),
                        "no_published_update" => text("No published update", "لا تحديث منشور"),
                        "staged_waiting_for_app_close" => text("Update ready", "تحديث جاهز"),
                        "installed_for_next_launch" => text("Installed", "مثبّت"),
                        "not_checked" => text("Not checked", "لم يُفحص"),
                        "error" => text("Check failed", "فشل الفحص"),
                        _ => text("Launcher unavailable", "المشغّل غير متاح"),
                    },
                )
                .on_hover_text(text(
                    "Last successful check; launch through the Windows launcher to enable checks. Errors leave version status unconfirmed.",
                    "نتيجة آخر فحص ناجح؛ افتح عبر مشغّل ويندوز لتفعيل الفحص. عند الخطأ تبقى حالة الإصدار غير مؤكدة.",
                ));
                check = ui
                    .add_enabled(self.enabled(), egui::Button::new(crate::rtl::widget(ui, text("Check & Stage Update", "فحص وتجهيز التحديث"))))
                    .clicked();
            });
            if self.manifest.as_ref().is_some_and(Manifest::staged) || status == "staged_waiting_for_app_close" {
                let commit = self.staged_commit().map_or("unknown", short_commit);
                crate::rtl::label(
                    ui,
                    egui::RichText::new(format!("{} ({commit})", text("Verified update staged", "تحديث موثق جاهز"))).color(tokens.accent),
                );
                crate::rtl::label(
                    ui,
                    text(
                        "Save your work, close DesignCraft and relaunch via Launcher to activate.",
                        "احفظ جميع أعمالك، ثم أغلق البرنامج وأعد فتحه عبر المشغّل لتفعيل التحديث.",
                    ),
                );
            }
            if let Some(error) = self.error.as_ref().or(self.manifest_error.as_ref()) {
                ui.collapsing(crate::rtl::widget(ui, text("Update error details", "تفاصيل خطأ التحديث")), |ui| {
                    crate::rtl::label(ui, error);
                });
            }
        });
        check
    }
}

#[cfg(target_os = "windows")]
fn execute_updater(manifest: &std::path::Path, configured_script: &std::path::Path) -> Result<UpdateResult, String> {
    use std::io::{Read, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const SCRIPT: &[u8] = include_bytes!("../../../packaging/windows/update-native-preview.ps1");
    // The manifest is display data. Its script is never executed; only this binary's
    // embedded updater is written to a new file and passed as a literal -File argument.
    if !configured_script.is_absolute() {
        return Err("Updater path must be absolute.".into());
    }
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_err(|error| error.to_string())?.as_nanos();
    let script = std::env::temp_dir().join(format!("designcraft-update-{}-{stamp}.ps1", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&script).map_err(|error| error.to_string())?;
    let outcome = (|| -> Result<UpdateResult, String> {
        file.write_all(SCRIPT).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        let system_root = std::env::var_os("SystemRoot").ok_or("Windows SystemRoot is unavailable.")?;
        let powershell = PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = Command::new(powershell)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-ManifestPath")
            .arg(manifest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|error| format!("Cannot start updater: {error}"))?;
        fn drain(mut stream: impl Read) -> Result<Vec<u8>, String> {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                let keep = count.min((MAX_OUTPUT as usize + 1).saturating_sub(output.len()));
                if let Some(bytes) = buffer.get(..keep) {
                    output.extend_from_slice(bytes);
                }
            }
            Ok(output)
        }
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Updater stdout is unavailable.".into());
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Updater stderr is unavailable.".into());
        };
        let out = match std::thread::Builder::new().name("preview-update-output".into()).spawn(move || drain(stdout)) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let err = match std::thread::Builder::new().name("preview-update-errors".into()).spawn(move || drain(stderr)) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                return Err(error.to_string());
            }
        };
        let started = Instant::now();
        let exit = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) if started.elapsed() < Duration::from_secs(600) => std::thread::sleep(Duration::from_millis(100)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err("Update check timed out after 10 minutes.".into());
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!("Cannot wait for updater: {error}"));
                }
            }
        };
        let stdout = out.join().map_err(|_| "Updater output reader failed.").and_then(|result| result.map_err(|_| "Cannot read updater output."));
        let stderr = err.join().map_err(|_| "Updater error reader failed.").and_then(|result| result.map_err(|_| "Cannot read updater errors."));
        UpdateResult::parse(exit?, &stdout?, &stderr?)
    })();
    let _ = std::fs::remove_file(script);
    outcome
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    const ACTIVE: &str = "0123456789abcdef0123456789abcdef01234567";
    const STAGED: &str = "abcdef0123456789abcdef0123456789abcdef01";

    #[test]
    fn ai24_preview_update_command_is_registered_and_rejects_script_parameters() {
        assert!(crate::menus::UI_COMMANDS.iter().any(|command| command.0 == "app.checkPreviewUpdate"));
        let mut app = crate::DesignApp::new(designcraft_engine::Session::new(), crate::Services::default());
        for params in [json!({"script": "arbitrary.ps1"}), json!({"url": "https://invalid.example"}), json!(7), json!(null)] {
            assert!(app.run("app.checkPreviewUpdate", params).is_err());
        }
        assert!(!crate::menus::enabled(&app, "app.checkPreviewUpdate"), "missing launcher manifest must disable update checks");
    }

    #[test]
    fn ai24_preview_update_manifest_keeps_active_and_staged_commits_distinct() {
        let manifest = super::Manifest::parse(
            &json!({
                "published_source_commit": ACTIVE,
                "update_status": "staged_waiting_for_app_close",
                "staged_source_commit": STAGED,
                "staged_executable": std::env::temp_dir().join("designcraft.exe"),
                "update_script_path": std::env::temp_dir().join("update-native-preview.ps1")
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(manifest.active_commit, ACTIVE);
        assert_eq!(manifest.staged_commit.as_deref(), Some(STAGED));
        assert!(manifest.staged());
    }

    #[test]
    fn ai24_preview_update_rejects_malformed_manifest_and_hostile_sha_without_panicking() {
        for source in ["{", "{}", r#"{"published_source_commit":"عربيعربيعربي","update_status":"up_to_date"}"#] {
            assert!(super::Manifest::parse(source).is_err());
        }
        assert_eq!(super::short_commit("عربيعربيعربي"), "unknown");
        assert_eq!(super::short_commit("short"), "unknown");
        assert_eq!(super::short_commit(ACTIVE), "01234567");
    }

    #[test]
    fn ai24_preview_update_result_requires_successful_exit_and_known_status() {
        let good = json!({"status": "staged_waiting_for_app_close", "selectedCommit": STAGED,
            "targetExe": std::env::temp_dir().join("designcraft.exe"), "ciUrl": "https://github.com/ahrrfy/designcraft/actions/runs/123"})
        .to_string();
        let result = super::UpdateResult::parse(true, good.as_bytes(), b"").unwrap();
        assert_eq!(result.status, "staged_waiting_for_app_close");
        assert!(super::UpdateResult::parse(false, good.as_bytes(), b"download failed").is_err());
        assert!(super::UpdateResult::parse(true, br#"{"status":"error"}"#, b"").is_err());
        assert!(super::UpdateResult::parse(true, b"The preview is up to date", b"").is_err());
        let commit_proof = json!({
            "status": "installed_for_next_launch",
            "selectedCommit": STAGED,
            "targetExe": std::env::temp_dir().join("designcraft.exe"),
            "ciUrl": format!("https://github.com/ahrrfy/designcraft/commit/{STAGED}")
        })
        .to_string();
        assert_eq!(super::UpdateResult::parse(true, commit_proof.as_bytes(), b"").unwrap().status, "installed_for_next_launch");
        let untrusted_proof = json!({
            "status": "installed_for_next_launch",
            "selectedCommit": STAGED,
            "targetExe": std::env::temp_dir().join("designcraft.exe"),
            "ciUrl": format!("https://github.com/storytold/designcraft/commit/{STAGED}")
        })
        .to_string();
        assert!(super::UpdateResult::parse(true, untrusted_proof.as_bytes(), b"").is_err());
    }

    #[test]
    fn ai24_preview_update_reserves_only_one_concurrent_worker() {
        let state = super::PreviewUpdate::default();
        assert!(state.reserve().is_ok());
        assert!(state.reserve().is_err());
    }

    #[test]
    fn ai24_preview_update_rejects_confined_automation_before_manifest_or_worker_access() {
        let directory = tempfile::tempdir().unwrap();
        let roots = designcraft_engine::file_access::AutomationRoots::new(Some(directory.path()), None).unwrap();
        let mut state = super::PreviewUpdate::default();
        let error = designcraft_engine::file_access::confine(roots.as_ref(), || state.command(&json!({}))).unwrap_err();
        assert!(error.contains("automation confinement"));
        assert!(!state.running.load(std::sync::atomic::Ordering::Acquire));
        assert!(state.manifest.is_none());
        assert!(state.manifest_path.is_none());
        #[cfg(target_os = "windows")]
        assert!(!state.initialized);
    }

    #[test]
    fn ai24_preview_update_staged_identity_takes_precedence_over_up_to_date_result() {
        let source = json!({"published_source_commit": ACTIVE, "update_status": "staged_waiting_for_app_close",
            "staged_source_commit": STAGED, "staged_executable": std::env::temp_dir().join("designcraft.exe"),
            "update_script_path": std::env::temp_dir().join("update-native-preview.ps1")})
        .to_string();
        let state = super::PreviewUpdate {
            manifest: Some(super::Manifest::parse(&format!("\u{feff}{source}")).unwrap()),
            result: Some(super::UpdateResult { status: "up_to_date".into(), commit: Some(ACTIVE.into()) }),
            ..Default::default()
        };
        assert_eq!(state.staged_commit(), Some(STAGED));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ai24_preview_update_manifest_poll_recovers_after_malformed_file_without_false_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher.json");
        std::fs::write(&path, "{").unwrap();
        let mut state = super::PreviewUpdate { initialized: true, manifest_path: Some(path.clone()), ..Default::default() };
        state.poll();
        assert!(state.manifest.is_none());
        assert!(state.manifest_error.is_some());
        assert!(state.result.is_none());
        std::fs::write(
            &path,
            json!({"published_source_commit": ACTIVE,
            "update_script_path": directory.path().join("update-native-preview.ps1")})
            .to_string(),
        )
        .unwrap();
        state.last_poll = None;
        state.poll();
        assert_eq!(state.manifest.as_ref().unwrap().status, "not_checked");
        assert!(state.manifest_error.is_none());
        assert!(state.result.is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ai24_preview_update_failed_completion_releases_worker_without_success_result() {
        let mut state = super::PreviewUpdate::default();
        state.reserve().unwrap();
        *state.completion.lock().unwrap() = Some(Err("download failed".into()));
        state.poll();
        assert!(state.error.as_ref().is_some_and(|error| error == "download failed"));
        assert!(state.result.is_none());
        assert!(state.reserve().is_ok());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn ai24_preview_update_unsupported_host_poll_is_inert() {
        let mut state = super::PreviewUpdate::default();
        state.poll();
        assert!(state.manifest.is_none());
        assert!(state.result.is_none());
        assert!(state.error.is_none());
        assert!(!state.enabled());
        assert!(state.command(&json!({})).is_err());
    }
}
