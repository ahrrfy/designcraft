//! OS-protected credentials, isolated from profile/settings/report files.
//!
//! Windows uses the explicit safe WinCredential API from pinned keyring 3.6.3.
//! This avoids its global default-builder lock and never creates plaintext files.
//! References: https://learn.microsoft.com/en-us/windows/win32/secbp/handling-passwords
//! and https://docs.rs/keyring/3.6.3/x86_64-pc-windows-msvc/keyring/windows/struct.WinCredential.html

use designcraft_ai_providers::{SecretString, validate_endpoint};
use designcraft_ai_types::{ModelSelection, ProviderKind};
use sha2::{Digest, Sha256};
use std::path::Path;
use zeroize::Zeroize;

const SERVICE: &str = "org.storytold.designcraft.ai.v1";
const MAX_SECRET_BYTES: usize = 1280;
#[cfg(not(windows))]
const UNSUPPORTED: &str = "OS-protected API key storage is unavailable on this platform; use an in-memory key";
const STORE_ERROR: &str = "Windows credential storage is unavailable, locked, or denied";

/// A canonical profile identity, never a filesystem location for secret data.
/// Endpoint changes select another credential; they never reuse the old key.
pub struct Vault {
    profile_digest: String,
}

/// Runtime-only key authorization for one provider endpoint. Model changes do
/// not change authorization; endpoint changes do. Never serializable.
pub(crate) struct SessionKey {
    provider: ProviderKind,
    endpoint: String,
    secret: SecretString,
}

impl SessionKey {
    pub(crate) fn new(selection: &ModelSelection, secret: SecretString) -> Result<Self, String> {
        validate_secret(&secret)?;
        let mut endpoint = validate_endpoint(selection)?.to_string();
        if endpoint.contains(secret.expose()) {
            endpoint.zeroize();
            return Err("API key must not occur in endpoint URLs".into());
        }
        Ok(Self { provider: selection.provider, endpoint, secret })
    }

    pub(crate) fn matches(&self, selection: &ModelSelection) -> bool {
        selection.provider == self.provider && validate_endpoint(selection).is_ok_and(|endpoint| endpoint.as_str() == self.endpoint)
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self { provider: self.provider, endpoint: self.endpoint.clone(), secret: SecretString::new(self.secret.expose().into()) }
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.endpoint.zeroize();
    }
}

pub(crate) fn selected_key(keys: &[SessionKey], selection: &ModelSelection) -> Option<SecretString> {
    selected_key_with(keys, selection, |name| std::env::var(name).ok())
}

fn into_validated_secret(mut raw: String) -> Result<SecretString, String> {
    if raw.is_empty() || raw.len() > MAX_SECRET_BYTES || raw.chars().any(char::is_control) {
        raw.zeroize();
        return Err("API key is empty, too long, or contains control characters".into());
    }
    Ok(SecretString::new(raw))
}

fn selected_key_with(keys: &[SessionKey], selection: &ModelSelection, environment: impl FnOnce(&str) -> Option<String>) -> Option<SecretString> {
    if let Some(key) = keys.iter().find(|key| key.matches(selection)) {
        return Some(SecretString::new(key.secret.expose().into()));
    }
    let name = match selection.provider {
        ProviderKind::OpenAi => "OPENAI_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        _ => return None,
    };
    let endpoint = validate_endpoint(selection).ok()?;
    let canonical = validate_endpoint(&ModelSelection { provider: selection.provider, model: String::new(), endpoint: None }).ok()?;
    if endpoint != canonical {
        return None;
    }
    into_validated_secret(environment(name)?).ok()
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("os_protected", &Self::available()).finish_non_exhaustive()
    }
}

impl Vault {
    /// Resolve aliases once. Later junction changes cannot switch this vault's
    /// identity. The caller must already own/validate its profile root.
    pub fn for_profile(profile_root: &Path) -> Result<Self, String> {
        let root = profile_root.canonicalize().map_err(|_| "Cannot identify the AI profile for credential storage")?;
        if !root.is_dir() {
            return Err("AI credential profile must be a directory".into());
        }
        Ok(Self { profile_digest: digest(&[root.as_os_str().as_encoded_bytes()]) })
    }

    pub const fn available() -> bool {
        cfg!(windows)
    }

    pub fn save(&self, selection: &ModelSelection, secret: &SecretString) -> Result<(), String> {
        self.save_with(&NativeBackend, selection, secret)
    }

    pub fn load(&self, selection: &ModelSelection) -> Result<Option<SecretString>, String> {
        let mut target = self.target(selection)?;
        let secret = guard(|| NativeBackend.load(&target));
        target.zeroize();
        let secret = secret?;
        if let Some(secret) = &secret {
            validate_secret(secret)?;
        }
        Ok(secret)
    }

    pub fn remove(&self, selection: &ModelSelection) -> Result<(), String> {
        let mut target = self.target(selection)?;
        let result = guard(|| NativeBackend.remove(&target));
        target.zeroize();
        result
    }

    /// Returns only presence. The key is never loaded into UI/snapshot DTOs.
    pub fn presence(&self, selection: &ModelSelection) -> Result<bool, String> {
        let mut target = self.target(selection)?;
        let result = guard(|| NativeBackend.presence(&target));
        target.zeroize();
        result
    }

    fn save_with(&self, backend: &impl Backend, selection: &ModelSelection, secret: &SecretString) -> Result<(), String> {
        validate_secret(secret)?;
        let mut endpoint = String::from(validate_endpoint(selection)?);
        let leaked_in_endpoint = endpoint.contains(secret.expose());
        endpoint.zeroize();
        if leaked_in_endpoint {
            return Err("API key must not occur in endpoint URLs".into());
        }
        let mut target = self.target(selection)?;
        let result = guard(|| backend.save(&target, secret));
        target.zeroize();
        result
    }

    fn target(&self, selection: &ModelSelection) -> Result<String, String> {
        let endpoint = validate_endpoint(selection)?;
        let provider = match selection.provider {
            ProviderKind::OpenAi => "open_ai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Ollama => "ollama",
            ProviderKind::LocalOpenAi => "local_open_ai",
            ProviderKind::ComfyUi => "comfy_ui",
        };
        let slot = digest(&[self.profile_digest.as_bytes(), provider.as_bytes(), endpoint.as_str().as_bytes(), b"default"]);
        Ok(format!("{SERVICE}/{slot}"))
    }
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    let bytes = hash.finalize();
    format!("{bytes:x}")
}

fn validate_secret(secret: &SecretString) -> Result<(), String> {
    let secret = secret.expose();
    // Bound UTF-16 expansion below Windows' credential blob limit as well.
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES || secret.chars().any(char::is_control) {
        return Err("API key is empty, too long, or contains control characters".into());
    }
    Ok(())
}

fn guard<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).map_err(|_| STORE_ERROR.to_string())?
}

trait Backend {
    fn save(&self, target: &str, secret: &SecretString) -> Result<(), String>;
    fn load(&self, target: &str) -> Result<Option<SecretString>, String>;
    fn remove(&self, target: &str) -> Result<(), String>;
    fn presence(&self, target: &str) -> Result<bool, String>;
}

struct NativeBackend;

#[cfg(windows)]
mod windows {
    use super::{Backend, NativeBackend, SERVICE, STORE_ERROR, SecretString};
    use keyring::{credential::CredentialApi, windows::WinCredential};

    fn entry(target: &str) -> Result<WinCredential, String> {
        WinCredential::new_with_target(Some(target), SERVICE, "provider-api-key").map_err(|_| STORE_ERROR.to_string())
    }

    impl Backend for NativeBackend {
        fn save(&self, target: &str, secret: &SecretString) -> Result<(), String> {
            entry(target)?.set_password(secret.expose()).map_err(|_| STORE_ERROR.to_string())
        }
        fn load(&self, target: &str) -> Result<Option<SecretString>, String> {
            match entry(target)?.get_password() {
                Ok(secret) => Ok(Some(SecretString::new(secret))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(STORE_ERROR.into()),
            }
        }
        fn remove(&self, target: &str) -> Result<(), String> {
            match entry(target)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(STORE_ERROR.into()),
            }
        }
        fn presence(&self, target: &str) -> Result<bool, String> {
            // Metadata query, not get_password. The vetted wrapper clears the
            // OS-provided secret buffer before releasing it to Windows.
            match entry(target)?.get_credential() {
                Ok(_) => Ok(true),
                Err(keyring::Error::NoEntry) => Ok(false),
                Err(_) => Err(STORE_ERROR.into()),
            }
        }
    }
}

#[cfg(not(windows))]
impl Backend for NativeBackend {
    fn save(&self, _target: &str, _secret: &SecretString) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    fn load(&self, _target: &str) -> Result<Option<SecretString>, String> {
        Err(UNSUPPORTED.into())
    }
    fn remove(&self, _target: &str) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    fn presence(&self, _target: &str) -> Result<bool, String> {
        Err(UNSUPPORTED.into())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::cell::RefCell;

    fn model() -> ModelSelection {
        ModelSelection { provider: ProviderKind::OpenAi, model: "test-model".into(), endpoint: None }
    }

    struct Capture(RefCell<Option<String>>);
    impl Backend for Capture {
        fn save(&self, target: &str, _secret: &SecretString) -> Result<(), String> {
            *self.0.borrow_mut() = Some(target.into());
            Ok(())
        }
        fn load(&self, _target: &str) -> Result<Option<SecretString>, String> {
            Ok(None)
        }
        fn remove(&self, _target: &str) -> Result<(), String> {
            Ok(())
        }
        fn presence(&self, _target: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[test]
    fn metadata_is_profile_provider_and_endpoint_bound() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let vault = Vault::for_profile(first.path()).unwrap();
        let original = vault.target(&model()).unwrap();
        assert_ne!(original, Vault::for_profile(second.path()).unwrap().target(&model()).unwrap());
        let mut selection = model();
        selection.provider = ProviderKind::Gemini;
        assert_ne!(original, vault.target(&selection).unwrap());
        selection = model();
        selection.endpoint = Some("https://proxy.example/v1/".into());
        assert_ne!(original, vault.target(&selection).unwrap());
        selection = model();
        selection.endpoint = Some("https://api.openai.com/v1".into());
        assert_eq!(original, vault.target(&selection).unwrap());
        assert!(!original.contains("openai.com"));
    }

    #[test]
    fn secret_is_never_written_into_profile_or_credential_metadata() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::for_profile(root.path()).unwrap();
        let secret = SecretString::new("synthetic-secret-not-a-real-key".into());
        let capture = Capture(RefCell::new(None));
        vault.save_with(&capture, &model(), &secret).unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(!format!("{vault:?}{secret:?}").contains(secret.expose()));
        assert!(!capture.0.borrow().as_ref().unwrap().contains(secret.expose()));
        assert!(!capture.0.borrow().as_ref().unwrap().contains(&root.path().to_string_lossy().to_string()));
    }

    #[test]
    fn invalid_secret_or_endpoint_fails_before_backend_write() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::for_profile(root.path()).unwrap();
        let capture = Capture(RefCell::new(None));
        for secret in [String::new(), "contains\nnewline".into(), "x".repeat(MAX_SECRET_BYTES + 1)] {
            assert!(vault.save_with(&capture, &model(), &SecretString::new(secret)).is_err());
        }
        let mut selection = model();
        selection.endpoint = Some("https://example.org/?key=synthetic".into());
        assert!(vault.save_with(&capture, &selection, &SecretString::new("synthetic".into())).is_err());
        assert!(capture.0.borrow().is_none());
    }

    #[test]
    fn untrusted_custom_endpoint_cannot_receive_session_or_environment_key() {
        let selected = model();
        let keys = vec![SessionKey::new(&selected, SecretString::new("synthetic-session-key".into())).unwrap()];
        let mut attacker = selected.clone();
        attacker.endpoint = Some("https://attacker.example/v1/".into());
        let environment_reads = std::cell::Cell::new(0);
        let stolen = selected_key_with(&keys, &attacker, |_| {
            environment_reads.set(1);
            Some("synthetic-env-key".into())
        });
        assert!(stolen.is_none());
        assert_eq!(environment_reads.get(), 0);
        let selected_key = selected_key_with(&keys, &selected, |_| None).unwrap();
        assert_eq!(selected_key.expose(), "synthetic-session-key");
    }

    #[test]
    fn environment_fallback_is_only_for_exact_canonical_vendor_endpoint() {
        for (provider, name) in [(ProviderKind::OpenAi, "OPENAI_API_KEY"), (ProviderKind::Gemini, "GEMINI_API_KEY")] {
            let selected = ModelSelection { provider, model: String::new(), endpoint: None };
            let key = selected_key_with(&[], &selected, |actual| {
                assert_eq!(actual, name);
                Some("synthetic-env-key".into())
            })
            .unwrap();
            assert_eq!(key.expose(), "synthetic-env-key");
            for endpoint in [
                "https://api.openai.com.attacker.example/v1/",
                "https://api.openai.com:8443/v1/",
                "https://api.openai.com/other/",
                "http://localhost:8000/",
            ] {
                let custom = ModelSelection { endpoint: Some(endpoint.into()), ..selected.clone() };
                assert!(selected_key_with(&[], &custom, |_| panic!("must not read environment for an unauthorized endpoint")).is_none());
            }
        }
    }

    #[test]
    fn sec07_temporary_secret_validation_and_endpoint_leak_rejections_cleanly_fail() {
        assert!(into_validated_secret(String::new()).is_err());
        assert!(into_validated_secret("bad\0key".into()).is_err());
        let ok = into_validated_secret("synthetic-valid-secret".into()).unwrap();
        assert_eq!(ok.expose(), "synthetic-valid-secret");

        let leaked_sel = ModelSelection {
            provider: ProviderKind::OpenAi,
            model: "test-model".into(),
            endpoint: Some("https://proxy.example/synthetic-valid-secret/v1/".into()),
        };
        assert!(SessionKey::new(&leaked_sel, SecretString::new("synthetic-valid-secret".into())).is_err());
        assert!(selected_key_with(&[], &model(), |_| Some("invalid\nenv\nsecret".into())).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_never_falls_back_to_plaintext() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::for_profile(root.path()).unwrap();
        assert!(!Vault::available());
        assert!(vault.save(&model(), &SecretString::new("synthetic".into())).is_err());
        assert!(vault.load(&model()).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    /// Opt-in: uses only a synthetic value and a unique disposable profile slot.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes a disposable synthetic credential to the Windows user vault"]
    fn windows_protected_store_synthetic_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::for_profile(root.path()).unwrap();
        let selection = model();
        struct Cleanup<'a>(&'a Vault, &'a ModelSelection);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.remove(self.1);
            }
        }
        let _cleanup = Cleanup(&vault, &selection);
        assert!(!vault.presence(&selection).unwrap());
        let secret = SecretString::new("designcraft-disposable-synthetic-not-a-live-api-key".into());
        vault.save(&selection, &secret).unwrap();
        assert!(vault.presence(&selection).unwrap());
        assert_eq!(vault.load(&selection).unwrap().unwrap().expose(), secret.expose());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        vault.remove(&selection).unwrap();
        assert!(!vault.presence(&selection).unwrap());
    }
}
