//! Data-only bridge between the swappable UI and the native production host.
use crate::{EngineAction, ModelSelection, OutputMode, ProviderKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Full user briefs are retained verbatim; model context and spending have separate admission guards.
pub const MAX_BRIEF_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetSpec {
    pub token_limit: u64,
    pub max_output_tokens: u32,
    pub max_requests: u32,
    pub repair_attempts: u32,
    pub max_cost_usd: Option<f64>,
    pub rates: Option<TokenRates>,
}
impl Default for BudgetSpec {
    fn default() -> Self {
        Self { token_limit: 256_000, max_output_tokens: 8192, max_requests: 4, repair_attempts: 1, max_cost_usd: None, rates: None }
    }
}
/// User-configured rates, not a claim about a provider invoice.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRates {
    pub input_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
    pub cache_write_per_million: Option<f64>,
    pub image_per_request: Option<f64>,
    /// Admission reserve for token-priced images, distinct from the measured estimate.
    #[serde(default)]
    pub image_reserve_usd: Option<f64>,
}

/// Controls Arabic vowelization (tashkeel) density during generation and polish.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArabicTashkeelMode {
    #[default]
    Auto,
    None,
    Light,
    Full,
}

/// Controls whether multi-batch book production stops for human review or continues autonomously.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPacing {
    #[default]
    ReviewEveryBatch,
    StopAfterFirstBatch,
    AutoContinue,
}

/// Controls decorative/editorial artwork density and caching across multi-page production.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualDensityMode {
    #[default]
    SmartHybrid,
    PureNativeVector,
    RichPerPageImages,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiSettings {
    /// Real book spreads; existing documents always keep their current binding.
    pub facing_pages: bool,
    /// None infers direction from content language; explicit preferences remain available.
    pub right_to_left_binding: Option<bool>,
    /// Additional planning requests may be billed; opt in explicitly, including legacy settings.
    pub allow_paid_repairs: bool,
    /// Optional billed visual inspection using the selected supported vision text model.
    pub verify_generated_images: bool,
    /// None selects a relevant style locally; Some pins this skill's design profile at task start.
    pub design_style: Option<String>,
    pub text_model: ModelSelection,
    pub image_model: ModelSelection,
    pub output_mode: OutputMode,
    pub budget: BudgetSpec,
    pub image_rates: Option<TokenRates>,
    pub comfy_workflow: Option<Value>,
    /// Zero retains one-shot production for at most eight pages. Larger books require batches.
    pub pages_per_batch: u32,
    pub resources: crate::LocalResourcePolicy,
    /// Controls Arabic vowelization (tashkeel) density during generation and polish.
    #[serde(default)]
    pub arabic_tashkeel: ArabicTashkeelMode,
    /// Optional preferred Arabic font family (e.g. "Al-Jazeera-Arabic", "Cairo"); None picks best installed face.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arabic_font: Option<String>,
    /// Controls whether multi-batch book production stops for human review or continues autonomously.
    #[serde(default)]
    pub execution_pacing: ExecutionPacing,
    /// Controls decorative illustration reuse vs pure native vector vs per-page images.
    #[serde(default)]
    pub visual_density: VisualDensityMode,
}
impl Default for AiSettings {
    fn default() -> Self {
        Self {
            facing_pages: false,
            right_to_left_binding: None,
            allow_paid_repairs: false,
            verify_generated_images: false,
            design_style: None,
            text_model: ModelSelection { provider: ProviderKind::Ollama, model: String::new(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: None },
            output_mode: OutputMode::NativeOnly,
            budget: BudgetSpec::default(),
            image_rates: None,
            comfy_workflow: None,
            pages_per_batch: 2,
            resources: crate::LocalResourcePolicy::default(),
            arabic_tashkeel: ArabicTashkeelMode::Auto,
            arabic_font: None,
            execution_pacing: ExecutionPacing::ReviewEveryBatch,
            visual_density: VisualDensityMode::SmartHybrid,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskTarget {
    CurrentDocument,
    /// Explicit new authority to fill the contiguous blank suffix of the selected book.
    /// Persisted checkpoints never grant this authority.
    ContinueBook,
    NewA4 {
        pages: u32,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequest {
    pub prompt: String,
    pub settings: AiSettings,
    pub skill_ids: Vec<String>,
    #[serde(default = "auto_skills")]
    pub auto_select_skills: bool,
    pub target: TaskTarget,
    pub export_directory: Option<String>,
}
fn auto_skills() -> bool {
    true
}
/// Saved progress metadata preserved when binding a new-book draft to continuation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SavedProductionProgress {
    #[serde(default)]
    pub completed_pages: u32,
    #[serde(default)]
    pub target_total_pages: Option<u32>,
}
/// Recoverable instructions only. Reopening never restores a live worker or proof approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionDraft {
    pub id: String,
    pub request: TaskRequest,
    pub document_uid: Option<u64>,
    pub document_path: Option<String>,
    pub document_title: String,
    #[serde(default)]
    pub progress: SavedProductionProgress,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePlacement {
    pub prompt: String,
    pub page: usize,
    pub rect: [f64; 4],
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    /// Additional native frames sharing one generated single-subject source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repeat_rects: Vec<[f64; 4]>,
    /// Host-compiled groups inside `rect`; mutually exclusive with explicit cells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repeat_groups: Vec<u8>,
}
impl ImagePlacement {
    /// Group declarations are normalized before consumers use these cells.
    pub fn instance_count(&self) -> usize {
        self.repeat_rects.len().saturating_add(1)
    }
    pub fn all_rects(&self) -> impl Iterator<Item = [f64; 4]> + '_ {
        std::iter::once(self.rect).chain(self.repeat_rects.iter().copied())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionPlan {
    pub title: String,
    pub commands: Vec<EngineAction>,
    #[serde(default)]
    pub images: Vec<ImagePlacement>,
}

/// Ephemeral plaintext credential buffer; zeroized automatically whenever dropped
/// (including early rejection of [`AiRequest::SetApiKey`] or [`AiRequest::SaveApiKey`]).
#[derive(Clone, Default)]
pub struct EphemeralKey(String);

impl EphemeralKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.0)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for EphemeralKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for EphemeralKey {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Drop for EphemeralKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// Deliberately not serializable or Debug: SetApiKey carries an ephemeral secret.
#[derive(Clone)]
pub enum AiRequest {
    Poll,
    Start(TaskRequest),
    Cancel,
    /// Human UI gesture only. Never decoded from the control/MCP command channel.
    ApproveBatch {
        job_id: String,
        checkpoint: u32,
    },
    /// Retry an unpublished failed group with its original cumulative admission ledger.
    RetryBatch {
        job_id: String,
    },
    /// Re-run preflight and proof rendering for an edited checkpoint document without paid calls.
    RefreshBatchProofs {
        job_id: String,
    },
    /// Pause autonomous batch continuation cleanly after the active batch completes.
    PauseBatch {
        job_id: String,
    },
    /// Resume a paused batch job from the next pending batch.
    ResumeBatch {
        job_id: String,
    },
    UpdateSettings(AiSettings),
    SetApiKey {
        provider: ProviderKind,
        key: EphemeralKey,
    },
    SaveApiKey {
        selection: ModelSelection,
        key: EphemeralKey,
    },
    RemoveApiKey {
        selection: ModelSelection,
    },
    RefreshCredentials,
    RefreshResources,
    StartLocalOllama {
        executable: String,
    },
    StopLocalOllama,
    OpenPrintProof {
        token: String,
    },
    ListModels {
        selection: ModelSelection,
    },
    ImportSkill {
        path: String,
    },
    InspectSkill {
        id: String,
        revision: String,
    },
    PrepareSkills {
        prompt: String,
    },
    /// Local planning preview only; never launches a provider or grants production authority.
    PreviewProduction(TaskRequest),
    SaveSkillDraft {
        id: String,
        skill_md: String,
        manifest_json: String,
    },
    AddDesignCorrection {
        id: String,
        revision: String,
        correction: crate::DesignCorrection,
    },
    GenerateSkill {
        prompt: String,
        reference_paths: Vec<String>,
        settings: AiSettings,
        design_style: bool,
    },
    GenerateSkillFromDocument {
        prompt: String,
        settings: AiSettings,
        design_style: bool,
    },
    RunSkillSample {
        id: String,
        revision: String,
    },
    RunDesignTemplate {
        id: String,
        revision: String,
        template: String,
    },
    ApproveSkill {
        id: String,
        revision: String,
    },
    SetSkillEnabled {
        id: String,
        enabled: bool,
    },
    RollbackSkill {
        id: String,
        revision: String,
    },
    ExportSkill {
        id: String,
        revision: String,
        path: String,
    },
    CreateScienceSample {
        directory: String,
    },
    ExportDocument {
        path: String,
        pdf: bool,
    },
    ExportIndd {
        path: String,
    },
    ExportIdml {
        path: String,
    },
    PrintDocument {
        printer: Option<String>,
        grayscale: bool,
        duplex: String,
        request_id: String,
    },
    PreparePrint {
        printer: Option<String>,
        grayscale: bool,
        duplex: String,
    },
    SubmitPreparedPrint {
        token: String,
        request_id: String,
    },
    SavePreparedPrint {
        token: String,
        path: String,
    },
    ConfirmImageRegeneration {
        job_id: String,
    },
    DismissImageRegeneration {
        job_id: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageSummary {
    pub requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub estimated_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub has_unreported_usage: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialStatus {
    pub selection: ModelSelection,
    pub persisted: bool,
    /// Actual credential presence for this endpoint, from protected storage,
    /// an authorized session key or the canonical provider environment.
    pub available: bool,
    /// Whether this refresh could access protected storage; independent from
    /// whether a key exists or an in-memory credential is usable.
    #[serde(default)]
    pub protected_storage_available: bool,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalServerStatus {
    pub owned: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub endpoint: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrintProof {
    pub token: String,
    pub sha256: String,
    pub bytes: u64,
    pub pages: u32,
    pub manifest: Value,
    pub pdf_path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchReport {
    pub batch: u32,
    #[serde(default)]
    pub title: String,
    /// Zero-based first native page/spread in this non-facing-page document.
    pub page_start: u32,
    pub page_count: u32,
    pub status: String,
    pub elapsed_ms: u64,
    pub usage: UsageSummary,
    pub proof_paths: Vec<String>,
    pub proof_sha256: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchSnapshot {
    pub job_id: String,
    pub total_pages: u32,
    pub pages_per_batch: u32,
    /// True when a new facing-pages 2-page batch job aligns Batch 1 to the single cover sheet.
    #[serde(default)]
    pub spread_aligned: bool,
    /// One-based batch number, including a currently running batch.
    pub current_batch: u32,
    pub total_batches: u32,
    pub completed_pages: u32,
    pub status: String,
    /// Live failed-job authority only; always false for recovered checkpoints.
    #[serde(default)]
    pub retryable: bool,
    /// True when an autonomous batch job has been paused between batches and can be resumed.
    #[serde(default)]
    pub paused: bool,
    pub checkpoint: u32,
    pub document_uid: Option<u64>,
    pub document_revision: Option<u64>,
    pub cumulative_usage: UsageSummary,
    pub elapsed_ms: u64,
    pub batches: Vec<BatchReport>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskReport {
    #[serde(default)]
    pub design_style: Option<crate::DesignStyleUse>,
    pub id: String,
    pub status: String,
    pub title: String,
    pub provider: ProviderKind,
    pub model: String,
    #[serde(default)]
    pub settings: Option<AiSettings>,
    pub skills: Vec<(String, String)>,
    pub usage: UsageSummary,
    pub elapsed_ms: u64,
    pub actions: usize,
    pub issues: Vec<Value>,
    pub artifacts: Vec<String>,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillCard {
    #[serde(default)]
    pub design_profile: Option<crate::DesignProfile>,
    pub id: String,
    pub name: String,
    pub description: String,
    pub revision: String,
    pub approved: bool,
    #[serde(default)]
    pub automatically_validated: bool,
    #[serde(default)]
    pub trusted_source: bool,
    pub active: bool,
    pub enabled: bool,
    pub compatible: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillReview {
    pub id: String,
    pub revision: String,
    pub skill_md: String,
    pub manifest_json: String,
    pub sample_path: Option<String>,
    pub sample_valid: bool,
    pub files: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingImageApproval {
    pub job_id: String,
    #[serde(default)]
    pub document_uid: u64,
    pub count: usize,
    pub provider: ProviderKind,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub summary: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AiSnapshot {
    pub available: bool,
    #[serde(default)]
    pub production_preview: Option<Value>,
    pub local_server: Option<LocalServerStatus>,
    pub busy: bool,
    pub stage: String,
    pub progress: f32,
    /// Current task's measured cumulative ledger, refreshed after each provider call.
    #[serde(default)]
    pub current_usage: UsageSummary,
    /// Local fitting decisions and production checks; never provider credentials.
    #[serde(default)]
    pub current_issues: Vec<Value>,
    pub message: String,
    pub settings: AiSettings,
    #[serde(default)]
    pub production_draft: Option<ProductionDraft>,
    pub skills: Vec<SkillCard>,
    #[serde(default)]
    pub skill_warnings: Vec<String>,
    pub review: Option<SkillReview>,
    pub models: Vec<String>,
    pub reports: Vec<TaskReport>,
    #[serde(default)]
    pub batch: Option<BatchSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_image_approval: Option<PendingImageApproval>,
    #[serde(default)]
    pub credentials: Vec<CredentialStatus>,
    #[serde(default)]
    pub resources: Option<crate::ResourceSnapshot>,
    #[serde(default)]
    pub print_proof: Option<PrintProof>,
}

#[cfg(test)]
mod ephemeral_key_tests {
    use super::*;

    #[test]
    fn ephemeral_key_take_moves_and_leaves_empty_buffer_while_drop_completes_cleanly() {
        let mut k = EphemeralKey::from("sk-synthetic-test-secret");
        assert_eq!(k.as_str(), "sk-synthetic-test-secret");
        let moved = k.take();
        assert_eq!(moved, "sk-synthetic-test-secret");
        assert!(k.as_str().is_empty());

        let rejected = AiRequest::SetApiKey { provider: ProviderKind::Gemini, key: "sk-dropped-early".into() };
        drop(rejected);
        let rejected_save = AiRequest::SaveApiKey {
            selection: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-4.1-mini".into(), endpoint: None },
            key: String::from("sk-dropped-save").into(),
        };
        drop(rejected_save);
    }
}
