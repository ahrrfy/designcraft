//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{
    AiRequest, AiSettings, AiSnapshot, BatchSnapshot, ProductionDraft, SkillCard, TaskRequest, TaskTarget, TokenRates,
};
use designcraft_ai_types::{
    ArabicTashkeelMode, ExecutionPacing, ModelSelection, OutputMode, ProviderKind, VisualDensityMode, is_non_image_model, sanitize_user_path,
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

use panel_local::*;
use panel_production::*;
use panel_providers::*;
use panel_skills::*;
use panel_usage::*;
pub use state::AiUiState;
use state::*;
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

pub fn request_command(app: &mut DesignApp, p: &Value) -> Result<Value, String> {
    let field = |name: &str| p.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string: {name}"));
    let decode = |name: &str| p.get(name).cloned().ok_or_else(|| format!("missing {name}"));
    let request = match field("method")?.as_str() {
        "poll" => AiRequest::Poll,
        "cancel" => AiRequest::Cancel,
        "start" => {
            ensure_ceiling_initialized(app);
            let mut req: TaskRequest = serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?;
            clamp_control_settings(&app.ui.ai.settings, &mut req.settings);
            AiRequest::Start(req)
        }
        "preview_production" => AiRequest::PreviewProduction(serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?),
        "update_settings" => {
            ensure_ceiling_initialized(app);
            AiRequest::UpdateSettings(clamped_settings(&app.ui.ai.settings, decode("settings")?)?)
        }
        "list_models" => AiRequest::ListModels { selection: serde_json::from_value(decode("selection")?).map_err(|e| e.to_string())? },
        "import_skill" => AiRequest::ImportSkill { path: field("path")? },
        "inspect_skill" => AiRequest::InspectSkill { id: field("id")?, revision: field("revision")? },
        "prepare_skills" => AiRequest::PrepareSkills { prompt: field("prompt")? },
        "save_skill_draft" => AiRequest::SaveSkillDraft { id: field("id")?, skill_md: field("skill_md")?, manifest_json: field("manifest_json")? },
        "add_design_correction" => AiRequest::AddDesignCorrection {
            id: field("id")?,
            revision: field("revision")?,
            correction: serde_json::from_value(decode("correction")?).map_err(|e| e.to_string())?,
        },
        "generate_skill" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkill {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: field("prompt")?,
                reference_paths: serde_json::from_value(decode("reference_paths")?).map_err(|e| e.to_string())?,
                settings: clamped_settings(&app.ui.ai.settings, decode("settings")?)?,
            }
        }
        "generate_skill_from_document" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkillFromDocument {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: field("prompt")?,
                settings: clamped_settings(&app.ui.ai.settings, decode("settings")?)?,
            }
        }
        "run_skill_sample" => AiRequest::RunSkillSample { id: field("id")?, revision: field("revision")? },
        "run_design_template" => AiRequest::RunDesignTemplate { id: field("id")?, revision: field("revision")?, template: field("template")? },
        "refresh_batch_proofs" => AiRequest::RefreshBatchProofs { job_id: field("job_id")? },
        "pause_batch" => AiRequest::PauseBatch { job_id: field("job_id")? },
        "resume_batch" => {
            ensure_ceiling_initialized(app);
            if app.ui.ai.settings.execution_pacing == ExecutionPacing::ReviewEveryBatch {
                return Err("Approval requires the human review button in the desktop panel.".into());
            }
            AiRequest::ResumeBatch { job_id: field("job_id")? }
        }
        "dismiss_image_regeneration" => AiRequest::DismissImageRegeneration { job_id: field("job_id")? },
        "approve_skill" | "approve_batch" | "confirm_image_regeneration" => {
            return Err("Approval requires the human review button in the desktop panel.".into());
        }
        "save_api_key" | "remove_api_key" | "refresh_credentials" => {
            return Err("Credential actions require the masked human controls in Providers and keys.".into());
        }
        "refresh_resources" => AiRequest::RefreshResources,
        "set_skill_enabled" => {
            AiRequest::SetSkillEnabled { id: field("id")?, enabled: p.get("enabled").and_then(Value::as_bool).ok_or("missing enabled")? }
        }
        "rollback_skill" => AiRequest::RollbackSkill { id: field("id")?, revision: field("revision")? },
        "export_skill" => AiRequest::ExportSkill { id: field("id")?, revision: field("revision")?, path: field("path")? },
        "create_science_sample" => AiRequest::CreateScienceSample { directory: field("directory")? },
        "export_document" => AiRequest::ExportDocument { path: field("path")?, pdf: p.get("pdf").and_then(Value::as_bool).ok_or("missing pdf")? },
        "export_idml" => AiRequest::ExportIdml { path: field("path")? },
        "export_indd" => AiRequest::ExportIndd { path: field("path")? },
        "prepare_print" => AiRequest::PreparePrint {
            printer: p.get("printer").and_then(Value::as_str).map(str::to_owned),
            grayscale: p.get("grayscale").and_then(Value::as_bool).unwrap_or(false),
            duplex: field("duplex")?,
        },
        "save_prepared_print" => AiRequest::SavePreparedPrint { token: field("token")?, path: field("path")? },
        "print_document" | "submit_prepared_print" | "open_print_proof" | "start_local_ollama" | "stop_local_ollama" => {
            return Err("Printing and local service launch require the explicit human controls in the desktop panel.".into());
        }
        _ => return Err("Unknown AI request method; human-only actions require desktop controls.".into()),
    };
    dispatch(app, request)
}

pub fn poll(app: &mut DesignApp, ctx: &egui::Context) {
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
pub(super) fn field(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str, value: &mut String) {
    label(ui, arabic, en, ar);
    let field_rtl = if value.trim().is_empty() { arabic } else { rtl::has_rtl(value) };
    let mut layouter = bidi_layouter(false);
    rtl::show_text_edit(
        ui,
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .horizontal_align(if field_rtl { egui::Align::Max } else { egui::Align::Min })
            .layouter(&mut layouter),
    );
}
pub(super) fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::ComfyUi => "ComfyUI",
    }
}
pub(super) fn provider(ui: &mut egui::Ui, id: &str, selected: &mut ProviderKind, images: bool) {
    egui::ComboBox::from_id_salt(id).selected_text(provider_name(*selected)).show_ui(ui, |ui| {
        for p in [ProviderKind::OpenAi, ProviderKind::Gemini, if images { ProviderKind::ComfyUi } else { ProviderKind::Ollama }] {
            ui.selectable_value(selected, p, provider_name(p));
        }
    });
}
pub(super) fn model(
    ui: &mut egui::Ui,
    a: bool,
    id: &str,
    m: &mut ModelSelection,
    images: bool,
    pending_models: &mut Option<ModelSelection>,
    discovered: &BTreeMap<(ProviderKind, Option<String>), Vec<String>>,
    requests: &mut Vec<AiRequest>,
) {
    ui.horizontal_wrapped(|ui| {
        provider(ui, id, &mut m.provider, images);
        label(ui, a, "Model", "النموذج");
        ui.add(egui::TextEdit::singleline(&mut m.model).desired_width(240.0));
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
            egui::ComboBox::from_id_salt((id, "discovered_models")).selected_text(rtl::widget(ui, picker_label)).show_ui(ui, |ui| {
                for name in filtered {
                    ui.selectable_value(&mut m.model, name.clone(), name);
                }
            });
        }
    }
    let mut endpoint = m.endpoint.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        label(ui, a, "Endpoint (optional)", "عنوان الخدمة (اختياري)");
        if ui.add(egui::TextEdit::singleline(&mut endpoint).desired_width(350.0)).changed() {
            m.endpoint = (!endpoint.trim().is_empty()).then_some(endpoint);
        }
    });
}

pub fn show(app: &mut DesignApp, ctx: &egui::Context) {
    if !app.ui.ai.open {
        return;
    }
    let document = app.session.active().map(|document| (document.uid, document.revision));
    let needs_validation = !app.ui.ai.snapshot.busy
        && app.ui.ai.snapshot.batch.as_ref().is_some_and(|batch| {
            batch.status == "awaiting_human_proof"
                && document.is_some_and(|(uid, revision)| Some(uid) == batch.document_uid && Some(revision) != batch.document_revision)
        });
    if needs_validation && app.ui.ai.proof_validation_requested != document && app.services.ai.is_some() {
        app.ui.ai.proof_validation_requested = document;
        // Selection and page navigation advance the cache revision without changing content.
        // Keep review evidence disabled until the host validates the pinned document allocation,
        // font cache and preferences; a rejected version is requested only once.
        app.ui.ai.sync_proof_document(document);
        if let Err(error) = dispatch(app, AiRequest::Poll) {
            app.ui.ai.error = error;
        }
    }
    let mut state = std::mem::take(&mut app.ui.ai);
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
    let mut open = state.open;
    let mut requests = Vec::new();
    let mut check_preview_update = false;
    let mut toggle_language = false;
    let available = app.services.ai.is_some() && state.snapshot.available;
    let documents: Vec<_> =
        app.session.documents().iter().enumerate().map(|(index, document)| OpenDocument::from_document(index, document)).collect();
    let mut activate_document = app.session.active_index();
    let original_document = activate_document;
    egui::Window::new(rtl::widget_ctx(ctx, text(a, "AI Production and Skills", "الذكاء")))
        .id(egui::Id::new("ai_production"))
        .open(&mut open)
        .default_size(egui::vec2(720.0, 720.0))
        .min_width(540.0)
        .resizable(true)
        .show(ctx, |ui| {
            check_preview_update = state.preview_update.show(ui, a);
            ui.separator();
            ui.horizontal(|ui| {
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
                if button(ui, a, "Refresh", "تحديث") {
                    requests.push(AiRequest::Poll);
                }
                if ui.button(rtl::widget(ui, if a { "EN" } else { "العربية" })).clicked() {
                    toggle_language = true;
                }
            });
            ui.separator();
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
                        if usage.has_unreported_usage {
                            text(a, " · Some usage unreported", " · بعض الاستخدام غير مبلغ")
                        } else {
                            ""
                        }
                    ),
                );
                ui.horizontal_wrapped(|ui| {
                    if button(ui, a, "Cancel task — keep produced pages", "إلغاء المهمة مع الاحتفاظ بالصفحات المنتجة")
                    {
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
                // The proof and its actions share a bounded, non-scrolling workspace.
                production(ui, &mut state, a, available, &documents, &mut activate_document, &mut requests);
            } else {
                egui::ScrollArea::vertical().id_salt("ai_panel_body").max_height(650.0).show(ui, |ui| match state.tab {
                    1 => skills(ui, &mut state, a, available, app.session.active().is_some(), &mut requests),
                    2 => usage(ui, &state, a),
                    3 => providers(ui, &mut state, a, available, &mut requests),
                    4 => local_ai(ui, &mut state, a, available, app.services.pick_open.is_some(), &mut requests),
                    _ => production(ui, &mut state, a, available, &documents, &mut activate_document, &mut requests),
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
        });
    state.open = open;
    if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        state.review_pointer_press = None;
    }
    app.ui.ai = state;
    if toggle_language && let Err(error) = app.run("app.language", serde_json::json!({"lang": if a { "" } else { "ar" }})) {
        app.ui.ai.error = error;
    }
    if check_preview_update && let Err(error) = app.run("app.checkPreviewUpdate", serde_json::json!({})) {
        app.ui.ai.error = error;
    }
    if activate_document != original_document {
        if let Some(index) = activate_document
            && let Err(error) = app.run("file.activate", serde_json::json!({"index":index}))
        {
            app.ui.ai.error = error;
        }
        // A document choice is never implicit authority to start a task in that same frame.
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_ollama)
        && let Some(path) = app.services.pick_open.as_mut().and_then(|pick| pick("ollama_executable"))
    {
        app.ui.ai.ollama_executable = path;
        ctx.request_repaint();
    }
    if std::mem::take(&mut app.ui.ai.pick_script_file) {
        if let Some(picked) = app.services.pick_open.as_mut().and_then(|pick| pick("ai_script")) {
            let clean = sanitize_user_path(&picked);
            let max_bytes = designcraft_ai_types::protocol::MAX_BRIEF_BYTES as u64;
            let loaded = if let Some(read) = app.services.read.as_mut() {
                read(&clean)
            } else {
                designcraft_engine::io::read_capped(&clean, max_bytes).map_err(|e| format!("{clean}: {e}"))
            };
            match loaded {
                Ok(bytes) if bytes.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES => {
                    app.ui.ai.error = text(
                        a,
                        "Script file exceeds 1 MiB. Split or move static reference text into a skill package.",
                        "يتجاوز ملف السكربت 1 ميبيبايت. قسّم الملف أو انقل المراجع الثابتة إلى مهارة.",
                    )
                    .into();
                }
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(script) if !script.contains('\0') => {
                        if let Some(detected) = explicit_script_pages(&script).or_else(|| natural_prompt_pages(&script))
                            && app.ui.ai.new_document
                        {
                            app.ui.ai.pages = detected;
                        }
                        app.ui.ai.prompt = script;
                        app.ui.ai.edit_large_script_inline = false;
                        app.ui.ai.clear_failure_alert();
                        ctx.request_repaint();
                    }
                    _ => {
                        app.ui.ai.error = text(
                            a,
                            "Script file must be valid UTF-8 plain text or Markdown (.md / .txt).",
                            "يجب أن يكون ملف السكربت نصاً صالحاً بترميز UTF-8 (.md أو .txt).",
                        )
                        .into();
                    }
                },
                Err(error) => app.ui.ai.error = error,
            }
        }
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_target_document) {
        if app.services.pick_open.is_some() {
            if let Some(path) = app.services.pick_open.as_mut().and_then(|pick| pick("ai_target_document")) {
                app.ui.ai.open_document_path = path.clone();
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
        } else if let Err(error) = app.pick_and_open("ai_target_document") {
            app.ui.ai.error = error;
        }
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
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
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if !requests.is_empty() {
        ctx.request_repaint();
    }
    for request in requests {
        if let Err(e) = dispatch(app, request) {
            app.ui.ai.error = e;
        }
    }
    app.ui.ai.batch_approval_gesture = None;
    load_preview(app, ctx);
    load_proof_preview(app, ctx);
    load_companion_proof(app, ctx);
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
    if p.is_null() || p.as_object().is_some_and(|m| !m.contains_key("prompt")) {
        ensure_ceiling_initialized(app);
        app.ui.ai.open = true;
        app.ui.ai.tab = 0;
        app.ui.ai.restored_target_unconfirmed = false;
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
    app.ui.ai.tab = 0;
    app.ui.ai.prompt = trimmed.to_owned();
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
