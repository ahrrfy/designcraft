//! Off-UI generation. This module never publishes documents or changes the skill registry.
use crate::ledger::UsageLedger;
use crate::references::{self as reference_loader, ReferenceMaterial};
use crate::vault::{SessionKey, selected_key};
use base64::Engine;
use designcraft_ai_providers::{self as providers, ComfyRequest, GenerationRequest, GenerationResponse, ImageInput, ImageRequest, ImageResponse};
use designcraft_ai_skills::{CachedAnalysis, NativeManifest, PackageFiles, SkillRegistry, SkillSnapshot};
use designcraft_ai_types::protocol::{AiSettings, BudgetSpec, ImagePlacement, ProductionPlan, TaskRequest, UsageSummary};
use designcraft_ai_types::{EngineAction, ModelSelection, OutputMode, ProviderKind};
use designcraft_engine::{DocState, Prefs, Session};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

const MAX_JSON: usize = 4 * 1024 * 1024;
const MAX_PROMPT: usize = 4 * 1024 * 1024;
// Raster admission is independent of model text/JSON size. Inline placements
// count each view; the source/cache budget counts a shared source only once.
const MAX_RASTER_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNIQUE_RASTER_BYTES: usize = 32 * 1024 * 1024;
const MAX_INLINE_RASTER_BYTES: usize = 32 * 1024 * 1024;
const MAX_RASTER_PIXELS: u64 = 20_000_000;
const MAX_IMAGES: usize = 8;
const MAX_ACTIONS: usize = 512;
const MAX_REVIEW_PAGES: u32 = 1000;
const MAX_GROUP_PLAN_BYTES: usize = 256 * 1024 * 1024;
const MAX_PROOF_BYTES: usize = 256 * 1024 * 1024;
const MAX_PROOFS_BYTES: usize = 512 * 1024 * 1024;
const ANALYZER: &str = "designcraft.visual-style.v1";

mod catalog;
mod images;
mod plan;
mod runner;
mod skills;
#[cfg(test)]
mod tests;

#[allow(unused_imports)]
use self::catalog::{
    EDITORIAL_POLICY, NATIVE_POLICY, action_schema, allowed_commands, batch_command, compact_catalog, native_result_contract, native_result_shape,
    parse_json, parse_plan, plan_schema, production_system, reference_source, repair_prompt, template_repair_prompt, validate_identifier_references,
};
pub(crate) use self::catalog::{
    batch_catalog, compact_catalog_and_skills_for_agent, compact_skill_guidance, compact_skill_guidance_limit, validate_actions,
};
#[allow(unused_imports)]
use self::images::{
    BoundedImageBytes, authored_image_frame, declared_image_slot, existing_document_image_frame, gemini_image_ratio, generated_asset_name,
    illustration_style, image_command_budget, image_memory_key, image_request_size, image_review_identity, image_source_ratio, image_view_fitting,
    openai_image_size, preflight_profile_image_ratios, prepare_generated_image, project_image_requests, project_slice_image_requests,
    resolved_slot_view_rect, review_projection_image, validate_image, validate_image_dimensions, validate_image_review_settings,
    validate_monetary_image_rates, validate_webp_chunks,
};
#[cfg(test)]
pub(crate) use self::plan::simulate_group_scoped;
#[allow(unused_imports)]
use self::plan::{
    StoryKey, arabic_vowel_mark, auto_preferred_arabic_font_contract, batch_proofs_from_draft, batch_proofs_with_layout,
    check_opened_document_geometry, check_opened_document_geometry_with_policy, context_has_vocalized_copy, continuation_style_geometry_differs,
    explicit_arabic_font_contract, extract_arabic_fonts_from_prompt, generic_page_capacity, identical_layout_requested, internal_template_context,
    model_needs_thinking_margin, opened_document_geometry, prepare_editorial_plan, prepare_editorial_plan_with_mode, previous_template_family,
    push_proof, resolve_installed_arabic_font_request, resolve_story_key, review_end, review_proofs, simulate_styled_with_ppi,
    strip_arabic_vowels_with_byte_map, template_family_for_labels, unbound_math_request, validate_previous_body_layout,
    validate_template_family_sequence,
};
#[cfg(test)]
#[allow(unused_imports)]
use self::plan::{batch_proofs, simulate, simulate_scoped, simulate_styled};
pub(crate) use self::plan::{
    ensure_no_new_low_resolution, marker_only_sections_within_scope, prompt_requests_document_geometry_edit, render_live_batch_proofs,
    simulate_group_scoped_with_ppi, validate_batch_plan, validate_batch_result,
};
pub(crate) use self::runner::Runner;
#[allow(unused_imports)]
use self::skills::{
    AnalysisDraft, PageTemplateDraft, SkillDraft, design_profile_schema, package_draft, reference_prompt, skill_schema, validate_skill_id,
};

#[cfg(test)]
pub(crate) static LOCAL_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy, Debug, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchScope {
    pub start: u32,
    pub count: u32,
}

#[derive(Clone, Copy)]
enum GeometryCheck<'a> {
    Engine,
    Profile(&'a designcraft_ai_types::DesignProfile),
    OpenedDocument { allow_geometry_edits: bool },
}

// Each job deliberately owns an isolated document/settings snapshot for its thread.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Work {
    Indesign(crate::indesign::PreparedIndesignExport),
    Production {
        request: TaskRequest,
        context: Value,
        catalog: Value,
        skills: Vec<SkillSnapshot>,
        base: DocState,
        prefs: Prefs,
    },
    DeferredImages {
        title: String,
        batches: Vec<crate::doc_tools::DeferredImagePlan>,
        settings: AiSettings,
        style_profile: Option<designcraft_ai_types::DesignProfile>,
        cache_root: Option<PathBuf>,
        min_ppi: f64,
        print_contract: Option<crate::print_contract::PrintContract>,
        base: DocState,
        prefs: Prefs,
    },
    Skill {
        prompt: String,
        reference_paths: Vec<String>,
        context: Option<Value>,
        settings: AiSettings,
        catalog: Value,
        registry_root: PathBuf,
        design_style: bool,
    },
    Models(ModelSelection),
    Resources,
}
pub(crate) enum Product {
    Indesign(crate::indesign::IndesignExport),
    Plan(ProductionPlan),
    Agentic {
        title: String,
        summary: String,
        chunks: Vec<Vec<EngineAction>>,
        deferred_images: Vec<crate::doc_tools::DeferredImagePlan>,
        scheduled_formats: Option<(Vec<designcraft_ai_types::protocol::DocumentGeometrySpec>, designcraft_ai_types::protocol::MultiFormatOutputMode)>,
    },
    BatchPlan {
        plan: ProductionPlan,
        proofs: Vec<ProofArtifact>,
    },
    BatchGroup {
        plans: Vec<ProductionPlan>,
        proofs: Vec<ProofArtifact>,
    },
    Skill {
        files: PackageFiles,
        analyses: Vec<CachedAnalysis>,
    },
    Models(Vec<String>),
    Resources(designcraft_ai_types::ResourceSnapshot),
}
pub(crate) struct ProofArtifact {
    pub name: String,
    pub bytes: Vec<u8>,
}
pub(crate) struct Outcome {
    pub result: Result<Product, String>,
    pub usage: UsageSummary,
}
pub(crate) enum Message {
    Usage(UsageSummary),
    Diagnostic(Value),
    Progress {
        stage: String,
        fraction: f32,
    },
    Finished(Outcome),
    /// A batch returns the actual admission ledger, including conservative unknown charges.
    FinishedWithLedger(Outcome, Box<UsageLedger>),
}
pub(crate) struct Worker {
    pub receiver: Receiver<Message>,
    pub cancel: Arc<AtomicBool>,
}

/// Session keys are owned only by the worker closure and are never formatted or persisted.
pub(crate) fn spawn(
    work: Work,
    keys: Vec<SessionKey>,
    roots: Option<Arc<designcraft_engine::file_access::AutomationRoots>>,
) -> Result<Worker, String> {
    spawn_inner(work, keys, roots, None)
}

pub(crate) fn spawn_with_ledger(
    work: Work,
    keys: Vec<SessionKey>,
    roots: Option<Arc<designcraft_engine::file_access::AutomationRoots>>,
    ledger: UsageLedger,
) -> Result<Worker, String> {
    spawn_inner(work, keys, roots, Some(ledger))
}

fn spawn_inner(
    work: Work,
    keys: Vec<SessionKey>,
    roots: Option<Arc<designcraft_engine::file_access::AutomationRoots>>,
    prior_ledger: Option<UsageLedger>,
) -> Result<Worker, String> {
    #[cfg(test)]
    let test_memory = crate::resources::test_admission_snapshot();
    let (sender, receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancellation = cancel.clone();
    std::thread::Builder::new()
        .name("designcraft-ai-worker".into())
        .spawn(move || {
            #[cfg(test)]
            let _test_memory = test_memory.map(crate::resources::TestAdmissionGuard::install);
            let budget = match &work {
                Work::Production { request, .. } => request.settings.budget.clone(),
                Work::DeferredImages { settings, .. } | Work::Skill { settings, .. } => settings.budget.clone(),
                Work::Models(_) | Work::Resources | Work::Indesign(_) => BudgetSpec::default(),
            };
            let batched = prior_ledger.is_some();
            let external_export = matches!(&work, Work::Indesign(_));
            let mut ledger = match prior_ledger.map(Ok).unwrap_or_else(|| UsageLedger::new(budget)) {
                Ok(ledger) => ledger,
                Err(message) => {
                    let _ = sender.send(Message::Finished(Outcome { result: Err(message), usage: UsageSummary::default() }));
                    return;
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut runner = Runner {
                    cache_root: None,
                    print: None,
                    planning_only: false,
                    ledger: &mut ledger,
                    keys: &keys,
                    cancel: &cancellation,
                    sender: &sender,
                    provider: &HttpProvider,
                };
                designcraft_engine::file_access::confine(roots.as_ref(), || runner.run(work))
            }))
            .unwrap_or_else(|_| Err("AI worker could not safely complete the task".into()));
            // An external export decides cancellation before publishing its output. A late
            // cancel cannot hide an already committed file from the user's artifact report.
            let result = if cancellation.load(Ordering::Relaxed) && !external_export { Err("AI request cancelled".into()) } else { result };
            let outcome = Outcome { result, usage: ledger.summary() };
            let message = if batched { Message::FinishedWithLedger(outcome, Box::new(ledger)) } else { Message::Finished(outcome) };
            let _ = sender.send(message);
        })
        .map_err(|_| "Cannot start AI worker thread".to_string())?;
    Ok(Worker { receiver, cancel })
}

pub(crate) trait Provider {
    fn generate(
        &self,
        selection: &ModelSelection,
        key: Option<&str>,
        request: &GenerationRequest,
        cancel: &AtomicBool,
    ) -> Result<GenerationResponse, String>;
    fn supports_agent_loop(&self) -> bool {
        false
    }
    fn agent_turn(
        &self,
        _selection: &ModelSelection,
        _key: Option<&str>,
        _request: &providers::AgentTurnRequest,
        _cancel: &AtomicBool,
    ) -> Result<providers::AgentTurnResponse, String> {
        Err("Provider does not implement multi-turn agent loop".into())
    }
    fn images(&self, selection: &ModelSelection, key: Option<&str>, request: &ImageRequest, cancel: &AtomicBool) -> Result<ImageResponse, String>;
    fn models(&self, selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<String>, String>;
    fn model_infos(&self, selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<providers::ModelInfo>, String> {
        self.models(selection, key, cancel)
            .map(|ids| ids.into_iter().map(|id| providers::ModelInfo { id, supports_generate_content: None }).collect())
    }
}
pub(crate) fn filter_discovered_models(provider: ProviderKind, models: Vec<providers::ModelInfo>) -> Vec<String> {
    models
        .into_iter()
        .filter(|model| provider != ProviderKind::Gemini || model.supports_generate_content != Some(false))
        .map(|model| model.id)
        .collect()
}
struct HttpProvider;
impl Provider for HttpProvider {
    fn generate(
        &self,
        selection: &ModelSelection,
        key: Option<&str>,
        request: &GenerationRequest,
        cancel: &AtomicBool,
    ) -> Result<GenerationResponse, String> {
        providers::generate(selection, key, request, cancel)
    }
    fn supports_agent_loop(&self) -> bool {
        true
    }
    fn agent_turn(
        &self,
        selection: &ModelSelection,
        key: Option<&str>,
        request: &providers::AgentTurnRequest,
        cancel: &AtomicBool,
    ) -> Result<providers::AgentTurnResponse, String> {
        providers::generate_agent_turn(selection, key, request, cancel)
    }
    fn images(&self, selection: &ModelSelection, key: Option<&str>, request: &ImageRequest, cancel: &AtomicBool) -> Result<ImageResponse, String> {
        providers::generate_images(selection, key, request, cancel)
    }
    fn models(&self, selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<String>, String> {
        self.model_infos(selection, key, cancel).map(|models| filter_discovered_models(selection.provider, models))
    }
    fn model_infos(&self, selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<providers::ModelInfo>, String> {
        providers::list_models(selection, key, cancel)
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { Err("AI request cancelled".into()) } else { Ok(()) }
}
fn guarded<T>(call: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).unwrap_or_else(|_| Err("AI provider could not safely complete the request".into()))
}
fn validate_prompt(prompt: &str) -> Result<(), String> {
    if prompt.trim().is_empty() || prompt.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES {
        Err(format!("AI request must contain 1 to {} UTF-8 bytes", designcraft_ai_types::protocol::MAX_BRIEF_BYTES))
    } else {
        Ok(())
    }
}
fn bounded_json(value: &Value, limit: usize) -> Result<String, String> {
    let mut stack = vec![(value, 0usize)];
    let mut size = 0usize;
    let mut count = 0usize;
    while let Some((value, depth)) = stack.pop() {
        count = count.saturating_add(1);
        if depth > 24 || count > 65536 {
            return Err("AI context nesting or item limit exceeded".into());
        }
        match value {
            Value::String(text) => size = size.saturating_add(text.len()),
            Value::Array(values) => stack.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => {
                for (key, value) in values {
                    size = size.saturating_add(key.len());
                    stack.push((value, depth + 1));
                }
            }
            _ => {}
        }
        if size > limit {
            return Err("AI context exceeds its byte limit".into());
        }
    }
    let encoded = serde_json::to_string(value).map_err(|e| e.to_string())?;
    if encoded.len() > limit {
        return Err("AI context exceeds its encoded byte limit".into());
    }
    Ok(encoded)
}
