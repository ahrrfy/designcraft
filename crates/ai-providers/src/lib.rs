//! Explicit, bounded provider access. Call on a worker, never on the UI thread.
//! No provider fallback, automatic model downloads, or credential persistence.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use designcraft_ai_types::{ModelSelection, ProviderKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

#[cfg(not(target_arch = "wasm32"))]
mod native;

/// Runtime-only secret. Intentionally neither serializable nor printable.
///
/// ```compile_fail
/// let secret = designcraft_ai_providers::SecretString::new("synthetic".into());
/// let _ = serde_json::to_string(&secret);
/// ```
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

#[derive(Clone, Debug)]
pub struct GenerationRequest {
    pub system: String,
    pub prompt: String,
    pub json_schema: Value,
    pub max_output_tokens: u32,
    pub images: Vec<ImageInput>,
    pub resources: designcraft_ai_types::LocalResourcePolicy,
}

/// Runtime-only reference image; not stored in configuration or skill files.
#[derive(Clone)]
pub struct ImageInput {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput").field("mime_type", &self.mime_type).field("byte_length", &self.bytes.len()).finish()
    }
}

/// Missing provider measurements remain `None`; zero means a reported zero.
/// Output tokens include separately reported reasoning (Gemini candidate plus
/// thought tokens); reasoning_tokens is a subset, not an additional charge.
/// Cache writes remain unknown unless explicitly reported by a provider.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct GenerationResponse {
    pub text: String,
    pub usage: Usage,
    /// Provider refusal/incomplete status. Record usage before inspecting this.
    /// Invalid model JSON remains raw text with no error for caller repair.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// Discovery does not imply support for every structured-output schema.
    pub supports_generate_content: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ComfyRequest {
    /// An API-format workflow supplied by the user. `generate_images` binds
    /// {{prompt}} and {{model}} in string values to the actual task selection.
    pub workflow: Value,
    pub poll_interval_ms: u64,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct ImageRequest {
    pub prompt: String,
    /// OpenAI Images options; None delegates to the selected model's default.
    pub size: Option<String>,
    pub quality: Option<String>,
    /// Required only for ComfyUI; no workflow or model is installed implicitly.
    pub comfy: Option<ComfyRequest>,
    pub resources: designcraft_ai_types::LocalResourcePolicy,
}

#[derive(Clone, Debug)]
pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
    pub usage: Usage,
    /// Parsed provider response failed/refused or returned invalid image data.
    /// Record measured usage before checking this status; images are empty.
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime_type: String,
}

impl std::fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedImage").field("byte_length", &self.bytes.len()).field("mime_type", &self.mime_type).finish_non_exhaustive()
    }
}

pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { Err("AI request cancelled".into()) } else { Ok(()) }
}

/// Validate before persisting configuration. Endpoint credentials, query and
/// fragment are prohibited. Cloud providers require HTTPS; local providers
/// allow HTTP only to literal loopback IPs or localhost. Redirects are disabled.
pub fn validate_endpoint(selection: &ModelSelection) -> Result<Url, String> {
    let default = match selection.provider {
        ProviderKind::OpenAi => "https://api.openai.com/v1/",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta/",
        ProviderKind::Ollama => "http://127.0.0.1:11434/",
        ProviderKind::ComfyUi => "http://127.0.0.1:8188/",
    };
    let raw = selection.endpoint.as_deref().unwrap_or(default);
    if raw.len() > 2048 {
        return Err("AI endpoint is too long".into());
    }
    let mut url = Url::parse(raw).map_err(|_| "Invalid AI endpoint URL")?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("AI endpoint must not contain credentials, query, or fragment".into());
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => return Err("AI endpoint needs a host".into()),
    };
    let local = matches!(selection.provider, ProviderKind::Ollama | ProviderKind::ComfyUi);
    if url.scheme() != "https" && !(local && loopback && url.scheme() == "http") {
        return Err("AI endpoint requires HTTPS (local HTTP is restricted to loopback)".into());
    }
    let path = url.path().to_string();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    Ok(url)
}

pub fn generate(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &GenerationRequest,
    cancel: &AtomicBool,
) -> Result<GenerationResponse, String> {
    check_cancel(cancel)?;
    validate_endpoint(selection)?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::generate(selection, key, request, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (key, request);
        Err("AI HTTP transport is available only in the native application".into())
    }
}

/// Real model discovery; ComfyUI returns checkpoint names, not text models.
pub fn list_models(selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<ModelInfo>, String> {
    check_cancel(cancel)?;
    validate_endpoint(selection)?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::list_models(selection, key, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = key;
        Err("AI HTTP transport is available only in the native application".into())
    }
}

/// Connectivity/authentication check uses discovery and does not spend a generation.
pub fn test_connection(selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<ModelInfo>, String> {
    list_models(selection, key, cancel)
}

/// Executes exactly the supplied ComfyUI workflow. Cancellation removes only
/// this job from the pending queue; a running job may continue server-side.
/// Never calls ComfyUI's global /interrupt endpoint.
pub fn generate_comfy(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &ComfyRequest,
    cancel: &AtomicBool,
) -> Result<Vec<GeneratedImage>, String> {
    check_cancel(cancel)?;
    validate_endpoint(selection)?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::generate_comfy(selection, key, request, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (key, request);
        Err("AI HTTP transport is available only in the native application".into())
    }
}

pub fn generate_images(selection: &ModelSelection, key: Option<&str>, request: &ImageRequest, cancel: &AtomicBool) -> Result<ImageResponse, String> {
    check_cancel(cancel)?;
    validate_endpoint(selection)?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::generate_images(selection, key, request, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (key, request);
        Err("AI HTTP transport is available only in the native application".into())
    }
}

#[cfg(test)]
mod tests;
