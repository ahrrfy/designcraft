//! Portable data contracts. Credentials belong to the native runtime, never these types.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use serde::{Deserialize, Serialize};

pub mod design;
pub mod protocol;
pub use design::{DesignCorrection, DesignImageSlot, DesignProfile, DesignStyleUse, DesignTextSlot, PageFamily};
pub mod resources;
pub use resources::{GpuSnapshot, LocalResourcePolicy, ResourceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
        matches!(
            component,
            "image"
                | "images"
                | "imagen"
                | "tts"
                | "audio"
                | "embed"
                | "embedding"
                | "embeddings"
                | "live"
                | "realtime"
                | "transcribe"
                | "moderation"
                | "veo"
                | "sora"
                | "lyria"
                | "aqa"
                | "whisper"
        )
    })
}

/// Identifies providers or model identifiers that cannot generate images so illustrated runs fail
/// before spending text-planning tokens.
pub fn is_non_image_model(provider: ProviderKind, model: &str) -> bool {
    if provider == ProviderKind::Ollama {
        return true;
    }
    if provider == ProviderKind::ComfyUi {
        return false;
    }
    let model = model.trim().trim_start_matches("models/").to_ascii_lowercase();
    if model.is_empty() {
        return false;
    }
    if provider == ProviderKind::Gemini && (model == "imagen" || model.starts_with("imagen-") || model.starts_with("imagen_")) {
        return true;
    }
    if model.starts_with("dall-e")
        || model.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| {
            if provider == ProviderKind::Gemini {
                matches!(part, "image" | "images" | "banana")
            } else {
                matches!(part, "image" | "images" | "imagen" | "banana" | "dalle" | "flux" | "sdxl" | "diffusion")
            }
        })
    {
        return false;
    }
    if [
        "gemini-",
        "gemma-",
        "learnlm-",
        "gpt-3",
        "gpt-4",
        "gpt-5",
        "chatgpt-",
        "claude-",
        "llama",
        "qwen",
        "mistral",
        "deepseek",
        "text-embedding",
        "tts-",
        "whisper",
        "veo",
        "sora",
        "lyria",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
    {
        return true;
    }
    if ["o1", "o3", "o4"]
        .iter()
        .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")) || model.starts_with(&format!("{prefix}_")))
    {
        return true;
    }
    model.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| {
        matches!(
            part,
            "tts"
                | "audio"
                | "embed"
                | "embedding"
                | "embeddings"
                | "live"
                | "realtime"
                | "transcribe"
                | "moderation"
                | "veo"
                | "sora"
                | "lyria"
                | "aqa"
                | "whisper"
        )
    })
}

/// Conservative local check for Gemini text models that support multimodal image inspection.
pub fn supports_gemini_image_review(provider: ProviderKind, model: &str) -> bool {
    if provider != ProviderKind::Gemini {
        return false;
    }
    let model = model.trim().to_ascii_lowercase();
    let model = model.strip_prefix("models/").unwrap_or(&model);
    [
        "gemini-flash",
        "gemini-flash-lite",
        "gemini-pro",
        "gemini-2.0-flash",
        "gemini-2.0-flash-lite",
        "gemini-2.5-flash",
        "gemini-2.5-flash-lite",
        "gemini-2.5-pro",
        "gemini-3-flash",
        "gemini-3-pro",
        "gemini-3.1-flash",
        "gemini-3.1-pro",
        "gemini-3.1-flash-lite",
        "gemini-3.5-flash",
        "gemini-3.5-pro",
        "gemini-3.5-flash-lite",
        "gemini-3.6-flash",
        "gemini-3.6-pro",
        "gemini-3.8-flash",
        "gemini-3.8-pro",
    ]
    .iter()
    .any(|family| {
        model == *family
            || model.strip_prefix(family).is_some_and(|suffix| {
                suffix == "-latest"
                    || suffix == "-preview-customtools"
                    || suffix.strip_prefix("-preview").is_some_and(|date| {
                        date.is_empty() || (date.starts_with('-') && date.bytes().skip(1).all(|c| c.is_ascii_digit() || c == b'-'))
                    })
                    || suffix.strip_prefix('-').is_some_and(|version| version.len() == 3 && version.bytes().all(|c| c.is_ascii_digit()))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_image_and_image_review_model_guards_classify_text_and_media_families() {
        assert!(is_non_image_model(ProviderKind::Gemini, "gemini-3.8-flash"));
        assert!(is_non_image_model(ProviderKind::Gemini, "models/gemini-2.5-flash"));
        assert!(is_non_image_model(ProviderKind::Gemini, "gemini-embedding-001"));
        assert!(is_non_image_model(ProviderKind::Gemini, "imagen-3.0-generate-002"));
        assert!(is_non_image_model(ProviderKind::Gemini, "models/imagen-3.0-generate-002"));
        assert!(is_non_image_model(ProviderKind::OpenAi, "gpt-4o"));
        assert!(is_non_image_model(ProviderKind::OpenAi, "o3-mini"));
        assert!(is_non_image_model(ProviderKind::Ollama, "qwen2.5:14b"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-3.1-flash-image"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-3.1-flash-image-preview"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-2.5-flash-image"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-nano-banana-2.1"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "gpt-image-1"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "dall-e-3"));
        assert!(!is_non_image_model(ProviderKind::ComfyUi, ""));
        assert!(!is_non_image_model(ProviderKind::Gemini, "image-model"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "test"));

        for non_gen in [
            "gemini-tuned-embed-only",
            "nomic-embed-text",
            "mxbai-embed-large:latest",
            "omni-moderation-latest",
            "gpt-4o-realtime-preview",
            "gpt-4o-transcribe",
            "lyria-realtime-exp",
            "sora-1",
        ] {
            assert!(is_non_text_model(non_gen), "expected {non_gen} to be classified non-text");
            assert!(is_non_image_model(ProviderKind::OpenAi, non_gen), "expected {non_gen} to be classified non-image");
            assert!(is_non_image_model(ProviderKind::Gemini, non_gen), "expected {non_gen} to be classified non-image");
        }
        assert!(!is_non_text_model("embedded-layout-helper"));

        assert!(supports_gemini_image_review(ProviderKind::Gemini, " gemini-3.8-flash "));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "models/gemini-3.1-pro-preview-customtools"));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "gemini-2.0-flash"));
        assert!(!supports_gemini_image_review(ProviderKind::OpenAi, "gpt-4o"));
        assert!(!supports_gemini_image_review(ProviderKind::Ollama, "llava"));
        assert!(!supports_gemini_image_review(ProviderKind::Gemini, "gemini-3.1-flash-image"));
    }
}
