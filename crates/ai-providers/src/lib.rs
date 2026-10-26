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
    /// OpenAI pixel size, or a Gemini native aspect ratio (e.g. 4:1).
    /// A Gemini ratio may include an explicit native tier: `4:1@2K`.
    /// None delegates to the selected model's default.
    pub size: Option<String>,
    pub quality: Option<String>,
    /// Required only for ComfyUI; no workflow or model is installed implicitly.
    pub comfy: Option<ComfyRequest>,
    pub resources: designcraft_ai_types::LocalResourcePolicy,
}

/// Select native source resolution without resampling. Actual returned pixels still need preflight.
/// Dimensions and tiers: https://ai.google.dev/gemini-api/docs/generate-content/image-generation
pub fn gemini_print_size(model: &str, ratio: &str, min_pixels: [u32; 2]) -> Result<String, String> {
    if !gemini_has_print_tiers(model) || !gemini_print_ratio_supported(model, ratio) {
        return Err("Selected Gemini model has no verified 2K/4K print resolution support".into());
    }
    let [width, height] = gemini_base_pixels(ratio).ok_or("Unsupported Gemini image aspect ratio")?;
    let [min_width, min_height] = min_pixels;
    if min_width == 0 || min_height == 0 {
        return Err("Print image dimensions must be positive".into());
    }
    for (scale, tier) in [(1, "1K"), (2, "2K"), (4, "4K")] {
        if width * scale >= min_width && height * scale >= min_height {
            return Ok(format!("{ratio}@{tier}"));
        }
    }
    Err(format!(
        "Requested source image {min_width}×{min_height} pixels exceeds Gemini's documented 4K tier for {ratio}; reduce the placed size or use a higher-resolution original"
    ))
}

fn normalize_gemini_model(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    lower.strip_prefix("models/").unwrap_or(&lower).to_string()
}

fn gemini_has_print_tiers(model: &str) -> bool {
    let model = normalize_gemini_model(model);
    ["gemini-3.1-flash-image", "gemini-3-pro-image", "gemini-3.1-pro-image", "gemini-nano-banana-2.1"]
        .iter()
        .any(|known| model == *known || model.strip_prefix(known).is_some_and(|suffix| suffix.starts_with("-preview")))
}

fn gemini_print_ratio_supported(model: &str, ratio: &str) -> bool {
    if matches!(ratio, "1:4" | "4:1" | "1:8" | "8:1") {
        let model = normalize_gemini_model(model);
        model.starts_with("gemini-3.1-flash-image") || model.starts_with("gemini-3.1-flash-lite-image") || model.starts_with("gemini-nano-banana-2.1")
    } else {
        gemini_base_pixels(ratio).is_some()
    }
}

fn gemini_base_pixels(ratio: &str) -> Option<[u32; 2]> {
    Some(match ratio {
        "1:1" => [1024, 1024],
        "1:4" => [512, 2048],
        "1:8" => [384, 3072],
        "2:3" => [848, 1264],
        "3:2" => [1264, 848],
        "3:4" => [896, 1200],
        "4:1" => [2048, 512],
        "4:3" => [1200, 896],
        "4:5" => [928, 1152],
        "5:4" => [1152, 928],
        "8:1" => [3072, 384],
        "9:16" => [768, 1376],
        "16:9" => [1376, 768],
        "21:9" => [1584, 672],
        _ => return None,
    })
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

fn is_forbidden_cloud_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        || octets[0] == 0
}

fn is_forbidden_cloud_ipv6(ip: std::net::Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped().or_else(|| ip.to_ipv4())
        && is_forbidden_cloud_ipv4(v4)
    {
        return true;
    }
    let segments = ip.segments();
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || (segments[0] & 0xfe00) == 0xfc00 || (segments[0] & 0xffc0) == 0xfe80
}

fn is_forbidden_cloud_domain(host: &str) -> bool {
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    lower.is_empty() || lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") || lower.ends_with(".internal")
}

/// Validate before persisting configuration. Endpoint credentials, query and
/// fragment are prohibited. Cloud providers require HTTPS to public hosts;
/// local providers allow HTTP/HTTPS only to localhost, 127.0.0.1, or [::1]. Redirects are disabled.
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
    let local = matches!(selection.provider, ProviderKind::Ollama | ProviderKind::ComfyUi);
    if local {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Local AI endpoint requires HTTP or HTTPS".into());
        }
        let allowed_local_host = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
            Some(url::Host::Domain(host)) => host.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
            None => return Err("AI endpoint needs a host".into()),
        };
        if !allowed_local_host {
            return Err("Local AI endpoint must target localhost, 127.0.0.1, or [::1]".into());
        }
    } else {
        if url.scheme() != "https" {
            return Err("AI endpoint requires HTTPS (local HTTP is restricted to loopback)".into());
        }
        match url.host() {
            Some(url::Host::Ipv4(ip)) if is_forbidden_cloud_ipv4(ip) => {
                return Err("Cloud AI endpoint must not target loopback, private, link-local, metadata, or CGNAT IP addresses".into());
            }
            Some(url::Host::Ipv6(ip)) if is_forbidden_cloud_ipv6(ip) => {
                return Err("Cloud AI endpoint must not target loopback, unique-local, link-local, or private IPv6 addresses".into());
            }
            Some(url::Host::Domain(host)) if is_forbidden_cloud_domain(host) => {
                return Err("Cloud AI endpoint must not target localhost, .local, or .internal hostnames".into());
            }
            Some(_) => {}
            None => return Err("AI endpoint needs a host".into()),
        }
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
