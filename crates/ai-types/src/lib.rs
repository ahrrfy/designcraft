//! Portable data contracts. Credentials belong to the native runtime, never these types.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use serde::{Deserialize, Serialize};

pub mod design;
pub mod protocol;
pub use design::{DesignCorrection, DesignImageSlot, DesignProfile, DesignStyleUse, DesignTextSlot, PageFamily};
pub mod resources;
pub use resources::{GpuSnapshot, LocalResourcePolicy, ResourceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    Gemini,
    Ollama,
    ComfyUi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    NativeOnly,
    NativeWithImages,
}

/// A native engine command; never a script or shell command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineAction {
    pub command: String,
    pub params: serde_json::Value,
}

/// Public model configuration. Endpoint URLs must not include credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: ProviderKind,
    pub model: String,
    pub endpoint: Option<String>,
}

/// Identifies known image, audio, video, speech, live-session, and embedding model identifiers
/// using delimited tokens or family prefixes so custom/local text aliases (such as `silver-text`)
/// are not rejected by substring collisions.
pub fn is_non_text_model(model: &str) -> bool {
    let model = model.trim().trim_start_matches("models/").to_ascii_lowercase();
    if model.is_empty() {
        return false;
    }
    if ["dall-e", "gpt-image", "imagen", "veo", "whisper"]
        .iter()
        .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")) || model.starts_with(&format!("{prefix}_")))
    {
        return true;
    }
    model.split(|c: char| !c.is_ascii_alphanumeric()).any(|component| {
        matches!(component, "image" | "images" | "imagen" | "tts" | "audio" | "embedding" | "embeddings" | "live" | "veo" | "aqa" | "whisper")
    })
}
