//! Portable data contracts. Credentials belong to the native runtime, never these types.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use serde::{Deserialize, Serialize};

pub mod design;
pub mod protocol;
pub use design::{DesignCorrection, DesignImageSlot, DesignProfile, DesignStyleUse, DesignTextSlot, PageFamily};
pub use protocol::{
    AiExecutionMode, ArabicTashkeelMode, AttachmentRole, DiscoveredLocalEndpoint, DocumentGeometrySpec, DocumentIntentKind, ExecutionPacing,
    KnowledgePackSummary, MultiFormatOutputMode, SelectionContext, TaskAttachment, VisualDensityMode,
};
pub mod resources;
pub use resources::{GpuSnapshot, LocalResourcePolicy, ResourceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    Gemini,
    Ollama,
    LocalOpenAi,
    ComfyUi,
}

impl ProviderKind {
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Ollama | Self::LocalOpenAi | Self::ComfyUi)
    }

    pub const fn is_text_provider(self) -> bool {
        matches!(self, Self::OpenAi | Self::Gemini | Self::Ollama | Self::LocalOpenAi)
    }
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
    if ["dall-e", "gpt-image", "imagen", "veo", "whisper", "flux", "sdxl"]
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
                | "banana"
                | "omni"
                | "flux"
                | "sdxl"
                | "diffusion"
                | "rerank"
                | "tts"
                | "audio"
                | "embed"
                | "embedding"
                | "embeddings"
                | "embeddinggemma"
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

/// Identifies retired or shut-down Gemini / Imagen model identifiers so callers can surface
/// proactive migration hints toward current models (`gemini-3.8-flash` / `gemini-3.1-flash-image`).
pub fn is_retired_gemini_model(provider: ProviderKind, model: &str) -> bool {
    if provider != ProviderKind::Gemini {
        return false;
    }
    let normalized = model.trim().to_ascii_lowercase();
    let name = normalized.strip_prefix("models/").unwrap_or(&normalized);
    if name == "imagen" || name.starts_with("imagen-") || name.starts_with("imagen_") {
        return true;
    }
    [
        "gemini-1.0",
        "gemini-1.5",
        "gemini-2.0",
        "gemini-2.5",
        "gemini-3-pro-preview",
        "gemini-3-pro-image-preview",
        "gemini-3.1-flash-image-preview",
        "gemini-3.1-flash-lite-preview",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Identifies providers or model identifiers that cannot generate images so illustrated runs fail
/// before spending text-planning tokens.
pub fn is_non_image_model(provider: ProviderKind, model: &str) -> bool {
    if matches!(provider, ProviderKind::Ollama | ProviderKind::LocalOpenAi) {
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
    if ["text-embedding", "tts-", "whisper", "veo", "sora", "lyria"].iter().any(|prefix| model.starts_with(prefix)) {
        return true;
    }
    if model.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| {
        matches!(
            part,
            "embed"
                | "embedding"
                | "embeddings"
                | "embeddinggemma"
                | "moderation"
                | "rerank"
                | "transcribe"
                | "whisper"
                | "tts"
                | "realtime"
                | "audio"
                | "live"
                | "veo"
                | "sora"
                | "lyria"
                | "aqa"
        )
    }) {
        return true;
    }
    if provider == ProviderKind::Gemini {
        return !model.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| matches!(part, "image" | "images" | "banana"));
    }
    if model.starts_with("dall-e") || model.contains("stable-diffusion") {
        return false;
    }
    !model.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| {
        matches!(
            part,
            "image"
                | "images"
                | "banana"
                | "imagen"
                | "dalle"
                | "flux"
                | "sdxl"
                | "diffusion"
                | "playground"
                | "ideogram"
                | "recraft"
                | "midjourney"
                | "safetensors"
                | "ckpt"
                | "gguf"
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
    if (model == "gemini-4" || model.starts_with("gemini-4-") || model.starts_with("gemini-4."))
        && !is_non_text_model(model)
        && model.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_'))
    {
        return true;
    }
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

/// Returns `true` when a free-form prompt explicitly asks to generate, regenerate, redraw, or
/// re-illustrate raster artwork (as opposed to mentioning images in a caption-matching or layout audit).
pub fn prompt_explicitly_requests_image_generation(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    const NEGATIONS: &[&str] = &[
        "بدون توليد صور",
        "دون توليد صور",
        "لا تولد صور",
        "لا تولّد صور",
        "لا تعيد توليد",
        "لا تعد توليد",
        "لا تعيد رسم",
        "لا تعد رسم",
        "بدون إعادة توليد",
        "بدون اعادة توليد",
        "دون إعادة توليد",
        "بدون رسم صور",
        "without generating",
        "without regenerating",
        "without redrawing",
        "do not generate",
        "don't generate",
        "dont generate",
        "do not regenerate",
        "don't regenerate",
        "dont regenerate",
        "do not redraw",
        "don't redraw",
        "dont redraw",
        "do not re-illustrate",
        "don't re-illustrate",
        "do not create new image",
        "don't create new image",
        "do not create new illustration",
        "don't create new illustration",
        "do not draw new",
        "don't draw new",
        "never generate",
        "never regenerate",
        "avoid generating",
        "avoid regenerating",
        "no image generation",
        "no new images",
        "no new illustrations",
    ];
    if NEGATIONS.iter().any(|neg| lower.contains(neg)) {
        return false;
    }
    const EXPLICIT_PATTERNS: &[&str] = &[
        "أعد توليد الصور",
        "اعد توليد الصور",
        "إعادة توليد الصور",
        "اعادة توليد الصور",
        "أعد توليد الصورة",
        "اعد توليد الصورة",
        "إعادة توليد الصورة",
        "اعادة توليد الصورة",
        "ولّد صور",
        "ولد صور",
        "توليد صور",
        "توليد الصور",
        "أعد رسم الصور",
        "اعد رسم الصور",
        "إعادة رسم الصور",
        "اعادة رسم الصور",
        "ارسم صور",
        "ارسم رسوم",
        "توليد رسوم",
        "استبدل الصور بصور جديدة",
        "صمم صوراً جديدة",
        "صمم صورا جديدة",
        "regenerate image",
        "regenerate images",
        "regenerate the image",
        "regenerate the images",
        "regenerate illustration",
        "regenerate illustrations",
        "redraw image",
        "redraw images",
        "redraw illustration",
        "redraw illustrations",
        "re-illustrate",
        "generate new image",
        "generate new images",
        "generate image",
        "generate images",
        "generate illustration",
        "generate illustrations",
        "create new illustration",
        "create new illustrations",
        "draw new image",
        "draw new images",
    ];
    EXPLICIT_PATTERNS.iter().any(|pat| lower.contains(pat))
}

/// Normalizes user-entered or pasted file/folder paths (such as Windows "Copy as path" strings)
/// by removing Unicode BiDi/invisible formatting marks, trimming whitespace/control characters,
/// and unwrapping surrounding quotes so `std::fs` never fails with `os error 123`.
pub fn sanitize_user_path(raw: &str) -> String {
    let filtered: String = raw
        .chars()
        .filter(|&c| {
            !matches!(
                c,
                '\0' | '\r' | '\n' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
            )
        })
        .collect();
    let mut trimmed = filtered.trim();
    for _ in 0..4 {
        let prev = trimmed;
        if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            trimmed = inner.trim();
        } else if let Some(inner) = trimmed.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            trimmed = inner.trim();
        } else if let Some(inner) = trimmed.strip_prefix('\u{201C}').and_then(|s| s.strip_suffix('\u{201D}')) {
            trimmed = inner.trim();
        } else {
            trimmed = trimmed.trim_matches(|c| matches!(c, '"' | '\u{201C}' | '\u{201D}')).trim();
        }
        if trimmed == prev {
            break;
        }
    }
    trimmed.to_owned()
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
        assert!(is_non_image_model(ProviderKind::LocalOpenAi, "qwen3-14b-instruct"));
        assert!(is_non_image_model(ProviderKind::LocalOpenAi, "flux.1-dev"));
        assert!(is_non_image_model(ProviderKind::LocalOpenAi, ""));
        assert!(is_non_image_model(ProviderKind::OpenAi, "jina-clip-image-embed"));
        assert!(is_non_image_model(ProviderKind::Gemini, "jina-clip-image-embed"));
        for unknown_llm in
            ["anthropic/claude-sonnet-4-5", "openai/gpt-5.4", "codex-mini-latest", "grok-3", "phi-4", "qwen3", "llama-3", "mistral", "deepseek"]
        {
            assert!(is_non_image_model(ProviderKind::OpenAi, unknown_llm), "expected {unknown_llm} to be rejected as non-image");
            assert!(is_non_image_model(ProviderKind::Gemini, unknown_llm), "expected {unknown_llm} to be rejected as non-image");
        }
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-3.1-flash-image"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-3.1-flash-image-preview"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-2.5-flash-image"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "gemini-nano-banana-2.1"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "gpt-image-1"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "dall-e-3"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "black-forest-labs/flux.1-dev"));
        assert!(!is_non_image_model(ProviderKind::OpenAi, "stabilityai/stable-diffusion-xl-base-1.0"));
        assert!(!is_non_image_model(ProviderKind::ComfyUi, ""));
        assert!(!is_non_image_model(ProviderKind::ComfyUi, "juggernautXL_v9"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "image-model"));
        assert!(!is_non_image_model(ProviderKind::Gemini, "test-image"));
    }

    #[test]
    fn non_text_model_guards_classify_embed_audio_video_and_gemma_families() {
        for non_gen in [
            "gemini-tuned-embed-only",
            "nomic-embed-text",
            "mxbai-embed-large:latest",
            "embeddinggemma:300m",
            "omni-moderation-latest",
            "gpt-4o-realtime-preview",
            "gpt-4o-transcribe",
            "lyria-realtime-exp",
            "sora-1",
            "jina-clip-image-embed",
        ] {
            assert!(is_non_text_model(non_gen), "expected {non_gen} to be classified non-text");
            assert!(is_non_image_model(ProviderKind::OpenAi, non_gen), "expected {non_gen} to be classified non-image");
            assert!(is_non_image_model(ProviderKind::Gemini, non_gen), "expected {non_gen} to be classified non-image");
        }
        for non_text in ["gemini-nano-banana-2.1", "qwen2.5-omni-7b", "flux.1-schnell", "sdxl-lightning", "stable-diffusion-3.5"] {
            assert!(is_non_text_model(non_text), "expected {non_text} to be classified non-text");
        }
        for gemma_text in ["gemma-3-12b-it", "gemma-3-27b-it", "gemma2:9b", "gemma3:12b", "google/gemma-3-27b-it"] {
            assert!(!is_non_text_model(gemma_text), "expected {gemma_text} to be allowed as a text model");
            assert!(is_non_image_model(ProviderKind::Ollama, gemma_text));
            assert!(is_non_image_model(ProviderKind::LocalOpenAi, gemma_text));
            assert!(is_non_image_model(ProviderKind::OpenAi, gemma_text));
            assert!(is_non_image_model(ProviderKind::Gemini, gemma_text));
        }
        assert!(!is_non_text_model("embedded-layout-helper"));
        assert!(!is_non_text_model("imaginative-writer-7b"));
    }

    #[test]
    fn retired_gemini_and_image_review_model_guards_classify_families() {
        for retired in [
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "models/gemini-2.5-flash-image",
            "gemini-3-pro-preview",
            "gemini-3-pro-image-preview",
            "gemini-3.1-flash-image-preview",
            "gemini-3.1-flash-lite-preview",
            "imagen-3.0-generate-002",
        ] {
            assert!(is_retired_gemini_model(ProviderKind::Gemini, retired), "expected {retired} to be classified retired");
            assert!(!is_retired_gemini_model(ProviderKind::OpenAi, retired));
        }
        assert!(!is_retired_gemini_model(ProviderKind::Gemini, "gemini-3.8-flash"));
        assert!(!is_retired_gemini_model(ProviderKind::Gemini, "gemini-3.1-flash-image"));
        assert!(!is_retired_gemini_model(ProviderKind::Gemini, "gemini-4-argon"));

        assert!(supports_gemini_image_review(ProviderKind::Gemini, " gemini-3.8-flash "));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "models/gemini-3.1-pro-preview-customtools"));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "gemini-2.0-flash"));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "gemini-4-argon"));
        assert!(supports_gemini_image_review(ProviderKind::Gemini, "models/gemini-4.1-pro"));
        assert!(!supports_gemini_image_review(ProviderKind::Gemini, "gemini-4-flash-image"));
        assert!(!supports_gemini_image_review(ProviderKind::OpenAi, "gpt-4o"));
        assert!(!supports_gemini_image_review(ProviderKind::Ollama, "llava"));
        assert!(!supports_gemini_image_review(ProviderKind::LocalOpenAi, "llava"));
        assert!(!supports_gemini_image_review(ProviderKind::Gemini, "gemini-3.1-flash-image"));
    }

    #[test]
    fn explicit_image_generation_prompt_classifier_distinguishes_audit_from_redraw() {
        assert!(!prompt_explicitly_requests_image_generation(
            "ازل التشكيل من الحروف ونريد تبديل الخط العربي وايضا لاحظنا الكلمات وبين الصور ليس متطابقة فمثلا الصورة وتحتها الكلمة ليست متطابقة ارجو المراجعة والتدقيق"
        ));
        assert!(!prompt_explicitly_requests_image_generation(
            "Review all pages 1-32, fix page numbers, and check caption-to-image alignment without generating images."
        ));
        assert!(prompt_explicitly_requests_image_generation("أعد توليد الصور التالفة في الصفحات 3 و 7"));
        assert!(prompt_explicitly_requests_image_generation("Fix captions and regenerate images for the mismatched cards"));
        assert!(!prompt_explicitly_requests_image_generation("أعد توليد الصور بدون توليد صور مدفوعة"));
        assert!(!prompt_explicitly_requests_image_generation("Review captions, but do not regenerate the images."));
        assert!(!prompt_explicitly_requests_image_generation("Please don't generate the illustrations; only align labels."));
    }

    #[test]
    fn sanitize_user_path_strips_windows_quotes_and_bidi_marks() {
        assert_eq!(
            sanitize_user_path(
                r#"  "C:\Users\alara\Desktop\Starlight_Blossom_365_Planner_Skill_And_Script_Package\planners-journals-notebooks-studio"  "#
            ),
            r"C:\Users\alara\Desktop\Starlight_Blossom_365_Planner_Skill_And_Script_Package\planners-journals-notebooks-studio"
        );
        assert_eq!(sanitize_user_path("\u{202A}\"C:\\Users\\alara\\Downloads\\pkg\"\u{202C}"), r"C:\Users\alara\Downloads\pkg");
        assert_eq!(sanitize_user_path("'\u{200F}/tmp/my skill/SKILL.md\u{200E}'"), "/tmp/my skill/SKILL.md");
        assert_eq!(sanitize_user_path("“D:\\كتاب\\بلانر.designcraft”\r\n"), r"D:\كتاب\بلانر.designcraft");
    }

    #[test]
    fn execution_pacing_and_visual_density_round_trip_with_backward_compatible_defaults() {
        let legacy: crate::protocol::AiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.execution_pacing, ExecutionPacing::ReviewEveryBatch);
        assert_eq!(legacy.visual_density, VisualDensityMode::SmartHybrid);

        let mut updated = legacy;
        updated.execution_pacing = ExecutionPacing::AutoContinue;
        updated.visual_density = VisualDensityMode::PureNativeVector;
        let encoded = serde_json::to_string(&updated).unwrap();
        let decoded: crate::protocol::AiSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.execution_pacing, ExecutionPacing::AutoContinue);
        assert_eq!(decoded.visual_density, VisualDensityMode::PureNativeVector);
    }

    #[test]
    fn provider_kind_local_openai_classification_and_legacy_defaults() {
        assert_eq!(serde_json::to_string(&ProviderKind::LocalOpenAi).unwrap(), "\"local_open_ai\"");
        assert_eq!(serde_json::from_str::<ProviderKind>("\"local_open_ai\"").unwrap(), ProviderKind::LocalOpenAi);

        assert_eq!(
            [
                ProviderKind::OpenAi.is_local(),
                ProviderKind::Gemini.is_local(),
                ProviderKind::Ollama.is_local(),
                ProviderKind::LocalOpenAi.is_local(),
                ProviderKind::ComfyUi.is_local(),
            ],
            [false, false, true, true, true]
        );
        assert_eq!(
            [
                ProviderKind::OpenAi.is_text_provider(),
                ProviderKind::Gemini.is_text_provider(),
                ProviderKind::Ollama.is_text_provider(),
                ProviderKind::LocalOpenAi.is_text_provider(),
                ProviderKind::ComfyUi.is_text_provider(),
            ],
            [true, true, true, true, false]
        );

        let legacy_settings: crate::protocol::AiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy_settings.execution_pacing, ExecutionPacing::ReviewEveryBatch);
        assert_eq!(legacy_settings.execution_mode, AiExecutionMode::Auto);
        assert!(legacy_settings.hybrid_local_model.is_none());
        assert_eq!(legacy_settings.multi_format_mode, MultiFormatOutputMode::AlternateLayouts);
        assert!(legacy_settings.active_knowledge_packs.is_empty());

        let default_endpoint = DiscoveredLocalEndpoint::default();
        assert_eq!(default_endpoint.provider, ProviderKind::Ollama);
        assert!(!default_endpoint.reachable);
    }

    #[test]
    fn task_request_new_document_attachments_and_selection_round_trip() {
        let legacy_task_json = serde_json::json!({
            "prompt": "Create a 4-page brochure",
            "settings": {},
            "skill_ids": [],
            "target": {"kind": "new_a4", "pages": 4},
            "export_directory": null
        });
        let legacy_task: crate::protocol::TaskRequest = serde_json::from_value(legacy_task_json).unwrap();
        assert!(legacy_task.auto_select_skills);
        assert!(legacy_task.attachments.is_empty());
        assert!(legacy_task.knowledge_pack_ids.is_empty());
        assert!(legacy_task.selection_context.is_none());
        assert!(legacy_task.target_formats.is_empty());
        assert_eq!(
            [
                legacy_task.target.requested_pages(),
                crate::protocol::TaskTarget::CurrentDocument.requested_pages(),
                crate::protocol::TaskTarget::ContinueBook.requested_pages(),
            ],
            [Some(4), None, None]
        );

        let omitted_geometry: crate::protocol::TaskTarget = serde_json::from_value(serde_json::json!({"kind": "new_document", "pages": 12})).unwrap();
        assert_eq!(omitted_geometry, crate::protocol::TaskTarget::NewDocument { pages: 12, geometry: DocumentGeometrySpec::default() });
        assert_eq!(omitted_geometry.requested_pages(), Some(12));

        let custom_geometry = DocumentGeometrySpec {
            width_pt: 419.528,
            height_pt: 595.276,
            intent: DocumentIntentKind::Print,
            units: "mm".into(),
            facing_pages: Some(true),
            right_to_left: Some(true),
            margins_pt: Some([34.0, 34.0, 42.0, 34.0]),
            bleed_pt: 8.504,
            columns: 2,
            column_gutter_pt: 10.0,
            preset_label: "A5".into(),
        };
        let mut rich_task = legacy_task;
        rich_task.settings.execution_mode = AiExecutionMode::SmartHybrid;
        rich_task.settings.hybrid_local_model = Some(ModelSelection {
            provider: ProviderKind::LocalOpenAi,
            model: "qwen3-14b-instruct".into(),
            endpoint: Some("http://127.0.0.1:1234/v1".into()),
        });
        rich_task.settings.multi_format_mode = MultiFormatOutputMode::SeparateDocuments;
        rich_task.settings.active_knowledge_packs = vec!["brand-pack".into()];
        rich_task.target = crate::protocol::TaskTarget::NewDocument { pages: 16, geometry: custom_geometry.clone() };
        rich_task.attachments = vec![TaskAttachment { path: "logo.svg".into(), role: AttachmentRole::BrandLogo }];
        rich_task.knowledge_pack_ids = vec!["brand-pack".into()];
        rich_task.selection_context = Some(SelectionContext {
            active_page: Some(2),
            selected_item_ids: vec![101, 102],
            selected_labels: vec!["hero-title".into()],
            selected_kinds: vec!["text".into()],
            selected_story_id: Some(7),
            selected_text_snippet: Some("مرحبا".into()),
        });
        rich_task.target_formats = vec![custom_geometry];

        let encoded = serde_json::to_string(&rich_task).unwrap();
        let decoded: crate::protocol::TaskRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.settings.execution_mode, AiExecutionMode::SmartHybrid);
        assert_eq!(decoded.settings.multi_format_mode, MultiFormatOutputMode::SeparateDocuments);
        assert_eq!(decoded.target.requested_pages(), Some(16));
        assert_eq!(decoded.attachments.len(), 1);
        assert_eq!(decoded.attachments[0].role, AttachmentRole::BrandLogo);
        assert_eq!(decoded.selection_context.as_ref().and_then(|s| s.active_page), Some(2));
        assert_eq!(decoded.target_formats.len(), 1);
    }
}
