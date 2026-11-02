//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{
    AiRequest, AiSettings, AiSnapshot, BatchSnapshot, ProductionDraft, SelectionContext, SkillCard, TaskAttachment, TaskRequest, TaskTarget,
    TokenRates,
};
use designcraft_ai_types::{
    AiExecutionMode as ExecutionMode, ArabicTashkeelMode, AttachmentRole, DocumentGeometrySpec, DocumentIntentKind as DocumentIntentSpec,
    ExecutionPacing, ModelSelection, MultiFormatOutputMode, OutputMode, ProviderKind, VisualDensityMode, is_non_image_model, sanitize_user_path,
    supports_gemini_image_review,
};
use serde_json::Value;

mod panel_local;
mod panel_production;
mod panel_providers;
mod panel_skills;
mod panel_usage;
mod state;
#[cfg(test)]
mod tests;

use panel_local::local_ai;
use panel_production::{elapsed, production, production_blocking_issue, selectable_skill_cards};
use panel_providers::providers;
use panel_skills::skills;
use panel_usage::usage;
pub use state::AiUiState;
use state::{
    CompanionProof, DiagnosticAlertKind, MAX_REQUEST_ALLOWANCE, MAX_TOKEN_LIMIT, OpenDocument, PromptAnalysisCache, RequestEstimate, TokenEstimate,
    automatic_proof_binding, explicit_script_pages, facing_proof_pair, has_configured_rates, is_skill_scoped_error, natural_prompt_pages,
    preset_a4_portrait, preset_banner_widescreen, preset_business_card, preset_certificate_a4_landscape, preset_poster_a3, preset_social_square,
    preset_social_story,
};
pub(crate) use state::{is_non_text_model, is_retired_gemini_text_model};

/// Typed UI path shared by all panel buttons. Credentials move directly to the host without JSON,
/// logs, serialization, document metadata or preference storage.
pub fn dispatch(app: &mut DesignApp, request: AiRequest) -> Result<Value, String> {
    if !human_review_input(app)
        && matches!(
            &request,
            AiRequest::ApproveSkill { .. }
                | AiRequest::ApproveBatch { .. }
                | AiRequest::ConfirmImageRegeneration { .. }
                | AiRequest::SetApiKey { .. }
                | AiRequest::SaveApiKey { .. }
                | AiRequest::RemoveApiKey { .. }
                | AiRequest::RefreshCredentials
                | AiRequest::PrintDocument { .. }
                | AiRequest::SubmitPreparedPrint { .. }
                | AiRequest::OpenPrintProof { .. }
                | AiRequest::StartLocalOllama { .. }
                | AiRequest::StopLocalOllama
        )
    {
        return Err("This action requires a human gesture in the desktop panel; control-generated input is not approval.".into());
    }
    if let AiRequest::ApproveBatch { job_id, checkpoint } = &request {
        let gesture = app.ui.ai.batch_approval_gesture.take();
        app.ui.ai.review_input_is_human = human_review_input(app);
        app.ui.ai.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
        if !gesture.is_some_and(|(key, ctx, frame)| app.ui.ai.proof_key.as_ref() == Some(&key) && ctx.cumulative_frame_nr() == frame)
            || !app.ui.ai.can_approve_batch()
            || !app.ui.ai.proof_key.as_ref().is_some_and(|key| key.job_id == *job_id && key.checkpoint == *checkpoint)
        {
            return Err("Review and confirm every page of the current document proof before approving this checkpoint.".into());
        }
        app.ui.ai.reviewed_proofs.clear();
    }
    let Some(host) = app.services.ai.as_mut() else {
        let e = "Native AI host is unavailable in this build.".to_string();
        app.ui.ai.error = e.clone();
        return Err(e);
    };
    if matches!(&request, AiRequest::InspectSkill { .. }) {
        app.ui.ai.review_key = None;
    }
    if matches!(&request, AiRequest::RunSkillSample { .. } | AiRequest::InspectSkill { .. }) {
        app.ui.ai.preview_path = None;
        app.ui.ai.preview = None;
    }
    app.ui.ai.error.clear();
    if let AiRequest::ListModels { selection } = &request
        && !app.ui.ai.task_active()
    {
        app.ui.ai.pending_models = Some(selection.clone());
    }
    let updated_settings = matches!(&request, AiRequest::UpdateSettings(_));
    let refreshed_proofs = matches!(&request, AiRequest::RefreshBatchProofs { .. });
    let production_request = if let AiRequest::Start(request) = &request { Some(request.clone()) } else { None };
    let snapshot = host(&mut app.session, request);
    let result = serde_json::to_value(&snapshot).map_err(|e| e.to_string());
    app.ui.ai.accept(snapshot);
    if refreshed_proofs {
        app.ui.ai.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
    }
    if updated_settings && app.ui.ai.snapshot.stage != "failed" {
        let next_settings = app.ui.ai.snapshot.settings.clone();
        app.ui.ai.sync_settings(next_settings);
    }
    if let Some(alert) = app.ui.ai.active_diagnostic_alert() {
        app.ui.status = format!("⚠ {} | {}", alert.title_ar, alert.title_en);
    }
    if let Some(request) = production_request {
        app.ui.ai.load_production_request(&request);
    }
    result
}

pub(super) fn human_review_input(app: &DesignApp) -> bool {
    !app.in_control && !app.control_input_frame.load(std::sync::atomic::Ordering::Relaxed)
}

/// Control channel / MCP command path: `ai.request {method, ...}`. Methods match the snake_case
/// AiRequest variants. `start` accepts `request: TaskRequest`; settings are `settings: AiSettings`;
/// model discovery accepts `selection: ModelSelection`. All other fields match the typed variant.
/// Credentials, human approvals, physical print submission and owned-process launch use only
/// typed panel controls. Secret input never enters control transcripts.
/// Control/MCP automation may tighten spending budgets, never raise them above the user's ceiling.
pub(super) fn clamp_control_settings(ceiling: &AiSettings, settings: &mut AiSettings) {
    settings.budget.max_requests = settings.budget.max_requests.min(ceiling.budget.max_requests);
    settings.budget.token_limit = settings.budget.token_limit.min(ceiling.budget.token_limit);
    settings.budget.max_output_tokens = settings.budget.max_output_tokens.min(ceiling.budget.max_output_tokens);
    settings.budget.repair_attempts = settings.budget.repair_attempts.min(ceiling.budget.repair_attempts);
    if let Some(cap) = ceiling.budget.max_cost_usd {
        settings.budget.max_cost_usd = Some(settings.budget.max_cost_usd.map_or(cap, |v| v.min(cap)));
    }
    // Control cannot spoof rate tables or escalate into paid repairs, image generation, or vision review.
    settings.budget.rates = ceiling.budget.rates.clone();
    settings.image_rates = ceiling.image_rates.clone();
    if !ceiling.allow_paid_repairs {
        settings.allow_paid_repairs = false;
    }
    if ceiling.output_mode == OutputMode::NativeOnly {
        settings.output_mode = OutputMode::NativeOnly;
    }
    if !ceiling.verify_generated_images {
        settings.verify_generated_images = false;
    }
    if ceiling.execution_pacing == ExecutionPacing::ReviewEveryBatch {
        settings.execution_pacing = ExecutionPacing::ReviewEveryBatch;
    }
    if ceiling.execution_mode == ExecutionMode::LocalOnly {
        settings.execution_mode = ExecutionMode::LocalOnly;
        if matches!(settings.text_model.provider, ProviderKind::Gemini | ProviderKind::OpenAi) {
            settings.text_model = ceiling.text_model.clone();
        }
        if matches!(settings.image_model.provider, ProviderKind::Gemini | ProviderKind::OpenAi) {
            settings.image_model = ceiling.image_model.clone();
        }
    }
}

pub(super) fn clamped_settings(ceiling: &AiSettings, raw: Value) -> Result<AiSettings, String> {
    let mut settings: AiSettings = serde_json::from_value(raw).map_err(|e| e.to_string())?;
    clamp_control_settings(ceiling, &mut settings);
    Ok(settings)
}

pub(super) fn ensure_ceiling_initialized(app: &mut DesignApp) {
    if !app.ui.ai.initialized && app.services.ai.is_some() {
        let _ = dispatch(app, AiRequest::Poll);
    }
}

fn req_str_field(p: &Value, name: &str) -> Result<String, String> {
    p.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string: {name}"))
}

fn req_value_field(p: &Value, name: &str) -> Result<Value, String> {
    p.get(name).cloned().ok_or_else(|| format!("missing {name}"))
}

fn decode_skill_or_batch_request(app: &mut DesignApp, method: &str, p: &Value) -> Result<Option<AiRequest>, String> {
    let req = match method {
        "import_skill" => AiRequest::ImportSkill { path: req_str_field(p, "path")? },
        "inspect_skill" => AiRequest::InspectSkill { id: req_str_field(p, "id")?, revision: req_str_field(p, "revision")? },
        "prepare_skills" => AiRequest::PrepareSkills { prompt: req_str_field(p, "prompt")? },
        "save_skill_draft" => AiRequest::SaveSkillDraft {
            id: req_str_field(p, "id")?,
            skill_md: req_str_field(p, "skill_md")?,
            manifest_json: req_str_field(p, "manifest_json")?,
        },
        "add_design_correction" => AiRequest::AddDesignCorrection {
            id: req_str_field(p, "id")?,
            revision: req_str_field(p, "revision")?,
            correction: serde_json::from_value(req_value_field(p, "correction")?).map_err(|e| e.to_string())?,
        },
        "generate_skill" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkill {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: req_str_field(p, "prompt")?,
                reference_paths: serde_json::from_value(req_value_field(p, "reference_paths")?).map_err(|e| e.to_string())?,
                settings: clamped_settings(&app.ui.ai.settings, req_value_field(p, "settings")?)?,
            }
        }
        "generate_skill_from_document" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkillFromDocument {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: req_str_field(p, "prompt")?,
                settings: clamped_settings(&app.ui.ai.settings, req_value_field(p, "settings")?)?,
            }
        }
        "run_skill_sample" => AiRequest::RunSkillSample { id: req_str_field(p, "id")?, revision: req_str_field(p, "revision")? },
        "run_design_template" => AiRequest::RunDesignTemplate {
            id: req_str_field(p, "id")?,
            revision: req_str_field(p, "revision")?,
            template: req_str_field(p, "template")?,
        },
        "refresh_batch_proofs" => AiRequest::RefreshBatchProofs { job_id: req_str_field(p, "job_id")? },
        "pause_batch" => AiRequest::PauseBatch { job_id: req_str_field(p, "job_id")? },
        "resume_batch" => {
            ensure_ceiling_initialized(app);
            if app.ui.ai.settings.execution_pacing == ExecutionPacing::ReviewEveryBatch {
                return Err("Approval requires the human review button in the desktop panel.".into());
            }
            AiRequest::ResumeBatch { job_id: req_str_field(p, "job_id")? }
        }
        "dismiss_image_regeneration" => AiRequest::DismissImageRegeneration { job_id: req_str_field(p, "job_id")? },
        "set_skill_enabled" => {
            AiRequest::SetSkillEnabled { id: req_str_field(p, "id")?, enabled: p.get("enabled").and_then(Value::as_bool).ok_or("missing enabled")? }
        }
        "rollback_skill" => AiRequest::RollbackSkill { id: req_str_field(p, "id")?, revision: req_str_field(p, "revision")? },
        "export_skill" => {
            AiRequest::ExportSkill { id: req_str_field(p, "id")?, revision: req_str_field(p, "revision")?, path: req_str_field(p, "path")? }
        }
        "discover_local_endpoints" => AiRequest::DiscoverLocalEndpoints,
        "continue_remaining_locally" => AiRequest::ContinueRemainingLocally {
            job_id: p.get("job_id").and_then(Value::as_str).map(str::to_owned),
            local_model: p.get("local_model").filter(|v| !v.is_null()).cloned().map(serde_json::from_value).transpose().map_err(|e| e.to_string())?,
        },
        "create_knowledge_pack" => {
            let paths_val = p.get("paths").or_else(|| p.get("source_paths")).cloned().ok_or("missing paths")?;
            AiRequest::CreateKnowledgePack {
                name: req_str_field(p, "name")?,
                description: p.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
                paths: serde_json::from_value(paths_val).map_err(|e| e.to_string())?,
            }
        }
        "delete_knowledge_pack" => AiRequest::DeleteKnowledgePack { id: req_str_field(p, "id")? },
        _ => return Ok(None),
    };
    Ok(Some(req))
}

fn decode_export_or_print_request(method: &str, p: &Value) -> Result<AiRequest, String> {
    match method {
        "approve_skill" | "approve_batch" | "confirm_image_regeneration" => {
            Err("Approval requires the human review button in the desktop panel.".into())
        }
        "save_api_key" | "remove_api_key" | "refresh_credentials" => {
            Err("Credential actions require the masked human controls in Providers and keys.".into())
        }
        "create_science_sample" => Ok(AiRequest::CreateScienceSample { directory: req_str_field(p, "directory")? }),
        "export_document" => {
            Ok(AiRequest::ExportDocument { path: req_str_field(p, "path")?, pdf: p.get("pdf").and_then(Value::as_bool).ok_or("missing pdf")? })
        }
        "export_idml" => Ok(AiRequest::ExportIdml { path: req_str_field(p, "path")? }),
        "export_indd" => Ok(AiRequest::ExportIndd { path: req_str_field(p, "path")? }),
        "prepare_print" => Ok(AiRequest::PreparePrint {
            printer: p.get("printer").and_then(Value::as_str).map(str::to_owned),
            grayscale: p.get("grayscale").and_then(Value::as_bool).unwrap_or(false),
            duplex: req_str_field(p, "duplex")?,
        }),
        "save_prepared_print" => Ok(AiRequest::SavePreparedPrint { token: req_str_field(p, "token")?, path: req_str_field(p, "path")? }),
        "print_document" | "submit_prepared_print" | "open_print_proof" | "start_local_ollama" | "stop_local_ollama" => {
            Err("Printing and local service launch require the explicit human controls in the desktop panel.".into())
        }
        _ => Err("Unknown AI request method; human-only actions require desktop controls.".into()),
    }
}

pub fn request_command(app: &mut DesignApp, p: &Value) -> Result<Value, String> {
    let method = req_str_field(p, "method")?;
    let request = match method.as_str() {
        "poll" => AiRequest::Poll,
        "cancel" => AiRequest::Cancel,
        "refresh_resources" => AiRequest::RefreshResources,
        "start" => {
            ensure_ceiling_initialized(app);
            let mut req: TaskRequest = serde_json::from_value(req_value_field(p, "request")?).map_err(|e| e.to_string())?;
            clamp_control_settings(&app.ui.ai.settings, &mut req.settings);
            AiRequest::Start(req)
        }
        "preview_production" => AiRequest::PreviewProduction(serde_json::from_value(req_value_field(p, "request")?).map_err(|e| e.to_string())?),
        "update_settings" => {
            ensure_ceiling_initialized(app);
            AiRequest::UpdateSettings(clamped_settings(&app.ui.ai.settings, req_value_field(p, "settings")?)?)
        }
        "list_models" => AiRequest::ListModels { selection: serde_json::from_value(req_value_field(p, "selection")?).map_err(|e| e.to_string())? },
        other => match decode_skill_or_batch_request(app, other, p)? {
            Some(req) => req,
            None => decode_export_or_print_request(other, p)?,
        },
    };
    dispatch(app, request)
}

pub fn poll(app: &mut DesignApp, ctx: &egui::Context) {
    if app.ui.ai.preview_update.poll_background(ctx)
        && let Some(notice) = app.ui.ai.preview_update.take_status_notice(app.ui.language == "ar")
    {
        app.status(notice);
    }
    if !app.ui.ai.snapshot.busy {
        return;
    }
    let due = app.ui.ai.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_millis(150));
    if due {
        app.ui.ai.last_poll = Some(Instant::now());
        let _ = dispatch(app, AiRequest::Poll);
    }
    ctx.request_repaint_after(Duration::from_millis(150));
}

pub(super) fn text<'a>(arabic: bool, en: &'a str, ar: &'a str) -> &'a str {
    if arabic { ar } else { en }
}
pub(super) fn label(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) {
    rtl::label(ui, text(arabic, en, ar));
}
pub(super) fn button(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) -> bool {
    ui.button(rtl::widget(ui, text(arabic, en, ar))).clicked()
}
pub(super) fn bidi_layouter(multiline: bool) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> std::sync::Arc<egui::Galley> {
    move |ui, buf, wrap_width| {
        let font_id = egui::TextStyle::Body.resolve(ui.style());
        let color = ui.visuals().text_color();
        rtl::editable_layout(ui, buf.as_str(), wrap_width, font_id, color, multiline)
    }
}
pub(super) fn named_rtl_text_edit(ui: &mut egui::Ui, edit: egui::TextEdit<'_>, name: impl Into<String>) -> egui::Response {
    rtl::show_text_edit(ui, edit).response.response.accessible_name(name)
}
pub(super) fn field(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str, value: &mut String) {
    let caption = text(arabic, en, ar);
    label(ui, arabic, en, ar);
    let field_rtl = if value.trim().is_empty() { arabic } else { rtl::has_rtl(value) };
    let mut layouter = bidi_layouter(false);
    named_rtl_text_edit(
        ui,
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .align(if field_rtl { egui::Align2::RIGHT_CENTER } else { egui::Align2::LEFT_CENTER })
            .layouter(&mut layouter),
        format!("{caption} input"),
    );
}
pub(super) fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::LocalOpenAi => "Local OpenAI",
        ProviderKind::ComfyUi => "ComfyUI",
    }
}
pub(super) fn provider(ui: &mut egui::Ui, id: &str, selected: &mut ProviderKind, images: bool) {
    crate::widgets::name_combo(
        craft_ui::ComboBox::from_id_salt(id).selected_text(provider_name(*selected)).show_ui(ui, |ui| {
            let choices: &[ProviderKind] = if images {
                &[ProviderKind::OpenAi, ProviderKind::Gemini, ProviderKind::ComfyUi]
            } else {
                &[ProviderKind::OpenAi, ProviderKind::Gemini, ProviderKind::Ollama, ProviderKind::LocalOpenAi]
            };
            for &p in choices {
                ui.selectable_value(selected, p, provider_name(p));
            }
        }),
        format!("{id} selector"),
    );
}
pub(super) fn model(
    ui: &mut egui::Ui,
    a: bool,
    spec: (&str, bool),
    m: &mut ModelSelection,
    pending_models: &mut Option<ModelSelection>,
    discovered: &BTreeMap<(ProviderKind, Option<String>), Vec<String>>,
    requests: &mut Vec<AiRequest>,
) {
    let (id, images) = spec;
    ui.horizontal_wrapped(|ui| {
        provider(ui, id, &mut m.provider, images);
        label(ui, a, "Model", "النموذج");
        let w = (ui.available_width() - 96.0).clamp(120.0, 240.0);
        ui.add(egui::TextEdit::singleline(&mut m.model).desired_width(w)).accessible_name(format!("{id} model input"));
        if button(ui, a, "Discover", "اكتشاف") {
            *pending_models = Some(m.clone());
            requests.push(AiRequest::ListModels { selection: m.clone() });
        }
    });
    if let Some(models) = discovered.get(&(m.provider, m.endpoint.clone())) {
        let filtered: Vec<&String> =
            models.iter().filter(|name| if images { !is_non_image_model(m.provider, name) } else { !is_non_text_model(name) }).collect();
        if !filtered.is_empty() {
            let picker_label = if images {
                text(a, "Available image models", "نماذج الصور المتاحة")
            } else {
                text(a, "Available models", "النماذج المتاحة")
            };
            crate::widgets::name_combo(
                craft_ui::ComboBox::from_id_salt((id, "discovered_models")).selected_text(rtl::widget(ui, picker_label)).show_ui(ui, |ui| {
                    for name in filtered {
                        ui.selectable_value(&mut m.model, name.clone(), name);
                    }
                }),
                format!("{id} available models"),
            );
        }
    }
    let mut endpoint = m.endpoint.clone().unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        label(ui, a, "Endpoint (optional)", "عنوان الخدمة (اختياري)");
        let ew = (ui.available_width() - 12.0).clamp(140.0, 350.0);
        if ui.add(egui::TextEdit::singleline(&mut endpoint).desired_width(ew)).accessible_name(format!("{id} endpoint input")).changed() {
            m.endpoint = (!endpoint.trim().is_empty()).then_some(endpoint);
        }
    });
}

fn truncate_chars_clean(raw: &str, max_chars: usize) -> String {
    let normalized: String = raw.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

pub(crate) fn capture_selection_context(app: &DesignApp) -> Option<SelectionContext> {
    let st = app.session.active()?;
    let active_page = crate::canvas::current_page(app).or_else(|| {
        st.selection
            .items
            .first()
            .and_then(|id| {
                st.doc.spreads.iter().enumerate().find_map(|(si, sp)| sp.items.iter().any(|it| it.id == *id).then(|| st.doc.first_page_of_spread(si)))
            })
            .or(Some(0))
    });
    let mut selected_item_ids = Vec::new();
    let mut selected_labels = Vec::new();
    let mut selected_kinds = Vec::new();
    let mut first_story_id = st.selection.text.map(|ts| ts.story.0);
    for id in &st.selection.items {
        selected_item_ids.push(id.0);
        if let Some(it) = st.doc.item(*id) {
            selected_labels.push(crate::panels::layers::item_label(&st.doc, it));
            selected_kinds.push(it.default_label().to_string());
            if first_story_id.is_none()
                && let designcraft_doc::Content::Text(tf) = &it.content
            {
                first_story_id = Some(tf.story.0);
            }
        }
    }
    let selected_text_snippet = crate::canvas::selected_text(app)
        .or_else(|| {
            if let Some(ts) = st.selection.text {
                st.doc.text_story(ts.story, ts.cell).map(|s| s.text.clone())
            } else if let Some(sid) = first_story_id {
                st.doc.story(designcraft_doc::StoryId(sid)).map(|s| s.text.clone())
            } else {
                None
            }
        })
        .map(|s| truncate_chars_clean(&s, 240))
        .filter(|s| !s.is_empty());

    if selected_item_ids.is_empty() && st.selection.text.is_none() {
        return None;
    }
    Some(SelectionContext {
        active_page,
        selected_item_ids,
        selected_labels,
        selected_kinds,
        selected_story_id: first_story_id,
        selected_text_snippet,
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn accept_drop_if_over_studio(app: &mut DesignApp, path: &str, pointer: Option<egui::Pos2>) -> bool {
    if !app.ui.ai.open || app.ui.ai.focus_mode {
        return false;
    }
    let Some(pos) = pointer else {
        return false;
    };
    if !app.ui.ai.last_panel_rect.is_some_and(|r| r.contains(pos)) {
        return false;
    }
    if app.ui.ai.attach_file_path(path, None) {
        app.ui.ai.tab = 0;
        return true;
    }
    false
}

struct StudioFrameOutcome {
    requests: Vec<AiRequest>,
    check_preview_update: bool,
    toggle_language: bool,
    activate_document: Option<usize>,
    original_document: Option<usize>,
}

fn prepare_studio_state(app: &mut DesignApp) -> (AiUiState, bool, bool, Vec<OpenDocument>, Option<usize>, bool) {
    let document = app.session.active().map(|document| (document.uid, document.revision));
    let needs_validation = !app.ui.ai.snapshot.busy
        && app.ui.ai.snapshot.batch.as_ref().is_some_and(|batch| {
            batch.status == "awaiting_human_proof"
                && document.is_some_and(|(uid, revision)| Some(uid) == batch.document_uid && Some(revision) != batch.document_revision)
        });
    if needs_validation && app.ui.ai.proof_validation_requested != document && app.services.ai.is_some() {
        app.ui.ai.proof_validation_requested = document;
        app.ui.ai.sync_proof_document(document);
        if let Err(error) = dispatch(app, AiRequest::Poll) {
            app.ui.ai.error = error;
        }
    }
    let live_selection = capture_selection_context(app);
    let mut state = std::mem::take(&mut app.ui.ai);
    state.live_selection = live_selection;
    state.batch_approval_gesture = None;
    state.review_input_is_human = human_review_input(app);
    state.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
    state.proof_facing_pages =
        app.session.active().filter(|document| state.proof_document_matches && document.doc.settings.facing_pages).map(|_| true);
    state.proof_binding = app
        .session
        .active()
        .filter(|document| state.proof_document_matches && document.doc.settings.facing_pages)
        .map(|document| document.doc.settings.right_to_left_binding);
    let a = app.ui.language == "ar";
    let available = app.services.ai.is_some() && state.snapshot.available;
    let documents: Vec<_> =
        app.session.documents().iter().enumerate().map(|(index, document)| OpenDocument::from_document(index, document)).collect();
    let activate_document = app.session.active_index();
    let has_active_doc = app.session.active().is_some();
    (state, a, available, documents, activate_document, has_active_doc)
}

fn consume_raw_studio_drops(ctx: &egui::Context, state: &mut AiUiState) {
    if state.focus_mode {
        return;
    }
    let (dropped, pointer) = ctx.input(|i| (i.raw.dropped_files.clone(), i.pointer.hover_pos().or_else(|| i.pointer.interact_pos())));
    if dropped.is_empty() {
        return;
    }
    let over_panel = match (pointer, state.last_panel_rect) {
        (Some(pos), Some(rect)) => rect.contains(pos),
        (None, _) => true,
        _ => false,
    };
    if !over_panel {
        return;
    }
    for file in dropped {
        let raw = file.path().to_string_lossy();
        if !raw.trim().is_empty() {
            let _ = state.attach_file_path(&raw, None);
        }
    }
}

fn render_focus_rail(ui: &mut egui::Ui, state: &mut AiUiState, a: bool, vertical_rail: bool, requests: &mut Vec<AiRequest>) {
    let t = crate::theme::Tokens::get(ui.ctx());
    let progress_badge = if let Some(batch) = &state.snapshot.batch
        && batch.total_pages > 0
    {
        format!("{}/{}", batch.completed_pages, batch.total_pages)
    } else if state.snapshot.busy {
        format!("{:.0}%", (state.snapshot.progress.clamp(0.0, 1.0) * 100.0))
    } else {
        text(a, "Idle", "جاهز").to_owned()
    };

    if vertical_rail {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            if ui.button(rtl::widget(ui, text(a, "Expand", "توسيع"))).on_hover_text(text(a, "Expand AI Studio", "توسيع استوديو الذكاء")).clicked()
            {
                state.focus_mode = false;
            }
            ui.add_space(6.0);
            egui::Frame::NONE
                .fill(t.panel_darker)
                .stroke(egui::Stroke::new(1.0, if state.task_active() { t.accent } else { t.border }))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(4, 6))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(&progress_badge).strong().color(if state.task_active() { t.accent } else { t.text_dim }));
                });
            if state.task_active() {
                ui.add_space(6.0);
                let phase = AiUiState::live_phase(&state.snapshot.stage, a);
                ui.add(egui::Label::new(egui::RichText::new(phase).size(10.5).color(t.text_dim)).wrap());
                ui.add_space(4.0);
                if let Some(batch) = &state.snapshot.batch
                    && !batch.job_id.is_empty()
                    && batch.completed_pages < batch.total_pages
                {
                    if batch.paused {
                        if ui.button("▶").on_hover_text(text(a, "Resume", "استئناف")).clicked() {
                            requests.push(AiRequest::ResumeBatch { job_id: batch.job_id.clone() });
                        }
                    } else if state.snapshot.busy
                        && state.settings.execution_pacing != ExecutionPacing::ReviewEveryBatch
                        && ui.button("⏸").on_hover_text(text(a, "Pause after current batch", "إيقاف مؤقت")).clicked()
                    {
                        requests.push(AiRequest::PauseBatch { job_id: batch.job_id.clone() });
                    }
                }
                if ui.button("⏹").on_hover_text(text(a, "Cancel task", "إلغاء المهمة")).clicked() {
                    requests.push(AiRequest::Cancel);
                }
            }
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            egui::Frame::NONE
                .fill(t.panel_darker)
                .stroke(egui::Stroke::new(1.0, if state.task_active() { t.accent } else { t.border }))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(6, 2))
                .show(ui, |ui| {
                    rtl::label(ui, egui::RichText::new(&progress_badge).strong().color(if state.task_active() { t.accent } else { t.text_dim }));
                });
            if state.task_active() {
                rtl::label(ui, egui::RichText::new(AiUiState::live_phase(&state.snapshot.stage, a)).color(t.text_dim));
                if let Some(batch) = &state.snapshot.batch
                    && !batch.job_id.is_empty()
                    && batch.completed_pages < batch.total_pages
                    && state.snapshot.busy
                    && !batch.paused
                    && state.settings.execution_pacing != ExecutionPacing::ReviewEveryBatch
                    && button(ui, a, "Pause", "إيقاف مؤقت")
                {
                    requests.push(AiRequest::PauseBatch { job_id: batch.job_id.clone() });
                }
                if button(ui, a, "Cancel", "إلغاء") {
                    requests.push(AiRequest::Cancel);
                }
            }
            if button(ui, a, "Dock", "إرساء جانبي") {
                state.docked = true;
            }
            if button(ui, a, "Expand", "توسيع") {
                state.focus_mode = false;
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn render_studio_body(
    ui: &mut egui::Ui,
    state: &mut AiUiState,
    a: bool,
    available: bool,
    has_active_doc: bool,
    is_docked_surface: bool,
    documents: &[OpenDocument],
    activate_document: &mut Option<usize>,
    open: &mut bool,
    check_preview_update: &mut bool,
    toggle_language: &mut bool,
    requests: &mut Vec<AiRequest>,
) {
    let t = crate::theme::Tokens::get(ui.ctx());
    *check_preview_update = state.preview_update.show(ui, a);
    egui::Frame::NONE.fill(t.tab_strip).inner_margin(egui::Margin::symmetric(6, 4)).corner_radius(t.radius).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            if is_docked_surface {
                rtl::label(ui, egui::RichText::new(text(a, "AI Studio", "استوديو الذكاء")).strong().color(t.text_strong));
            }
            if is_docked_surface {
                if button(ui, a, "Float", "نافذة عائمة") {
                    state.docked = false;
                }
            } else if button(ui, a, "Dock", "إرساء جانبي") {
                state.docked = true;
            }
            if button(ui, a, "Focus", "وضع التركيز") {
                state.focus_mode = true;
            }
            if button(ui, a, "Refresh", "تحديث") {
                requests.push(AiRequest::Poll);
            }
            if ui.button(rtl::widget(ui, if a { "EN" } else { "العربية" })).clicked() {
                *toggle_language = true;
            }
            if is_docked_surface
                && ui
                    .button("×")
                    .on_hover_text(text(a, "Close AI Studio", "إغلاق استوديو الذكاء"))
                    .accessible_name(text(a, "Close AI Studio", "إغلاق استوديو الذكاء"))
                    .clicked()
            {
                *open = false;
            }
        });
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            for (i, en, ar) in [
                (0, "Production", "الإنتاج"),
                (1, "Skills", "المهارات"),
                (2, "Usage", "الاستخدام"),
                (3, "Providers and keys", "المزودون والمفاتيح"),
                (4, "Local AI", "الذكاء المحلي"),
            ] {
                if ui.selectable_label(state.tab == i, rtl::widget(ui, text(a, en, ar))).clicked() {
                    state.tab = i;
                }
            }
        });
    });
    ui.add_space(3.0);
    if !available {
        label(
            ui,
            a,
            "Native AI host is unavailable. Open this panel in the desktop application.",
            "خدمة الذكاء الاصطناعي غير متاحة. افتح هذه اللوحة في تطبيق سطح المكتب.",
        );
    }
    if state.task_active() {
        ui.add(egui::ProgressBar::new(state.snapshot.progress.clamp(0.0, 1.0)).text(&state.snapshot.stage));
        let usage = &state.snapshot.current_usage;
        rtl::label(
            ui,
            format!(
                "{}: {} / {} · {} {} · {} {}{}{}",
                text(a, "Live requests", "الطلبات المباشرة"),
                usage.requests,
                state.settings.budget.max_requests,
                text(a, "Input", "الإدخال"),
                usage.input_tokens,
                text(a, "Output", "الإخراج"),
                usage.output_tokens,
                usage.estimated_cost_usd.map_or_else(String::new, |cost| {
                    if a { format!(" · التقدير {}", rtl::ltr(format!("${cost:.6}"))) } else { format!(" · Estimated ${cost:.6}") }
                }),
                if usage.has_unreported_usage { text(a, " · Some usage unreported", " · بعض الاستخدام غير مبلغ") } else { "" }
            ),
        );
        ui.horizontal_wrapped(|ui| {
            if button(ui, a, "Cancel task — keep produced pages", "إلغاء المهمة مع الاحتفاظ بالصفحات المنتجة") {
                requests.push(AiRequest::Cancel);
            }
            if let Some(batch) = &state.snapshot.batch
                && !batch.job_id.is_empty()
                && batch.completed_pages < batch.total_pages
            {
                if batch.paused || batch.status == "awaiting_human_proof" {
                    if button(ui, a, "Resume continuous generation", "استئناف التوليد المستمر") {
                        requests.push(AiRequest::ResumeBatch { job_id: batch.job_id.clone() });
                    }
                } else if state.snapshot.busy
                    && state.settings.execution_pacing != ExecutionPacing::ReviewEveryBatch
                    && button(ui, a, "Pause after current batch", "إيقاف مؤقت بعد هذه الدفعة")
                {
                    requests.push(AiRequest::PauseBatch { job_id: batch.job_id.clone() });
                }
            }
            rtl::label(ui, state.live_status_line(a));
        });
        if let Some(batch) = &state.snapshot.batch
            && batch.total_pages > 0
            && has_active_doc
        {
            let shown_pages = batch.completed_pages.max(1).min(batch.total_pages).min(24);
            egui::ScrollArea::horizontal().id_salt("ai_live_page_stepper").max_height(24.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    rtl::label(ui, egui::RichText::new(text(a, "Pages:", "الصفحات:")).color(t.text_dim));
                    for p in 1..=shown_pages {
                        let done = p <= batch.completed_pages;
                        let pill = if done { format!("{p} ✓") } else { format!("{p}…") };
                        if ui.selectable_label(false, pill).on_hover_text(text(a, "Jump canvas to page", "الانتقال إلى الصفحة")).clicked()
                        {
                            state.navigate_to_page = usize::try_from(p.saturating_sub(1)).ok();
                        }
                    }
                });
            });
        }
    }
    let status_message = if state.error.is_empty() { &state.snapshot.message } else { &state.error };
    if !(status_message.is_empty() || state.tab == 0 && is_skill_scoped_error(status_message)) {
        let summary: String = status_message.lines().next().unwrap_or("").chars().take(140).collect();
        rtl::label(ui, summary);
    }
    let reviewing = state.tab == 0
        && ((state.proof_display.is_some() && state.show_saved_proof)
            || state.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof"));
    if reviewing {
        production(ui, state, a, available, documents, activate_document, requests);
    } else {
        let scroll_max = if is_docked_surface { (ui.available_height() - 28.0).max(240.0) } else { 650.0 };
        egui::ScrollArea::vertical().id_salt("ai_panel_body").auto_shrink([false, !is_docked_surface]).max_height(scroll_max).show(ui, |ui| {
            match state.tab {
                1 => skills(ui, state, a, available, has_active_doc, requests),
                2 => usage(ui, state, a),
                3 => providers(ui, state, a, available, requests),
                4 => local_ai(ui, state, a, available, cfg!(not(target_arch = "wasm32")), requests),
                _ => production(ui, state, a, available, documents, activate_document, requests),
            }
        });
    }
    if !state.error.is_empty()
        || !state.snapshot.message.is_empty()
        || !state.snapshot.current_issues.is_empty()
        || state.snapshot.current_usage.requests > 0
    {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Details", "التفاصيل"))).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("ai_status_details").max_height(140.0).show(ui, |ui| {
                rtl::label(ui, &state.snapshot.message);
                rtl::label(ui, &state.error);
                let usage = &state.snapshot.current_usage;
                rtl::label(
                    ui,
                    format!(
                        "{} {} · {} {} · {} {} · {} {} · {} {} · {} {}",
                        text(a, "Requests", "الطلبات"),
                        usage.requests,
                        text(a, "Input", "الإدخال"),
                        usage.input_tokens,
                        text(a, "Output", "الإخراج"),
                        usage.output_tokens,
                        text(a, "Cached", "المخزن"),
                        usage.cached_input_tokens,
                        text(a, "Reasoning", "التفكير"),
                        usage.reasoning_tokens,
                        text(a, "Estimated tokens", "الرموز المقدرة"),
                        usage.estimated_tokens
                    ),
                );
                if usage.has_unreported_usage {
                    label(ui, a, "Some provider usage is unreported.", "بعض بيانات الاستخدام لم يبلغها المزود.");
                }
                for issue in &state.snapshot.current_issues {
                    if issue.get("kind").and_then(Value::as_str) == Some("advisory_fail") {
                        let label_str = issue.get("label").and_then(Value::as_str).unwrap_or("");
                        let reasons_str = issue
                            .get("reasons")
                            .and_then(Value::as_array)
                            .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("؛ "))
                            .unwrap_or_default();
                        rtl::label(
                            ui,
                            if a {
                                format!("ملاحظة فحص بصري استشاري ({label_str} — استمر الإنتاج للمراجعة البشرية): {reasons_str}")
                            } else {
                                format!("Advisory visual inspection note ({label_str} — kept for human proof review): {reasons_str}")
                            },
                        );
                    } else {
                        rtl::label(ui, serde_json::to_string_pretty(issue).unwrap_or_else(|_| issue.to_string()));
                    }
                }
            });
        });
    }
}

fn finish_studio_frame(app: &mut DesignApp, ctx: &egui::Context, mut state: AiUiState, a: bool, mut outcome: StudioFrameOutcome) {
    if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        state.review_pointer_press = None;
    }
    consume_raw_studio_drops(ctx, &mut state);
    app.ui.ai = state;
    if outcome.toggle_language
        && let Err(error) = app.run("app.language", serde_json::json!({"lang": if a { "" } else { "ar" }}))
    {
        app.ui.ai.error = error;
    }
    if outcome.check_preview_update
        && let Err(error) = app.run("app.checkPreviewUpdate", serde_json::json!({}))
    {
        app.ui.ai.error = error;
    }
    if let Some(page_zero_based) = app.ui.ai.navigate_to_page.take() {
        crate::canvas::go_to_page(app, page_zero_based);
        let _ = app.run("view.fitPage", serde_json::json!({}));
    }
    if let Some(cmd) = app.ui.ai.quick_export_cmd.take()
        && let Err(error) = app.run(cmd, serde_json::json!({}))
    {
        app.ui.ai.error = error;
    }
    if outcome.activate_document != outcome.original_document {
        if let Some(index) = outcome.activate_document
            && let Err(error) = app.run("file.activate", serde_json::json!({"index":index}))
        {
            app.ui.ai.error = error;
        }
        // A document choice is never implicit authority to start a task in that same frame.
        outcome.requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_ollama)
        && let Err(error) = app.ask_open("ollama_executable", crate::app::file_dialogs::Bound::App, |app, path| {
            app.ui.ai.ollama_executable = path;
            Ok(Value::Null)
        })
    {
        app.ui.ai.error = error;
    }
    if std::mem::take(&mut app.ui.ai.pick_attachment_file) {
        let role = app.ui.ai.attachment_role_input;
        if let Err(error) = app.ask_open("ai_attachment", crate::app::file_dialogs::Bound::App, move |app, picked| {
            app.ui.ai.attach_file_path(&picked, Some(role));
            Ok(Value::Null)
        }) {
            app.ui.ai.error = error;
        }
        outcome.requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_script_file) {
        let pick_res = if app.services.open_async.is_some() {
            app.pick_and_open("ai_script")
        } else {
            app.ask_open("ai_script", crate::app::file_dialogs::Bound::App, |app, picked| {
                load_script_path_into_prompt(app, &picked);
                Ok(Value::Null)
            })
        };
        if let Err(error) = pick_res {
            app.ui.ai.error = error;
        }
        outcome.requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_target_document) {
        let pick_res = if app.services.open_async.is_some() {
            app.pick_and_open("ai_target_document")
        } else {
            app.ask_open("ai_target_document", crate::app::file_dialogs::Bound::App, |app, path| {
                app.ui.ai.open_document_path = path.clone();
                match open_target_document_path(app, &path) {
                    Ok(_) => {
                        app.ui.ai.new_document = false;
                        app.ui.ai.continue_book = false;
                        app.ui.ai.restored_target_unconfirmed = false;
                        app.ui.ai.error.clear();
                    }
                    Err(error) => app.ui.ai.error = error,
                }
                Ok(Value::Null)
            })
        };
        if let Err(error) = pick_res {
            app.ui.ai.error = error;
        }
        outcome.requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.open_target_document_requested) {
        let path = app.ui.ai.open_document_path.trim().to_string();
        if path.is_empty() {
            app.ui.ai.error = text(a, "Enter a document path to open.", "أدخل مسار مستند لفتحه.").into();
        } else {
            match open_target_document_path(app, &path) {
                Ok(_) => {
                    app.ui.ai.new_document = false;
                    app.ui.ai.continue_book = false;
                    app.ui.ai.restored_target_unconfirmed = false;
                    app.ui.ai.error.clear();
                    ctx.request_repaint();
                }
                Err(error) => app.ui.ai.error = error,
            }
        }
        outcome.requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if !outcome.requests.is_empty() {
        ctx.request_repaint();
    }
    for request in outcome.requests {
        if let Err(e) = dispatch(app, request) {
            app.ui.ai.error = e;
        }
    }
    app.ui.ai.batch_approval_gesture = None;
    load_preview(app, ctx);
    load_proof_preview(app, ctx);
    load_companion_proof(app, ctx);
}

pub fn sync_dock_harmony(app: &mut DesignApp) {
    let docked_active = app.ui.ai.open && app.ui.ai.docked;
    if docked_active {
        if !app.ui.ai.synced_dock_harmony {
            app.ui.ai.synced_dock_harmony = true;
            if app.ui.dock_expanded && crate::panel_docking::set_iconic(app, true).is_ok() {
                app.ui.ai.auto_collapsed_dock = true;
            }
        }
    } else {
        app.ui.ai.synced_dock_harmony = false;
        if std::mem::take(&mut app.ui.ai.auto_collapsed_dock) && !app.ui.dock_expanded {
            let _ = crate::panel_docking::set_iconic(app, false);
        }
    }
}

pub fn show_docked(app: &mut DesignApp, ui: &mut egui::Ui) {
    if !app.ui.ai.open || !app.ui.ai.docked {
        return;
    }
    let ctx = ui.ctx().clone();
    app.ui.ai.drawn_frame = Some(ctx.cumulative_frame_nr());
    let is_rtl_ui = crate::i18n::is_rtl(&app.ui.language);
    let t = crate::theme::Tokens::get(&ctx);
    let avail_w = ui.available_width();
    let (mut state, a, available, documents, mut activate_document, has_active_doc) = prepare_studio_state(app);
    let original_document = activate_document;
    let mut open = state.open;
    let mut requests = Vec::new();
    let mut check_preview_update = false;
    let mut toggle_language = false;

    let frame = egui::Frame::NONE.fill(t.panel).stroke(egui::Stroke::new(1.0, t.border)).inner_margin(egui::Margin::symmetric(8, 6));

    let panel_rect = if state.focus_mode {
        let rail_frame = egui::Frame::NONE.fill(t.panel).stroke(egui::Stroke::new(1.0, t.border)).inner_margin(egui::Margin::symmetric(4, 4));
        let panel = if is_rtl_ui { egui::Panel::left("ai_studio_dock_rail") } else { egui::Panel::right("ai_studio_dock_rail") };
        panel
            .exact_size(56.0)
            .resizable(false)
            .frame(rail_frame)
            .show(ui, |ui| {
                render_focus_rail(ui, &mut state, a, true, &mut requests);
            })
            .response
            .rect
    } else {
        let dock_allowance = if avail_w < 1080.0 { 220.0 } else { 280.0 };
        let max_w = (avail_w - dock_allowance - 380.0).clamp(300.0, 560.0);
        let min_w = 300.0_f32.min(max_w);
        let default_w = (avail_w * 0.34).clamp(340.0, 460.0).clamp(min_w, max_w);
        let panel = if is_rtl_ui { egui::Panel::left("ai_studio_dock") } else { egui::Panel::right("ai_studio_dock") };
        panel
            .default_size(default_w)
            .size_range(min_w..=max_w)
            .resizable(true)
            .frame(frame)
            .show(ui, |ui| {
                render_studio_body(
                    ui,
                    &mut state,
                    a,
                    available,
                    has_active_doc,
                    true,
                    &documents,
                    &mut activate_document,
                    &mut open,
                    &mut check_preview_update,
                    &mut toggle_language,
                    &mut requests,
                );
            })
            .response
            .rect
    };

    state.open = open;
    if !open {
        state.focus_mode = false;
        state.last_panel_rect = None;
    } else {
        state.last_panel_rect = Some(panel_rect);
    }
    finish_studio_frame(
        app,
        &ctx,
        state,
        a,
        StudioFrameOutcome { requests, check_preview_update, toggle_language, activate_document, original_document },
    );
}

pub fn show(app: &mut DesignApp, ctx: &egui::Context) {
    if !app.ui.ai.open {
        app.ui.ai.last_panel_rect = None;
        return;
    }
    if app.ui.ai.drawn_frame == Some(ctx.cumulative_frame_nr()) {
        return;
    }
    let (mut state, a, available, documents, mut activate_document, has_active_doc) = prepare_studio_state(app);
    let original_document = activate_document;
    let mut open = state.open;
    let mut requests = Vec::new();
    let mut check_preview_update = false;
    let mut toggle_language = false;

    let mut window_open = open;
    let mut body_open = true;
    let win_resp = if state.focus_mode {
        egui::Window::new(rtl::widget_ctx(ctx, text(a, "AI Focus", "تركيز الذكاء")))
            .id(egui::Id::unique("ai_production_focus"))
            .open(&mut window_open)
            .default_size(egui::vec2(340.0, 64.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                render_focus_rail(ui, &mut state, a, false, &mut requests);
            })
    } else {
        egui::Window::new(rtl::widget_ctx(ctx, text(a, "AI Production and Skills", "الذكاء")))
            .id(egui::Id::unique("ai_production"))
            .open(&mut window_open)
            .default_size(egui::vec2(720.0, 720.0))
            .min_width(420.0)
            .resizable(true)
            .show(ctx, |ui| {
                render_studio_body(
                    ui,
                    &mut state,
                    a,
                    available,
                    has_active_doc,
                    false,
                    &documents,
                    &mut activate_document,
                    &mut body_open,
                    &mut check_preview_update,
                    &mut toggle_language,
                    &mut requests,
                );
            })
    };
    open = window_open && body_open;
    state.open = open;
    if !open {
        state.focus_mode = false;
        state.last_panel_rect = None;
    } else if let Some(resp) = win_resp {
        state.last_panel_rect = Some(resp.response.rect);
    }
    finish_studio_frame(
        app,
        ctx,
        state,
        a,
        StudioFrameOutcome { requests, check_preview_update, toggle_language, activate_document, original_document },
    );
}

fn load_script_path_into_prompt(app: &mut DesignApp, picked: &str) {
    let clean = sanitize_user_path(picked);
    let max_bytes = designcraft_ai_types::protocol::MAX_BRIEF_BYTES as u64;
    let loaded = if let Some(read) = app.services.read.as_mut() {
        read(&clean)
    } else {
        designcraft_engine::io::read_capped(&clean, max_bytes).map_err(|e| format!("{clean}: {e}"))
    };
    match loaded {
        Ok(bytes) => load_script_bytes_into_prompt(app, bytes),
        Err(error) => app.ui.ai.error = error,
    }
}

pub(crate) fn load_script_bytes_into_prompt(app: &mut DesignApp, bytes: Vec<u8>) {
    let a = app.ui.language == "ar";
    if bytes.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES {
        app.ui.ai.error = text(
            a,
            "Script file exceeds 1 MiB. Split or move static reference text into a skill package.",
            "يتجاوز ملف السكربت 1 ميبيبايت. قسّم الملف أو انقل المراجع الثابتة إلى مهارة.",
        )
        .into();
        return;
    }
    match String::from_utf8(bytes) {
        Ok(script) if !script.contains('\0') => {
            if let Some(detected) = explicit_script_pages(&script).or_else(|| natural_prompt_pages(&script))
                && app.ui.ai.new_document
            {
                app.ui.ai.pages = detected;
            }
            app.ui.ai.prompt = script;
            app.ui.ai.edit_large_script_inline = false;
            app.ui.ai.clear_failure_alert();
            app.ui.ai.error.clear();
        }
        _ => {
            app.ui.ai.error = text(
                a,
                "Script file must be valid UTF-8 plain text or Markdown (.md / .txt).",
                "يجب أن يكون ملف السكربت نصاً صالحاً بترميز UTF-8 (.md أو .txt).",
            )
            .into();
        }
    }
}

pub(super) const MAX_AI_TARGET_PDF_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn sanitize_pdf_page_size(size: Option<(f64, f64)>) -> (f64, f64) {
    match size {
        Some((w, h)) if w.is_finite() && h.is_finite() && (36.0..=15552.0).contains(&w) && (36.0..=15552.0).contains(&h) => (w, h),
        _ => (595.27559, 841.88976),
    }
}

pub(super) fn open_target_document_path(app: &mut DesignApp, raw_path: &str) -> Result<serde_json::Value, String> {
    let cleaned = sanitize_user_path(raw_path);
    let path = cleaned.as_str();
    let lower = path.to_ascii_lowercase();
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Imported");
    if [".docx", ".rtf", ".txt", ".md", ".xlsx"].iter().any(|ext| lower.ends_with(ext)) {
        let created = app.run("file.new", serde_json::json!({"preset": "A4", "pages": 1, "facingPages": false, "title": stem}))?;
        match app.open_file("file.place", serde_json::json!({"path": path, "autoflow": true})) {
            Ok(v) => Ok(v),
            Err(error) => {
                if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                    let _ = app.run("file.close", serde_json::json!({"index": index}));
                }
                Err(error)
            }
        }
    } else if lower.ends_with(".pdf") {
        let bytes = if let Some(read) = app.services.read.as_mut() {
            read(path)
        } else {
            designcraft_engine::io::read_capped(path, MAX_AI_TARGET_PDF_BYTES)
                .map_err(|e| format!("{path}: {e}"))
                .and_then(|b| if (b.len() as u64) > MAX_AI_TARGET_PDF_BYTES { Err("PDF file exceeds the 64-MiB import limit".into()) } else { Ok(b) })
        };
        let bytes = match bytes {
            Ok(b) if designcraft_render::is_pdf(&b) => b,
            Ok(_) => return app.open_file("file.open", serde_json::json!({"path": path})),
            Err(error) => return Err(error),
        };
        let pages = designcraft_render::pdf_page_count(&bytes).unwrap_or(1).clamp(1, 500);
        let (w, h) = sanitize_pdf_page_size(designcraft_render::pdf_page_size(&bytes, 0));
        let created = app.run("file.new", serde_json::json!({"width": w, "height": h, "pages": pages, "facingPages": false, "title": stem}))?;
        let mut last = serde_json::Value::Null;
        for page in 1..=pages {
            match app.open_file("file.place", serde_json::json!({"path": path, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w}))
            {
                Ok(v) => last = v,
                Err(error) => {
                    if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                        let _ = app.run("file.close", serde_json::json!({"index": index}));
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    } else {
        app.open_file("file.open", serde_json::json!({"path": path}))
    }
}

pub(crate) fn open_target_document_bytes(app: &mut DesignApp, name: &str, bytes: &[u8]) -> Result<serde_json::Value, String> {
    let lower = name.to_ascii_lowercase();
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Imported");
    let b64 = designcraft_engine::cmd::base64_encode(bytes);
    let result = if [".docx", ".rtf", ".txt", ".md", ".xlsx"].iter().any(|ext| lower.ends_with(ext)) {
        let created = app.run("file.new", serde_json::json!({"preset": "A4", "pages": 1, "facingPages": false, "title": stem}))?;
        match app.open_file("file.place", serde_json::json!({"name": name, "base64": b64, "autoflow": true})) {
            Ok(v) => Ok(v),
            Err(error) => {
                if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                    let _ = app.run("file.close", serde_json::json!({"index": index}));
                }
                Err(error)
            }
        }
    } else if lower.ends_with(".pdf") && designcraft_render::is_pdf(bytes) {
        if (bytes.len() as u64) > MAX_AI_TARGET_PDF_BYTES {
            return Err("PDF file exceeds the 64-MiB import limit".into());
        }
        let pages = designcraft_render::pdf_page_count(bytes).unwrap_or(1).clamp(1, 500);
        let (w, h) = sanitize_pdf_page_size(designcraft_render::pdf_page_size(bytes, 0));
        let created = app.run("file.new", serde_json::json!({"width": w, "height": h, "pages": pages, "facingPages": false, "title": stem}))?;
        let mut last = serde_json::Value::Null;
        let mut shared_asset: Option<u64> = None;
        for page in 1..=pages {
            let params = if let Some(aid) = shared_asset {
                serde_json::json!({"name": name, "reuseAsset": aid, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w})
            } else {
                serde_json::json!({"name": name, "base64": b64, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w})
            };
            match app.open_file("file.place", params) {
                Ok(v) => {
                    if shared_asset.is_none() {
                        shared_asset = v.get("asset").and_then(serde_json::Value::as_u64);
                    }
                    last = v;
                }
                Err(error) => {
                    if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                        let _ = app.run("file.close", serde_json::json!({"index": index}));
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    } else {
        app.open_file("file.openBytes", serde_json::json!({"name": stem, "base64": b64}))
    }?;
    app.ui.ai.open_document_path = name.to_string();
    app.ui.ai.new_document = false;
    app.ui.ai.continue_book = false;
    app.ui.ai.restored_target_unconfirmed = false;
    app.ui.ai.error.clear();
    Ok(result)
}

pub(super) fn preview_texture(read: &crate::ReadFn, path: &str, ctx: &egui::Context, name: &str) -> Result<egui::TextureHandle, String> {
    let bytes = read(path)?;
    if bytes.len() > 24 * 1024 * 1024 {
        return Err("Preview exceeds 24 MiB.".into());
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?.thumbnail(1200, 1600).to_rgba8();
    Ok(crate::widgets::load_texture(
        ctx,
        name,
        egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

pub(super) fn load_preview(app: &mut DesignApp, ctx: &egui::Context) {
    let s = &mut app.ui.ai;
    let Some(path) = s.snapshot.review.as_ref().and_then(|r| r.sample_path.clone()) else {
        return;
    };
    if s.preview_path.as_ref() == Some(&path) {
        return;
    }
    s.preview_path = Some(path.clone());
    s.preview_error.clear();
    let Some(read) = &app.services.read else {
        s.preview_error = "Preview reader unavailable.".into();
        return;
    };
    match preview_texture(read, &path, ctx, "ai_skill_sample") {
        Ok(texture) => s.preview = Some(texture),
        Err(e) => s.preview_error = e,
    }
    ctx.request_repaint();
}

pub(super) fn load_proof_preview(app: &mut DesignApp, ctx: &egui::Context) {
    let s = &mut app.ui.ai;
    let Some(path) = s.proof_path.clone() else {
        return;
    };
    if s.proof_loaded.as_ref() == Some(&path) {
        return;
    }
    s.proof_loaded = Some(path.clone());
    s.proof_error.clear();
    s.proof_texture = None;
    let Some(read) = &app.services.read else {
        s.proof_error = "Proof reader unavailable.".into();
        s.reviewed_proofs.retain(|(page, _)| page != &path);
        return;
    };
    match preview_texture(read, &path, ctx, "ai_batch_proof") {
        Ok(texture) => s.proof_texture = Some(texture),
        Err(e) => {
            s.proof_error = e;
            s.reviewed_proofs.retain(|(page, _)| page != &path);
        }
    }
    ctx.request_repaint();
}

pub(super) fn load_companion_proof(app: &mut DesignApp, ctx: &egui::Context) {
    let path = app.ui.ai.companion_proof_path().map(str::to_owned);
    if app.ui.ai.companion_proof.as_ref().map(|proof| &proof.path) == path.as_ref() {
        return;
    }
    app.ui.ai.companion_proof = None;
    let Some(path) = path else { return };
    let loaded = match &app.services.read {
        Some(read) => preview_texture(read, &path, ctx, "ai_companion_proof"),
        None => Err("Proof reader unavailable.".into()),
    };
    let (texture, error) = match loaded {
        Ok(texture) => (Some(texture), String::new()),
        Err(error) => {
            app.ui.ai.reviewed_proofs.retain(|(page, _)| page != &path);
            (None, error)
        }
    };
    app.ui.ai.companion_proof = Some(CompanionProof { path, texture, error });
    ctx.request_repaint();
}

/// Unified command-entry handoff (`ai.runPrompt`) used by Quick Apply (`Ctrl+Enter`) and the
/// top-bar command search when the user enters natural-language layout or editing instructions.
pub fn run_prompt_command(app: &mut DesignApp, p: &Value) -> Result<Value, String> {
    let has_active_doc = app.session.active().is_some();
    let captured_selection = capture_selection_context(app);
    if p.is_null() || p.as_object().is_some_and(|m| !m.contains_key("prompt")) {
        ensure_ceiling_initialized(app);
        app.ui.ai.open = true;
        app.ui.ai.focus_mode = false;
        app.ui.ai.tab = 0;
        app.ui.ai.restored_target_unconfirmed = false;
        app.ui.ai.focus_prompt_next_frame = true;
        app.ui.ai.live_selection = captured_selection;
        app.ui.ai.scope_to_selection = true;
        app.ui.ai.clear_failure_alert();
        if has_active_doc {
            app.ui.ai.new_document = false;
            app.ui.ai.continue_book = false;
        }
        if app.services.ai.is_some() {
            let _ = dispatch(app, AiRequest::Poll);
        }
        return Ok(serde_json::json!({
            "open": true,
            "dispatched": false,
            "reason": "awaiting_prompt",
            "target": if has_active_doc { "current_document" } else { "new_document" },
            "has_selection": app.ui.ai.live_selection.is_some(),
        }));
    }
    let raw_prompt = p.get("prompt").and_then(Value::as_str).ok_or("missing string: prompt")?;
    let trimmed = raw_prompt.trim();
    if trimmed.is_empty() {
        return Err("prompt must not be empty".into());
    }
    if trimmed.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES || trimmed.contains('\0') {
        return Err("prompt exceeds permitted size or contains invalid characters".into());
    }
    let auto_run = p.get("run").map(|v| v.as_bool().ok_or("run must be boolean")).transpose()?.unwrap_or(true);

    ensure_ceiling_initialized(app);
    let ceiling = app.ui.ai.settings.clone();

    app.ui.ai.open = true;
    app.ui.ai.focus_mode = false;
    app.ui.ai.tab = 0;
    app.ui.ai.prompt = trimmed.to_owned();
    app.ui.ai.live_selection = captured_selection;
    app.ui.ai.restored_target_unconfirmed = false;
    app.ui.ai.clear_failure_alert();
    if has_active_doc {
        app.ui.ai.new_document = false;
        app.ui.ai.continue_book = false;
    } else {
        app.ui.ai.new_document = true;
        app.ui.ai.continue_book = false;
        if let Some(detected_pages) = explicit_script_pages(trimmed).or_else(|| natural_prompt_pages(trimmed)) {
            app.ui.ai.pages = detected_pages;
        }
    }

    let documents: Vec<_> =
        app.session.documents().iter().enumerate().map(|(index, document)| OpenDocument::from_document(index, document)).collect();
    let active_index = app.session.active_index();
    let available = app.services.ai.is_some() && app.ui.ai.snapshot.available;
    let blocking = production_blocking_issue(&app.ui.ai, available, &documents, active_index, app.ui.language == "ar");

    if auto_run && blocking.is_none() {
        let mut req = app.ui.ai.production_task();
        clamp_control_settings(&ceiling, &mut req.settings);
        let _ = dispatch(app, AiRequest::Start(req))?;
        Ok(serde_json::json!({
            "open": true,
            "dispatched": true,
            "target": if has_active_doc { "current_document" } else { "new_document" },
        }))
    } else {
        if app.services.ai.is_some() {
            let _ = dispatch(app, AiRequest::Poll);
        }
        Ok(serde_json::json!({
            "open": true,
            "dispatched": false,
            "reason": blocking,
            "target": if has_active_doc { "current_document" } else { "new_document" },
        }))
    }
}
