//! Explicit user-owned Ollama server lifecycle. Never discovers or kills a
//! service by PID/name/port, never downloads models, never accepts shell args.
//! Ollama environment options: https://docs.ollama.com/faq

use designcraft_ai_types::LocalResourcePolicy;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

pub const OWNED_ENDPOINT: &str = "http://127.0.0.1:11434";
#[cfg(any(windows, test))]
const OWNED_ADDRESS: std::net::SocketAddr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 11434));

#[derive(Clone, Debug)]
pub struct LocalServerState {
    /// Ownership exists only while this manager holds its spawned Child handle.
    pub owned: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub endpoint: String,
    pub message: String,
}

/// Store one instance on the native host. Start/stop must be reachable only from
/// explicit typed human UI actions, never control/MCP deserialization.
pub struct LocalOllama {
    child: Option<Child>,
    message: String,
}

impl Default for LocalOllama {
    fn default() -> Self {
        Self { child: None, message: "No app-owned Ollama process".into() }
    }
}

impl LocalOllama {
    pub fn start(&mut self, executable: &Path, policy: &LocalResourcePolicy) -> Result<LocalServerState, String> {
        #[cfg(not(windows))]
        {
            let _ = (executable, policy);
            Err("Starting an owned Ollama process is supported only on Windows".into())
        }
        #[cfg(windows)]
        {
            let state = self.status();
            if state.owned {
                return Err("An app-owned Ollama process already exists; stop it before starting another".into());
            }
            let result = self.start_windows(executable, policy);
            if let Err(error) = &result {
                self.message = error.clone();
            }
            result
        }
    }

    #[cfg(windows)]
    fn start_windows(&mut self, executable: &Path, policy: &LocalResourcePolicy) -> Result<LocalServerState, String> {
        let prepared = prepare_launch(executable, policy, OWNED_ADDRESS)?;
        let mut command = build_command(&prepared.executable, policy)?;
        // Keep the reservation throughout all checks/command construction. The
        // OS cannot atomically transfer a Rust listener to Ollama; a later bind
        // race makes this child fail, never makes another server ours.
        drop(prepared.port_reservation);
        let child = command.spawn().map_err(|_| "Cannot start the selected Ollama executable")?;
        self.child = Some(child);
        self.message =
            "Owned Ollama process started with bounded local defaults; refresh models to verify readiness. This is not an OS memory/VRAM sandbox"
                .into();
        Ok(self.status())
    }

    /// Nonblocking child-handle observation. A running process does not prove
    /// that its HTTP listener/model is ready; the separate provider probe does.
    pub fn status(&mut self) -> LocalServerState {
        let mut running = false;
        let mut exited = false;
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => running = true,
                Ok(Some(status)) => {
                    exited = true;
                    self.message = match status.code() {
                        Some(code) => format!("Owned Ollama process exited with code {code}"),
                        None => "Owned Ollama process exited".into(),
                    };
                }
                Err(_) => self.message = "Cannot confirm the owned Ollama process state; its handle is retained for a safe stop".into(),
            }
        }
        if exited {
            self.child = None;
        }
        LocalServerState {
            owned: self.child.is_some(),
            running,
            pid: self.child.as_ref().map(Child::id),
            endpoint: OWNED_ENDPOINT.into(),
            message: self.message.clone(),
        }
    }

    /// Stops only the stored Child handle. No PID lookup, taskkill, global
    /// interruption, server shutdown endpoint, or external service management.
    /// Reaping is bounded to two seconds; failed cleanup keeps the owned handle.
    pub fn stop(&mut self) -> Result<LocalServerState, String> {
        let state = self.status();
        if !state.owned {
            return Ok(state);
        }
        let child = self.child.as_mut().ok_or("Owned Ollama process handle is unavailable")?;
        child.kill().map_err(|_| "Could not stop the owned Ollama child process")?;
        let deadline = Instant::now().checked_add(Duration::from_secs(2)).ok_or("Invalid owned Ollama stop deadline")?;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    self.message = "Owned Ollama stop is pending; its process handle is retained".into();
                    return Err(self.message.clone());
                }
                Err(_) => {
                    self.message = "Cannot confirm that the owned Ollama child stopped; its handle is retained".into();
                    return Err(self.message.clone());
                }
            }
        }
        self.child = None;
        self.message = "App-owned Ollama process stopped".into();
        Ok(self.status())
    }
}

impl Drop for LocalOllama {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(any(windows, test))]
struct PreparedLaunch {
    executable: std::path::PathBuf,
    port_reservation: std::net::TcpListener,
}

#[cfg(any(windows, test))]
fn validate_path_shape(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > 4096 || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err("Choose an absolute local path to ollama.exe without command arguments".into());
    }
    if !path.file_name().and_then(std::ffi::OsStr::to_str).is_some_and(|name| name.eq_ignore_ascii_case("ollama.exe")) {
        return Err("The selected executable must be named ollama.exe; no command arguments are accepted".into());
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(),Some(Component::Prefix(prefix)) if matches!(prefix.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_))) {
            return Err("Choose ollama.exe on a local Windows drive; network/device paths are unsupported".into());
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn prepare_launch(executable: &Path, policy: &LocalResourcePolicy, address: std::net::SocketAddr) -> Result<PreparedLaunch, String> {
    policy.validate()?;
    validate_path_shape(executable)?;
    // Authority check deliberately precedes any executable metadata, read,
    // canonicalization or launch probe. Junctions are checked again resolved.
    designcraft_engine::file_access::check_read(executable)?;
    let canonical = executable.canonicalize().map_err(|_| "Cannot resolve the selected Ollama executable")?;
    designcraft_engine::file_access::check_read(&canonical)?;
    validate_path_shape(&canonical)?;
    if !canonical.is_file() {
        return Err("The selected Ollama executable is not a regular file".into());
    }
    let port_reservation = std::net::TcpListener::bind(address)
        .map_err(|_| "Loopback Ollama port is occupied or unavailable; the existing service was left untouched")?;
    Ok(PreparedLaunch { executable: canonical, port_reservation })
}

#[cfg(any(windows, test))]
fn build_command(executable: &Path, policy: &LocalResourcePolicy) -> Result<std::process::Command, String> {
    policy.validate()?;
    let parent = executable.parent().ok_or("Ollama executable has no parent directory")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("serve")
        .current_dir(parent)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Prevent inherited provider credentials, proxy credentials, and arbitrary
    // OLLAMA_* overrides from entering the child. Only OS runtime/profile
    // directories are inherited; model files stay in Ollama's usual user store.
    command.env_clear();
    for name in [
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "HOMEDRIVE",
        "HOMEPATH",
        "TEMP",
        "TMP",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("OLLAMA_HOST", "127.0.0.1:11434")
        .env("OLLAMA_NO_CLOUD", "1")
        .env("OLLAMA_NUM_PARALLEL", "1")
        .env("OLLAMA_MAX_LOADED_MODELS", "1")
        .env("OLLAMA_MAX_QUEUE", "2")
        .env("OLLAMA_CONTEXT_LENGTH", policy.context_tokens.to_string())
        .env("OLLAMA_KEEP_ALIVE", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn relative_paths_and_arbitrary_command_arguments_are_rejected() {
        assert!(validate_path_shape(Path::new("ollama.exe")).is_err());
        assert!(validate_path_shape(Path::new(".\\ollama.exe")).is_err());
        let root = tempfile::tempdir().unwrap();
        for name in ["cmd.exe", "powershell.exe", "ollama.exe serve", "ollama.exe --model anything", "ollama.bat"] {
            assert!(validate_path_shape(&root.path().join(name)).is_err());
        }
        assert!(validate_path_shape(&root.path().join("ollama.exe")).is_ok());
    }

    #[test]
    fn executable_probe_respects_read_authority_before_metadata() {
        use designcraft_engine::file_access::{self, AutomationRoots};
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = AutomationRoots::new(&[allowed.path()], &[]).unwrap();
        // The file does not exist: authority must reject it before the missing
        // file probe could replace the containment error with an I/O error.
        let result = file_access::confine(roots.as_ref(), || {
            prepare_launch(&outside.path().join("ollama.exe"), &LocalResourcePolicy::default(), OWNED_ADDRESS)
        });
        let error = match result {
            Ok(_) => String::new(),
            Err(error) => error,
        };
        assert!(error.contains("outside the read root"));
    }

    #[test]
    fn occupied_loopback_port_is_rejected_before_any_process_launch() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ollama.exe");
        std::fs::write(&executable, b"synthetic non-executable fixture").unwrap();
        let existing = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let result = prepare_launch(&executable, &LocalResourcePolicy::default(), existing.local_addr().unwrap());
        let error = match result {
            Ok(_) => String::new(),
            Err(error) => error,
        };
        assert!(error.contains("occupied or unavailable"));
        assert!(existing.local_addr().is_ok());
    }

    #[test]
    fn successful_preflight_reserves_only_loopback_without_launching() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ollama.exe");
        std::fs::write(&executable, b"synthetic non-executable fixture").unwrap();
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
        let prepared = prepare_launch(&executable, &LocalResourcePolicy::default(), address).unwrap();
        assert_eq!(prepared.executable, executable.canonicalize().unwrap());
        assert!(prepared.port_reservation.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn launch_command_is_fixed_and_contains_only_bounded_ollama_defaults() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ollama.exe");
        let policy = LocalResourcePolicy { context_tokens: 4096, ..Default::default() };
        let command = build_command(&executable, &policy).unwrap();
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec![std::ffi::OsStr::new("serve")]);
        assert_eq!(command.get_program(), executable.as_os_str());
        let environment = command
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (name, value) in [
            ("OLLAMA_HOST", "127.0.0.1:11434"),
            ("OLLAMA_NO_CLOUD", "1"),
            ("OLLAMA_NUM_PARALLEL", "1"),
            ("OLLAMA_MAX_LOADED_MODELS", "1"),
            ("OLLAMA_MAX_QUEUE", "2"),
            ("OLLAMA_CONTEXT_LENGTH", "4096"),
            ("OLLAMA_KEEP_ALIVE", "0"),
        ] {
            assert_eq!(environment.get(name).map(String::as_str), Some(value));
        }
        assert!(!environment.contains_key("OPENAI_API_KEY"));
        assert!(!environment.contains_key("GEMINI_API_KEY"));
        assert!(!environment.contains_key("HTTPS_PROXY"));
        assert!(!environment.contains_key("OLLAMA_API_KEY"));
    }

    #[test]
    fn stop_without_owned_child_never_touches_an_external_service() {
        let mut server = LocalOllama::default();
        let status = server.stop().unwrap();
        assert!(!status.owned);
        assert!(!status.running);
        assert_eq!(status.pid, None);
    }
}
