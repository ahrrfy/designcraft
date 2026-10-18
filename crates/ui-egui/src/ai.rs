//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{AiRequest, AiSettings, AiSnapshot, TaskRequest, TaskTarget, TokenRates};
use designcraft_ai_types::{ModelSelection, OutputMode, ProviderKind};
use serde_json::Value;

/// Never persisted, and never included in a debug dump.
#[derive(Clone, Default)]
struct Secret(String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

#[derive(Clone)]
pub struct AiUiState {
    pub open: bool,
    pub tab: usize,
    pub snapshot: AiSnapshot,
    pub settings: AiSettings,
    initialized: bool,
    last_poll: Option<Instant>,
    api_key: Secret,
    key_for_images: bool,
    prompt: String,
    new_document: bool,
    pages: u32,
    automatic_skills: bool,
    selected_skills: BTreeSet<String>,
    directory: String,
    import_path: String,
    export_path: String,
    skill_prompt: String,
    references: String,
    review_key: Option<(String, String)>,
    skill_md: String,
    manifest_json: String,
    preview_path: Option<String>,
    preview: Option<egui::TextureHandle>,
    preview_error: String,
    error: String,
    printer: String,
    grayscale: bool,
    duplex: String,
    rates_confirmed: bool,
    workflow_json: String,
    workflow_error: String,
    proof_key: Option<(String, u32)>,
    proof_path: Option<String>,
    proof_loaded: Option<String>,
    proof_texture: Option<egui::TextureHandle>,
    proof_error: String,
    local_ollama: ModelSelection,
    local_comfy: ModelSelection,
    pending_models: Option<ModelSelection>,
    local_probe: Option<(ModelSelection, Vec<String>, String, bool)>,
    print_token: Option<String>,
    print_reviewed: bool,
    print_save_path: String,
    ollama_executable: String,
    pick_ollama: bool,
}
impl std::fmt::Debug for AiUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiUiState")
            .field("open", &self.open)
            .field("tab", &self.tab)
            .field("busy", &self.snapshot.busy)
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}
impl Default for AiUiState {
    fn default() -> Self {
        Self {
            open: false,
            tab: 0,
            snapshot: AiSnapshot::default(),
            settings: AiSettings::default(),
            initialized: false,
            last_poll: None,
            api_key: Secret::default(),
            key_for_images: false,
            prompt: String::new(),
            new_document: true,
            pages: 8,
            automatic_skills: true,
            selected_skills: BTreeSet::new(),
            directory: String::new(),
            import_path: String::new(),
            export_path: String::new(),
            skill_prompt: String::new(),
            references: String::new(),
            review_key: None,
            skill_md: String::new(),
            manifest_json: String::new(),
            preview_path: None,
            preview: None,
            preview_error: String::new(),
            error: String::new(),
            printer: String::new(),
            grayscale: false,
            duplex: "off".into(),
            rates_confirmed: false,
            workflow_json: String::new(),
            workflow_error: String::new(),
            proof_key: None,
            proof_path: None,
            proof_loaded: None,
            proof_texture: None,
            proof_error: String::new(),
            local_ollama: ModelSelection { provider: ProviderKind::Ollama, model: String::new(), endpoint: Some("http://127.0.0.1:11434".into()) },
            local_comfy: ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: Some("http://127.0.0.1:8188".into()) },
            pending_models: None,
            local_probe: None,
            print_token: None,
            print_reviewed: false,
            print_save_path: String::new(),
            ollama_executable: String::new(),
            pick_ollama: false,
        }
    }
}

impl AiUiState {
    fn accept(&mut self, snapshot: AiSnapshot) {
        if !self.initialized && snapshot.available {
            self.settings = snapshot.settings.clone();
            self.workflow_json = self.settings.comfy_workflow.as_ref().map(Value::to_string).unwrap_or_default();
            if self.settings.text_model.provider == ProviderKind::Ollama {
                self.local_ollama = self.settings.text_model.clone();
            }
            if self.settings.image_model.provider == ProviderKind::ComfyUi {
                self.local_comfy = self.settings.image_model.clone();
            }
            self.initialized = true;
        }
        let key = snapshot.review.as_ref().map(|r| (r.id.clone(), r.revision.clone()));
        if key != self.review_key {
            if let Some(r) = &snapshot.review {
                self.skill_md = r.skill_md.clone();
                self.manifest_json = r.manifest_json.clone();
            }
            self.review_key = key;
        }
        if self.snapshot.review.as_ref().and_then(|r| r.sample_path.as_ref()) != snapshot.review.as_ref().and_then(|r| r.sample_path.as_ref()) {
            self.preview_path = None;
            self.preview = None;
        }
        let proof_key = snapshot.batch.as_ref().map(|b| (b.job_id.clone(), b.checkpoint));
        if self.proof_key != proof_key {
            self.proof_path = snapshot
                .batch
                .as_ref()
                .and_then(|b| b.batches.iter().find(|report| report.batch == b.current_batch))
                .and_then(|r| r.proof_paths.iter().find(|p| p.to_ascii_lowercase().ends_with(".png")))
                .cloned();
            self.proof_loaded = None;
            self.proof_texture = None;
            self.proof_error.clear();
            self.proof_key = proof_key;
        }
        if !snapshot.busy
            && let Some(selection) = self.pending_models.take()
        {
            let success = snapshot.stage == "completed";
            self.local_probe = Some((selection, if success { snapshot.models.clone() } else { Vec::new() }, snapshot.message.clone(), success));
        }
        let print_token = snapshot.print_proof.as_ref().map(|proof| proof.token.clone());
        if print_token != self.print_token {
            self.print_token = print_token;
            self.print_reviewed = false;
        }
        self.snapshot = snapshot;
    }

    fn task_active(&self) -> bool {
        self.snapshot.busy || self.snapshot.batch.as_ref().is_some_and(|b| matches!(b.status.as_str(), "running" | "awaiting_human_proof"))
    }

    /// Parse only after an edit. The raw draft survives partial JSON, repaints and host polls.
    fn edit_workflow(&mut self) {
        match serde_json::from_str(&self.workflow_json) {
            Ok(value) => {
                self.settings.comfy_workflow = Some(value);
                self.workflow_error.clear();
            }
            Err(error) => {
                self.settings.comfy_workflow = None;
                self.workflow_error = error.to_string();
            }
        }
    }

    fn workflow_ready(&self) -> bool {
        self.settings.output_mode != OutputMode::NativeWithImages
            || self.settings.image_model.provider != ProviderKind::ComfyUi
            || (self.workflow_error.is_empty() && self.settings.comfy_workflow.is_some())
    }
}

/// Typed UI path shared by all panel buttons. Credentials move directly to the host without JSON,
/// logs, serialization, document metadata or preference storage.
pub fn dispatch(app: &mut DesignApp, request: AiRequest) -> Result<Value, String> {
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
    let snapshot = host(&mut app.session, request);
    let result = serde_json::to_value(&snapshot).map_err(|e| e.to_string());
    app.ui.ai.accept(snapshot);
    result
}

/// Control channel / MCP command path: `ai.request {method, ...}`. Methods match the snake_case
/// AiRequest variants. `start` accepts `request: TaskRequest`; settings are `settings: AiSettings`;
/// model discovery accepts `selection: ModelSelection`. All other fields match the typed variant.
/// Credentials, human approvals, physical print submission and owned-process launch use only
/// typed panel controls. Secret input never enters control transcripts.
pub fn request_command(app: &mut DesignApp, p: &Value) -> Result<Value, String> {
    let field = |name: &str| p.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string: {name}"));
    let decode = |name: &str| p.get(name).cloned().ok_or_else(|| format!("missing {name}"));
    let request = match field("method")?.as_str() {
        "poll" => AiRequest::Poll,
        "cancel" => AiRequest::Cancel,
        "start" => AiRequest::Start(serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?),
        "update_settings" => AiRequest::UpdateSettings(serde_json::from_value(decode("settings")?).map_err(|e| e.to_string())?),
        "list_models" => AiRequest::ListModels { selection: serde_json::from_value(decode("selection")?).map_err(|e| e.to_string())? },
        "import_skill" => AiRequest::ImportSkill { path: field("path")? },
        "inspect_skill" => AiRequest::InspectSkill { id: field("id")?, revision: field("revision")? },
        "save_skill_draft" => AiRequest::SaveSkillDraft { id: field("id")?, skill_md: field("skill_md")?, manifest_json: field("manifest_json")? },
        "generate_skill" => AiRequest::GenerateSkill {
            prompt: field("prompt")?,
            reference_paths: serde_json::from_value(decode("reference_paths")?).map_err(|e| e.to_string())?,
            settings: serde_json::from_value(decode("settings")?).map_err(|e| e.to_string())?,
        },
        "generate_skill_from_document" => AiRequest::GenerateSkillFromDocument {
            prompt: field("prompt")?,
            settings: serde_json::from_value(decode("settings")?).map_err(|e| e.to_string())?,
        },
        "run_skill_sample" => AiRequest::RunSkillSample { id: field("id")?, revision: field("revision")? },
        "approve_skill" | "approve_batch" => {
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

fn text<'a>(arabic: bool, en: &'a str, ar: &'a str) -> &'a str {
    if arabic { ar } else { en }
}
fn label(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) {
    rtl::label(ui, text(arabic, en, ar));
}
fn button(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) -> bool {
    ui.button(rtl::widget(ui, text(arabic, en, ar))).clicked()
}
fn field(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str, value: &mut String) {
    label(ui, arabic, en, ar);
    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
}
fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::ComfyUi => "ComfyUI",
    }
}
fn provider(ui: &mut egui::Ui, id: &str, selected: &mut ProviderKind, images: bool) {
    egui::ComboBox::from_id_salt(id).selected_text(provider_name(*selected)).show_ui(ui, |ui| {
        for p in [ProviderKind::OpenAi, ProviderKind::Gemini, if images { ProviderKind::ComfyUi } else { ProviderKind::Ollama }] {
            ui.selectable_value(selected, p, provider_name(p));
        }
    });
}
fn model(ui: &mut egui::Ui, a: bool, id: &str, m: &mut ModelSelection, images: bool, requests: &mut Vec<AiRequest>) {
    ui.horizontal(|ui| {
        provider(ui, id, &mut m.provider, images);
        label(ui, a, "Model", "النموذج");
        ui.add(egui::TextEdit::singleline(&mut m.model).desired_width(240.0));
        if button(ui, a, "Discover", "اكتشاف") {
            requests.push(AiRequest::ListModels { selection: m.clone() });
        }
    });
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
    let mut state = std::mem::take(&mut app.ui.ai);
    let a = app.ui.language == "ar";
    let mut open = state.open;
    let mut requests = Vec::new();
    let available = app.services.ai.is_some() && state.snapshot.available;
    egui::Window::new(text(a, "AI Production and Skills", "الذكاء"))
        .id(egui::Id::new("ai_production"))
        .open(&mut open)
        .default_size(egui::vec2(720.0, 720.0))
        .min_width(540.0)
        .resizable(true)
        .show(ctx, |ui| {
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
                if button(ui, a, "Cancel task — keep produced pages", "إلغاء المهمة مع الاحتفاظ بالصفحات المنتجة")
                {
                    requests.push(AiRequest::Cancel);
                }
            }
            if !state.snapshot.message.is_empty() {
                let summary: String = state.snapshot.message.lines().next().unwrap_or("").chars().take(140).collect();
                rtl::label(ui, summary);
            }
            egui::ScrollArea::vertical().max_height(650.0).show(ui, |ui| {
                match state.tab {
                    1 => skills(ui, &mut state, a, available, app.session.active().is_some(), &mut requests),
                    2 => usage(ui, &state, a),
                    3 => providers(ui, &mut state, a, available, &mut requests),
                    4 => local_ai(ui, &mut state, a, available, app.services.pick_open.is_some(), &mut requests),
                    _ => production(ui, &mut state, a, available, app.session.active().is_some(), &mut requests),
                }
                if !state.error.is_empty() || !state.snapshot.message.is_empty() {
                    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Details", "التفاصيل"))).show(ui, |ui| {
                        rtl::label(ui, &state.snapshot.message);
                        rtl::label(ui, &state.error);
                    });
                }
            });
        });
    state.open = open;
    app.ui.ai = state;
    if std::mem::take(&mut app.ui.ai.pick_ollama)
        && let Some(path) = app.services.pick_open.as_mut().and_then(|pick| pick("ollama_executable"))
    {
        app.ui.ai.ollama_executable = path;
        ctx.request_repaint();
    }
    if !requests.is_empty() {
        ctx.request_repaint();
    }
    for request in requests {
        if let Err(e) = dispatch(app, request) {
            app.ui.ai.error = e;
        }
    }
    load_preview(app, ctx);
    load_proof_preview(app, ctx);
}

fn providers(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, requests: &mut Vec<AiRequest>) {
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Models and credentials", "النماذج ومفاتيح الاتصال"))).default_open(true).show(ui, |ui| {
            label(ui, a, "Text and layout model", "نموذج النص والتخطيط");
            model(ui, a, "ai_text_provider", &mut s.settings.text_model, false, requests);
            label(ui, a, "Image model (separate)", "نموذج الصور (مستقل)");
            model(ui, a, "ai_image_provider", &mut s.settings.image_model, true, requests);
            for name in &s.snapshot.models {
                rtl::label(ui, name);
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut s.settings.output_mode, OutputMode::NativeOnly, rtl::widget(ui, text(a, "Native only", "عناصر أصلية فقط")));
                ui.selectable_value(
                    &mut s.settings.output_mode,
                    OutputMode::NativeWithImages,
                    rtl::widget(ui, text(a, "Native with images", "عناصر أصلية مع صور")),
                );
            });
            if s.settings.image_model.provider == ProviderKind::ComfyUi && s.settings.output_mode == OutputMode::NativeWithImages {
                label(ui, a, "ComfyUI workflow (API JSON)", "مخطط ComfyUI بصيغة API JSON");
                label(
                    ui,
                    a,
                    "Use {{prompt}} and {{model}} inside string values; each task fills in its own prompt and model.",
                    "ضع {{prompt}} و{{model}} داخل القيم النصية؛ تُستبدل بتعليمات المهمة ونموذجها.",
                );
                if ui.add(egui::TextEdit::multiline(&mut s.workflow_json).desired_rows(3).desired_width(f32::INFINITY)).changed() {
                    s.edit_workflow();
                }
                if !s.workflow_error.is_empty() {
                    label(
                        ui,
                        a,
                        "Complete valid JSON before applying settings or starting image production.",
                        "أكمل JSON صالحاً قبل تطبيق الإعدادات أو بدء إنتاج الصور.",
                    );
                    rtl::label(ui, &s.workflow_error);
                }
            }
            ui.separator();
            label(
                ui,
                a,
                "Credentials — saved with operating-system protection; stored secrets are never displayed",
                "المفاتيح محفوظة بحماية نظام التشغيل ولا تُعرض بعد الحفظ",
            );
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.key_for_images, false, rtl::widget(ui, text(a, "Text provider", "مزود النص")));
                ui.radio_value(&mut s.key_for_images, true, rtl::widget(ui, text(a, "Image provider", "مزود الصور")));
            });
            let selection = if s.key_for_images { s.settings.image_model.clone() } else { s.settings.text_model.clone() };
            rtl::label(ui, format!("{} / {}", provider_name(selection.provider), selection.endpoint.as_deref().unwrap_or("default endpoint")));
            ui.add(egui::TextEdit::singleline(&mut s.api_key.0).password(true).char_limit(4096).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                let remote = matches!(selection.provider, ProviderKind::OpenAi | ProviderKind::Gemini);
                if ui
                    .add_enabled(
                        remote && !s.api_key.0.trim().is_empty(),
                        egui::Button::new(rtl::widget(ui, text(a, "Save protected key", "حفظ المفتاح المحمي"))),
                    )
                    .clicked()
                {
                    requests.push(AiRequest::SaveApiKey { selection: selection.clone(), key: std::mem::take(&mut s.api_key.0) });
                }
                if button(ui, a, "Refresh presence", "تحديث حالة المفاتيح") {
                    requests.push(AiRequest::RefreshCredentials);
                }
                if ui.add_enabled(remote, egui::Button::new(rtl::widget(ui, text(a, "Remove stored key", "حذف المفتاح المحفوظ")))).clicked()
                {
                    requests.push(AiRequest::RemoveApiKey { selection: selection.clone() });
                }
            });
            if s.snapshot.credentials.is_empty() {
                label(
                    ui,
                    a,
                    "Refresh presence to check protected storage and environment credentials.",
                    "حدّث الحالة لفحص المفاتيح المحمية ومتغيرات البيئة.",
                );
            }
            for credential in &s.snapshot.credentials {
                rtl::label(
                    ui,
                    format!(
                        "{} / {}",
                        provider_name(credential.selection.provider),
                        credential.selection.endpoint.as_deref().unwrap_or("default endpoint")
                    ),
                );
                label(
                    ui,
                    a,
                    if credential.persisted { "OS-protected key saved" } else { "No protected key saved" },
                    if credential.persisted { "مفتاح محمي محفوظ" } else { "لا يوجد مفتاح محمي محفوظ" },
                );
                label(
                    ui,
                    a,
                    if credential.available { "Credential available" } else { "Credential unavailable" },
                    if credential.available { "مفتاح الاتصال متاح" } else { "مفتاح الاتصال غير متاح" },
                );
                rtl::label(ui, &credential.message);
            }
            label(
                ui,
                a,
                "Environment credentials are also supported. A custom endpoint has its own credential entry.",
                "يمكن استخدام متغيرات البيئة أيضاً. لكل عنوان خدمة مخصص مفتاح مستقل.",
            );
            if ui.add_enabled(s.workflow_ready(), egui::Button::new(rtl::widget(ui, text(a, "Apply settings", "تطبيق الإعدادات")))).clicked()
            {
                requests.push(AiRequest::UpdateSettings(s.settings.clone()));
            }
        });
    });
}

fn local_ai(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, can_pick: bool, requests: &mut Vec<AiRequest>) {
    label(ui, a, "Local inference services", "خدمات الذكاء الاصطناعي المحلية");
    label(
        ui,
        a,
        "Connect to installed services, check the reported models, then select one for production.",
        "اتصل بالخدمات المثبتة وافحص النماذج المتاحة ثم اختر نموذجاً للإنتاج.",
    );
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        label(ui, a, "Owned Ollama process", "عملية Ollama التي يشغلها التطبيق");
        field(ui, a, "Installed ollama.exe (absolute path)", "مسار ollama.exe المثبت (مسار كامل)", &mut s.ollama_executable);
        ui.horizontal(|ui| {
            if ui.add_enabled(can_pick, egui::Button::new(rtl::widget(ui, text(a, "Choose executable", "اختيار الملف التنفيذي")))).clicked() { s.pick_ollama = true; }
            let owned_running = s.snapshot.local_server.as_ref().is_some_and(|server| server.owned && server.running);
            if ui.add_enabled(!owned_running && std::path::Path::new(&s.ollama_executable).is_absolute(), egui::Button::new(rtl::widget(ui, text(a, "Start selected Ollama", "تشغيل Ollama المحدد")))).clicked() {
                s.local_ollama.endpoint = Some("http://127.0.0.1:11434".into());
                requests.push(AiRequest::StartLocalOllama { executable: s.ollama_executable.clone() });
            }
            if ui.add_enabled(owned_running, egui::Button::new(rtl::widget(ui, text(a, "Stop owned process", "إيقاف العملية التابعة للتطبيق")))).clicked() { requests.push(AiRequest::StopLocalOllama); }
        });
        label(ui, a, "Starts the selected installed server on 127.0.0.1:11434. It does not install or download models; other running services are left alone.", "يشغّل الخادم المثبت المحدد على 127.0.0.1:11434. لا يثبت أو ينزل نماذج ولا يوقف الخدمات الأخرى.");
        if let Some(server) = &s.snapshot.local_server {
            rtl::label(ui, format!("{} · {} · PID {:?}", server.endpoint, text(a, if server.running { "Running" } else { "Stopped" }, if server.running { "قيد التشغيل" } else { "متوقف" }), server.pid));
            rtl::label(ui, &server.message);
        }

        for images in [false, true] {
            let selection = if images { &mut s.local_comfy } else { &mut s.local_ollama };
            ui.separator();
            rtl::label(ui, provider_name(selection.provider));
            let mut endpoint = selection.endpoint.clone().unwrap_or_default();
            field(ui, a, "Service endpoint", "عنوان الخدمة", &mut endpoint);
            selection.endpoint = (!endpoint.trim().is_empty()).then_some(endpoint);
            field(ui, a, "Model name", "اسم النموذج", &mut selection.model);
            if button(ui, a, "Check connection and discover models", "فحص الاتصال واكتشاف النماذج") { requests.push(AiRequest::ListModels { selection: selection.clone() }); }
            let matching = s.local_probe.as_ref().filter(|(probed, _, _, _)| probed.provider == selection.provider && probed.endpoint == selection.endpoint);
            if let Some((_, models, message, connected)) = matching {
                label(ui, a, if *connected { "Connected: discovery succeeded" } else { "Connection failed or cancelled" }, if *connected { "متصل: نجح اكتشاف النماذج" } else { "فشل الاتصال أو ألغي الفحص" });
                rtl::label(ui, message);
                if *connected && models.is_empty() { label(ui, a, "The service reported no available models.", "لم تبلغ الخدمة عن نماذج متاحة."); }
                if !models.is_empty() {
                    egui::ComboBox::from_id_salt(("local_model", images)).selected_text(&selection.model).show_ui(ui, |ui| {
                        for name in models { ui.selectable_value(&mut selection.model, name.clone(), name); }
                    });
                }
            } else {
                label(ui, a, "Connection has not been checked for this endpoint.", "لم يُفحص الاتصال بعنوان الخدمة هذا.");
            }
            let ready = !selection.model.trim().is_empty();
            let chosen = selection.clone();
            if ui.add_enabled(ready, egui::Button::new(rtl::widget(ui, text(a, if images { "Select for images" } else { "Select for text" }, if images { "اختيار لإنتاج الصور" } else { "اختيار لإنتاج النص" })))).clicked() {
                if images { s.settings.image_model = chosen; } else { s.settings.text_model = chosen; }
            }
            if images {
                label(ui, a, "Start your installed ComfyUI launcher, confirm its endpoint, then check connection. Configure API workflow in Providers and keys.", "شغّل ComfyUI بواسطة مشغّله المثبت وتحقق من عنوانه ثم افحص الاتصال. اضبط مخطط API في صفحة المزودين والمفاتيح.");
            } else {
                label(ui, a, "Start your installed Ollama application or its configured service, then check connection. Model installation is managed by Ollama.", "شغّل تطبيق Ollama المثبت أو خدمته المضبوطة ثم افحص الاتصال. يتم تثبيت النماذج بواسطة Ollama.");
            }
        }
        ui.separator();
        label(ui, a, "Resource profile", "ملف حدود الموارد");
        ui.horizontal(|ui| {
            if button(ui, a, "Conservative CPU (Ollama)", "معالج محافظ (Ollama)") {
                s.settings.resources = designcraft_ai_types::LocalResourcePolicy { cpu_threads: 2, context_tokens: 4096, batch_tokens: 32, use_gpu: false, min_free_ram_mb: 2048, max_memory_percent: 75, max_image_edge: 1024, keep_alive_seconds: 0 };
                s.settings.budget.max_output_tokens = s.settings.budget.max_output_tokens.min(2048);
                s.settings.pages_per_batch = if s.settings.pages_per_batch == 0 { 2 } else { s.settings.pages_per_batch.min(2) };
            }
            if button(ui, a, "Standard limits", "حدود قياسية") { s.settings.resources = designcraft_ai_types::LocalResourcePolicy::default(); }
        });
        label(ui, a, "Conservative preset: 4096 context tokens, up to 2048 output tokens and up to 2 pages per batch.", "الإعداد المحافظ: سياق 4096 رمزاً وإخراج حتى 2048 رمزاً وصفحتان كحد أقصى في كل دفعة.");
        let resource = &mut s.settings.resources;
        egui::Grid::new("ai_resources").num_columns(2).show(ui, |ui| {
            label(ui, a, "Ollama CPU threads", "خيوط المعالج لـOllama"); ui.add(egui::DragValue::new(&mut resource.cpu_threads).range(1..=64)); ui.end_row();
            label(ui, a, "Ollama context tokens", "رموز سياق Ollama"); ui.add(egui::DragValue::new(&mut resource.context_tokens).range(2048..=65_536)); ui.end_row();
            label(ui, a, "Ollama inference batch tokens", "رموز دفعة استدلال Ollama"); ui.add(egui::DragValue::new(&mut resource.batch_tokens).range(16..=512)); ui.end_row();
            label(ui, a, "Local minimum free RAM (MiB)", "أقل ذاكرة RAM حرة للخدمات المحلية (MiB)"); ui.add(egui::DragValue::new(&mut resource.min_free_ram_mb).range(512..=131_072)); ui.end_row();
            label(ui, a, "Local memory admission ceiling (%)", "سقف قبول الذاكرة للخدمات المحلية (%)"); ui.add(egui::DragValue::new(&mut resource.max_memory_percent).range(50..=95)); ui.end_row();
            label(ui, a, "Local maximum image edge (px)", "أقصى ضلع للصور المحلية (بكسل)"); ui.add(egui::DragValue::new(&mut resource.max_image_edge).range(256..=4096)); ui.end_row();
            label(ui, a, "Ollama keep model loaded (seconds)", "إبقاء نموذج Ollama محملاً (ثوانٍ)"); ui.add(egui::DragValue::new(&mut resource.keep_alive_seconds).range(0..=300)); ui.end_row();
        });
        ui.checkbox(&mut resource.use_gpu, rtl::widget(ui, text(a, "Allow GPU for Ollama", "السماح بالمعالج الرسومي لـOllama")));
        label(ui, a, "RAM admission and image bounds apply to local requests. ComfyUI selects CPU/GPU at service startup; Ollama controls do not limit its GPU memory.", "تُطبق حدود قبول RAM والصور على الطلبات المحلية. يختار ComfyUI المعالج أو GPU عند تشغيل خدمته؛ ولا تحد إعدادات Ollama من ذاكرته الرسومية.");
        ui.horizontal(|ui| {
            if ui.add_enabled(s.workflow_ready(), egui::Button::new(rtl::widget(ui, text(a, "Apply models and limits", "تطبيق النماذج والحدود")))).clicked() { requests.push(AiRequest::UpdateSettings(s.settings.clone())); }
            if button(ui, a, "Refresh hardware report", "تحديث تقرير الجهاز") { requests.push(AiRequest::RefreshResources); }
        });
    });
    ui.separator();
    label(ui, a, "Reported hardware", "الموارد المبلغ عنها");
    if let Some(resource) = &s.snapshot.resources {
        if resource.cpu_threads_available > 0 {
            rtl::label(ui, format!("{} {}", text(a, "Available CPU threads:", "خيوط المعالج المتاحة:"), resource.cpu_threads_available));
        }
        if resource.total_ram_mb > 0 {
            rtl::label(
                ui,
                format!("RAM: {} MiB / {} MiB {}", resource.available_ram_mb, resource.total_ram_mb, text(a, "free / total", "حرة / إجمالي")),
            );
        }
        for gpu in &resource.gpus {
            if gpu.total_vram_mb > 0 {
                rtl::label(
                    ui,
                    format!("{} · VRAM: {} MiB / {} MiB {}", gpu.name, gpu.free_vram_mb, gpu.total_vram_mb, text(a, "free / total", "حرة / إجمالي")),
                );
            } else {
                rtl::label(ui, format!("{} · {}", gpu.name, text(a, "VRAM telemetry unavailable", "بيانات ذاكرة المعالج الرسومي غير متاحة")));
            }
        }
        if resource.gpus.is_empty() {
            label(ui, a, "No GPU telemetry was reported.", "لم تُبلغ بيانات المعالج الرسومي.");
        }
        rtl::label(ui, &resource.message);
    } else {
        label(ui, a, "Hardware has not been probed. Refresh the hardware report.", "لم تُفحص موارد الجهاز. حدّث تقرير الجهاز.");
    }
}

fn production(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, document: bool, requests: &mut Vec<AiRequest>) {
    batch_review(ui, s, a, requests);
    ui.add_enabled_ui(!s.task_active(), |ui| {
        rtl::label(ui, format!("{} / {}", provider_name(s.settings.text_model.provider), s.settings.text_model.model));
        if button(ui, a, "Configure providers and keys", "إعداد المزودين والمفاتيح") {
            s.tab = 3;
        }
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Limits", "الحدود"))).show(ui, |ui| {
            budgets(ui, s, a);
        });
        ui.separator();
        label(ui, a, "Production instructions", "تعليمات الإنتاج");
        ui.add(egui::TextEdit::multiline(&mut s.prompt).desired_rows(5).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.new_document, false, rtl::widget(ui, text(a, "Current document", "المستند الحالي")));
            ui.radio_value(&mut s.new_document, true, rtl::widget(ui, text(a, "New A4 document", "مستند A4 جديد")));
            ui.add_enabled(s.new_document, egui::DragValue::new(&mut s.pages).range(1..=1000).suffix(" pages"));
        });
        ui.horizontal(|ui| {
            label(ui, a, "Pages per batch", "الصفحات في كل دفعة");
            ui.add_enabled(s.new_document, egui::DragValue::new(&mut s.settings.pages_per_batch).range(1..=32));
        });
        label(
            ui,
            a,
            "Each batch stops for your visual approval before another provider request.",
            "تتوقف كل دفعة لمراجعتك البصرية قبل أي طلب جديد إلى المزود.",
        );
        ui.checkbox(
            &mut s.automatic_skills,
            rtl::widget(ui, text(a, "Automatically select compatible approved skills", "اختيار المهارات المعتمدة والمتوافقة تلقائياً")),
        );
        if !s.automatic_skills {
            for card in s.snapshot.skills.iter().filter(|c| c.active && c.approved && c.enabled && c.compatible) {
                let mut selected = s.selected_skills.contains(&card.id);
                if ui.checkbox(&mut selected, rtl::widget(ui, &card.name)).changed() {
                    if selected {
                        s.selected_skills.insert(card.id.clone());
                    } else {
                        s.selected_skills.remove(&card.id);
                    }
                }
            }
        }
        field(ui, a, "Output/sample directory (optional for production)", "مجلد المخرجات والعينة (اختياري للإنتاج)", &mut s.directory);
        let valid = available
            && !s.prompt.trim().is_empty()
            && !s.settings.text_model.model.trim().is_empty()
            && (s.new_document || document)
            && s.workflow_ready()
            && (s.settings.budget.max_cost_usd.is_none() || s.rates_confirmed);
        if ui.add_enabled(valid, egui::Button::new(rtl::widget(ui, text(a, "Start production", "بدء الإنتاج")))).clicked() {
            requests.push(AiRequest::Start(TaskRequest {
                prompt: s.prompt.clone(),
                settings: s.settings.clone(),
                auto_select_skills: s.automatic_skills,
                skill_ids: if s.automatic_skills { Vec::new() } else { s.selected_skills.iter().cloned().collect() },
                target: if s.new_document { TaskTarget::NewA4 { pages: s.pages } } else { TaskTarget::CurrentDocument },
                export_directory: (!s.directory.trim().is_empty()).then(|| s.directory.clone()),
            }));
        }
        if ui
            .add_enabled(
                available && !s.directory.trim().is_empty(),
                egui::Button::new(rtl::widget(ui, text(a, "Create original 8-page science example", "إنشاء مثال علوم أصلي من ٨ صفحات"))),
            )
            .clicked()
        {
            requests.push(AiRequest::CreateScienceSample { directory: s.directory.clone() });
        }
        ui.separator();
        export_print(ui, s, a, available && document, requests);
    });
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

fn usage_line(ui: &mut egui::Ui, a: bool, usage: &designcraft_ai_types::protocol::UsageSummary, elapsed_ms: u64) {
    rtl::label(
        ui,
        format!(
            "{} {} · {} {} · {} {} · {} {}",
            text(a, "Time", "الوقت"),
            elapsed(elapsed_ms),
            text(a, "Requests", "الطلبات"),
            usage.requests,
            text(a, "Input", "الإدخال"),
            usage.input_tokens,
            text(a, "Output", "الإخراج"),
            usage.output_tokens
        ),
    );
    rtl::label(
        ui,
        format!(
            "{} {} · {} {} · {} {}",
            text(a, "Cached", "المخزن"),
            usage.cached_input_tokens,
            text(a, "Reasoning", "التفكير"),
            usage.reasoning_tokens,
            text(a, "Estimated tokens", "الرموز المقدرة"),
            usage.estimated_tokens
        ),
    );
    if let Some(cost) = usage.estimated_cost_usd {
        rtl::label(ui, format!("{} ${cost:.6}", text(a, "Estimated cost", "التكلفة التقديرية")));
    }
    if usage.has_unreported_usage {
        label(ui, a, "Some usage has not been reported by the provider.", "لم يبلغ المزود عن بعض بيانات الاستخدام.");
    }
}

fn batch_review(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
    let Some(batch) = &s.snapshot.batch else {
        return;
    };
    ui.separator();
    label(ui, a, "Batch production and proof", "إنتاج الدفعات والمراجعة");
    rtl::label(
        ui,
        format!(
            "{} {} / {} · {} {} / {} · {}",
            text(a, "Batch", "الدفعة"),
            batch.current_batch,
            batch.total_batches,
            text(a, "Pages", "الصفحات"),
            batch.completed_pages,
            batch.total_pages,
            batch.status
        ),
    );
    label(ui, a, "Cumulative task usage", "الاستخدام التراكمي للمهمة");
    usage_line(ui, a, &batch.cumulative_usage, batch.elapsed_ms);
    if let Some(current) = batch.batches.iter().find(|report| report.batch == batch.current_batch) {
        label(ui, a, "Current batch", "الدفعة الحالية");
        usage_line(ui, a, &current.usage, current.elapsed_ms);
        for path in &current.proof_paths {
            ui.horizontal(|ui| {
                let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
                ui.add(egui::Label::new(name).truncate()).on_hover_text(path);
                if path.to_ascii_lowercase().ends_with(".png") && button(ui, a, "Open proof preview", "فتح معاينة المراجعة") {
                    s.proof_path = Some(path.clone());
                }
            });
        }
        if let Some(texture) = &s.proof_texture {
            ui.add(egui::Image::new(texture).max_size(egui::vec2(620.0, 440.0)));
        }
        if !s.proof_error.is_empty() {
            rtl::label(ui, &s.proof_error);
        }
        if let Some(path) = &s.proof_path {
            ui.add(egui::Label::new(path).wrap());
        }
    } else {
        label(ui, a, "Current batch usage and proof have not yet been reported.", "لم تُبلغ بيانات استخدام الدفعة الحالية ومعاينتها بعد.");
    }
    if batch.status == "awaiting_human_proof" {
        label(
            ui,
            a,
            "Review every proof page. The next batch uses the same cumulative task limits.",
            "راجع كل صفحات المراجعة. تخضع الدفعة التالية لحدود المهمة التراكمية نفسها.",
        );
        let final_batch = batch.current_batch >= batch.total_batches;
        let (en, ar) = if final_batch {
            ("Approve final proof and finish", "اعتماد المراجعة النهائية وإنهاء المهمة")
        } else {
            ("Approve proof and generate next batch", "اعتماد المراجعة وإنشاء الدفعة التالية")
        };
        if ui.add_enabled(s.proof_texture.is_some() && s.proof_error.is_empty(), egui::Button::new(rtl::widget(ui, text(a, en, ar)))).clicked() {
            requests.push(AiRequest::ApproveBatch { job_id: batch.job_id.clone(), checkpoint: batch.checkpoint });
        }
    }
    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Batch details", "تفاصيل الدفعات"))).show(ui, |ui| {
        rtl::label(
            ui,
            format!(
                "{} / checkpoint {} / document {:?} / revision {:?}",
                batch.job_id, batch.checkpoint, batch.document_uid, batch.document_revision
            ),
        );
        for report in &batch.batches {
            rtl::label(
                ui,
                format!(
                    "{} {} · {} {}–{} · {}",
                    text(a, "Batch", "الدفعة"),
                    report.batch,
                    text(a, "Pages", "الصفحات"),
                    report.page_start.saturating_add(1),
                    report.page_start.saturating_add(report.page_count),
                    report.status
                ),
            );
            usage_line(ui, a, &report.usage, report.elapsed_ms);
            for path in &report.proof_paths {
                ui.add(egui::Label::new(path).wrap());
            }
        }
    });
}

fn budgets(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
    let b = &mut s.settings.budget;
    egui::Grid::new("ai_limits").num_columns(2).show(ui, |ui| {
        label(ui, a, "Total tokens", "مجموع الرموز");
        ui.add(egui::DragValue::new(&mut b.token_limit).range(1..=10_000_000));
        ui.end_row();
        label(ui, a, "Output tokens per request", "رموز الإخراج لكل طلب");
        ui.add(egui::DragValue::new(&mut b.max_output_tokens).range(1..=1_000_000));
        ui.end_row();
        label(ui, a, "Maximum requests", "أقصى عدد للطلبات");
        ui.add(egui::DragValue::new(&mut b.max_requests).range(1..=100));
        ui.end_row();
        label(ui, a, "Repair attempts", "محاولات الإصلاح");
        ui.add(egui::DragValue::new(&mut b.repair_attempts).range(0..=10));
        ui.end_row();
    });
    let mut cap = b.max_cost_usd.is_some();
    if ui.checkbox(&mut cap, rtl::widget(ui, text(a, "Estimated cost limit (USD)", "حد التكلفة التقديرية بالدولار"))).changed()
    {
        b.max_cost_usd = cap.then_some(1.0);
    }
    if let Some(cost) = &mut b.max_cost_usd {
        ui.add(egui::DragValue::new(cost).range(0.001..=10_000.0).speed(0.01));
        let rates = b.rates.get_or_insert(TokenRates {
            input_per_million: 0.0,
            cached_input_per_million: 0.0,
            output_per_million: 0.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: None,
        });
        label(
            ui,
            a,
            "Enter your actual rates per million tokens. Estimates are not invoices.",
            "أدخل الأسعار الفعلية لكل مليون رمز. التقديرات ليست فواتير.",
        );
        for (en, ar, rate) in [
            ("Input", "الإدخال", &mut rates.input_per_million),
            ("Cached input", "الإدخال المخزن", &mut rates.cached_input_per_million),
            ("Output", "الإخراج", &mut rates.output_per_million),
        ] {
            ui.horizontal(|ui| {
                label(ui, a, en, ar);
                ui.add(egui::DragValue::new(rate).range(0.0..=100_000.0).speed(0.01));
            });
        }
        optional_rate(ui, a, "Cache-write / million", "كتابة التخزين / مليون", &mut rates.cache_write_per_million);
        if s.settings.output_mode == OutputMode::NativeWithImages {
            ui.separator();
            label(ui, a, "Separate image provider rates", "أسعار مزود الصور المستقلة");
            let image = s.settings.image_rates.get_or_insert(TokenRates {
                input_per_million: 0.0,
                cached_input_per_million: 0.0,
                output_per_million: 0.0,
                cache_write_per_million: None,
                image_per_request: None,
                image_reserve_usd: None,
            });
            for (en, ar, rate) in [
                ("Image input / million", "إدخال الصور / مليون", &mut image.input_per_million),
                ("Image cached input / million", "إدخال الصور المخزن / مليون", &mut image.cached_input_per_million),
                ("Image output / million", "إخراج الصور / مليون", &mut image.output_per_million),
            ] {
                ui.horizontal(|ui| {
                    label(ui, a, en, ar);
                    ui.add(egui::DragValue::new(rate).range(0.0..=100_000.0).speed(0.01));
                });
            }
            optional_rate(ui, a, "Image cache-write / million", "كتابة تخزين الصور / مليون", &mut image.cache_write_per_million);
            optional_rate(ui, a, "Fixed price / image request", "سعر ثابت / طلب صورة", &mut image.image_per_request);
            if image.image_per_request.is_none() {
                optional_rate(ui, a, "Image request budget reserve (USD)", "احتياط كلفة طلب الصورة بالدولار", &mut image.image_reserve_usd);
                label(
                    ui,
                    a,
                    "Token pricing: reserve before sending, then calculate from reported usage.",
                    "تسعير بالتوكنز: يحجز الاحتياط قبل الإرسال ثم يحتسب من الاستهلاك المبلّغ.",
                );
            } else {
                label(
                    ui,
                    a,
                    "Image cost uses the fixed request price; reported tokens remain visible.",
                    "تحتسب كلفة الصور بالسعر الثابت للطلب، مع عرض التوكنز المبلّغة.",
                );
            }
        }
        ui.checkbox(
            &mut s.rates_confirmed,
            rtl::widget(ui, text(a, "I entered the rates to use for this estimate", "أؤكد إدخال الأسعار المستخدمة في هذا التقدير")),
        );
    }
}
fn optional_rate(ui: &mut egui::Ui, a: bool, en: &str, ar: &str, rate: &mut Option<f64>) {
    ui.horizontal(|ui| {
        let mut enabled = rate.is_some();
        if ui.checkbox(&mut enabled, rtl::widget(ui, text(a, en, ar))).changed() {
            *rate = enabled.then_some(0.0);
        }
        if let Some(value) = rate {
            ui.add(egui::DragValue::new(value).range(0.0..=100_000.0).speed(0.01));
        }
    });
}
fn export_print(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, enabled: bool, requests: &mut Vec<AiRequest>) {
    field(
        ui,
        a,
        "Export file path (the selected format sets its extension)",
        "مسار ملف التصدير (يُضبط الامتداد حسب الصيغة المختارة)",
        &mut s.export_path,
    );
    ui.horizontal(|ui| {
        for (pdf, en, ar) in [(false, "Export native", "تصدير أصلي"), (true, "Export PDF", "تصدير PDF")] {
            if ui.add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, en, ar)))).clicked() {
                s.export_path =
                    std::path::Path::new(&s.export_path).with_extension(if pdf { "pdf" } else { "designcraft" }).to_string_lossy().into_owned();
                requests.push(AiRequest::ExportDocument { path: s.export_path.clone(), pdf });
            }
        }
        if ui
            .add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export IDML", "تصدير IDML"))))
            .clicked()
        {
            s.export_path = std::path::Path::new(&s.export_path).with_extension("idml").to_string_lossy().into_owned();
            requests.push(AiRequest::ExportIdml { path: s.export_path.clone() });
        }
        if ui
            .add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export INDD", "تصدير INDD"))))
            .clicked()
        {
            s.export_path = std::path::Path::new(&s.export_path).with_extension("indd").to_string_lossy().into_owned();
            requests.push(AiRequest::ExportIndd { path: s.export_path.clone() });
        }
    });
    label(
        ui,
        a,
        "INDD requires installed InDesign. Linked raster assets are currently unsupported for this conversion.",
        "يتطلب INDD تثبيت InDesign. لا تدعم عملية التحويل هذه الصور النقطية المرتبطة حالياً.",
    );
    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Print — actual size (100%)", "طباعة بالحجم الفعلي (١٠٠٪)"))).show(ui, |ui| {
        field(ui, a, "Printer (empty = system default)", "الطابعة (فارغ = الافتراضية)", &mut s.printer);
        ui.checkbox(&mut s.grayscale, rtl::widget(ui, text(a, "Grayscale", "تدرج رمادي")));
        egui::ComboBox::from_id_salt("ai_duplex").selected_text(&s.duplex).show_ui(ui, |ui| {
            for (v, en, ar) in [
                ("off", "One side", "وجه واحد"),
                ("longEdge", "Duplex long edge", "وجهان: الحافة الطويلة"),
                ("shortEdge", "Duplex short edge", "وجهان: الحافة القصيرة"),
            ] {
                ui.selectable_value(&mut s.duplex, v.into(), rtl::widget(ui, text(a, en, ar)));
            }
        });
        if ui.add_enabled(enabled, egui::Button::new(rtl::widget(ui, text(a, "Prepare frozen print proof", "إعداد بروفة الطباعة")))).clicked() {
            requests.push(AiRequest::PreparePrint { printer: (!s.printer.trim().is_empty()).then(|| s.printer.clone()), grayscale: s.grayscale, duplex: s.duplex.clone() });
        }
        if let Some(proof) = s.snapshot.print_proof.clone() {
            ui.separator();
            rtl::label(ui, format!("{} {} · {} {}", text(a, "Pages", "الصفحات"), proof.pages, text(a, "PDF bytes", "حجم PDF بالبايت"), proof.bytes));
            ui.add(egui::Label::new(format!("SHA-256: {}", proof.sha256)).wrap());
            if let Some(path) = &proof.pdf_path { ui.add(egui::Label::new(path).wrap()); }
            if button(ui, a, "Open complete PDF proof", "فتح PDF البروفة كاملاً") { requests.push(AiRequest::OpenPrintProof { token: proof.token.clone() }); }
            for (key, en, ar) in [("fonts", "Fonts", "الخطوط"), ("color", "Color settings", "إعدادات الألوان"), ("job", "Frozen printer queue and job settings", "الطابعة وإعدادات مهمة البروفة")] {
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, en, ar))).default_open(key == "job").show(ui, |ui| {
                    if let Some(value) = proof.manifest.get(key) { ui.add(egui::Label::new(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())).wrap()); }
                });
            }
            label(ui, a, "Changing options requires a new proof. Submission uses this frozen PDF and job; printer/RIP settings can affect physical scaling and color.", "يتطلب تغيير الخيارات إعداد مراجعة جديدة. تُرسل نسخة PDF والمهمة المجمدتان؛ وقد تؤثر إعدادات الطابعة أو RIP في قياس الورق واللون.");
            field(ui, a, "Save exact proof PDF path", "مسار حفظ PDF المطابق للبروفة", &mut s.print_save_path);
            if ui.add_enabled(!s.print_save_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Save frozen PDF", "حفظ PDF البروفة نفسه")))).clicked() {
                s.print_save_path = std::path::Path::new(&s.print_save_path).with_extension("pdf").to_string_lossy().into_owned();
                requests.push(AiRequest::SavePreparedPrint { token: proof.token.clone(), path: s.print_save_path.clone() });
            }
            ui.checkbox(&mut s.print_reviewed, rtl::widget(ui, text(a, "I reviewed this PDF, fonts, colors and printer queue", "راجعت ملف PDF والخطوط والألوان والطابعة لهذه البروفة")));
            if ui.add_enabled(enabled && s.print_reviewed, egui::Button::new(rtl::widget(ui, text(a, "Submit this frozen job to printer", "إرسال البروفة نفسها إلى الطابعة")))).clicked() {
                requests.push(AiRequest::SubmitPreparedPrint { token: proof.token, request_id: format!("ui-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)) });
                s.print_reviewed = false;
            }
        }

    });
}

fn skills(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, document: bool, requests: &mut Vec<AiRequest>) {
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        field(ui, a, "Import skill folder or ZIP path", "مسار مجلد المهارة أو ملف ZIP", &mut s.import_path);
        if ui.add_enabled(!s.import_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Import draft", "استيراد مسودة")))).clicked()
        {
            requests.push(AiRequest::ImportSkill { path: s.import_path.clone() });
        }
        ui.separator();
        let name_width = (ui.available_width() - 449.0).max(48.0);
        let version_width = 90.0;
        let status_width = 95.0;
        ui.horizontal(|ui| {
            for (en, ar, width) in [
                ("Name", "الاسم", name_width),
                ("Version", "الإصدار", version_width),
                ("Status", "الحالة", status_width),
                ("Actions", "الإجراءات", 240.0),
            ] {
                ui.add_sized([width, 20.0], egui::Label::new(rtl::widget(ui, text(a, en, ar))).truncate());
            }
        });
        for c in &s.snapshot.skills {
            ui.push_id((&c.id, &c.revision), |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([name_width, 20.0], egui::Label::new(rtl::widget(ui, &c.name)).truncate()).on_hover_text(&c.name);
                    let revision: String = c.revision.chars().take(12).collect();
                    ui.add_sized([version_width, 20.0], egui::Label::new(revision).truncate()).on_hover_text(&c.revision);
                    let mut status = text(a, if c.approved { "Approved" } else { "Draft" }, if c.approved { "معتمدة" } else { "مسودة" }).to_owned();
                    if c.active {
                        status.push_str(text(a, " / Active", " / نشطة"));
                    }
                    if !c.compatible {
                        status.push_str(text(a, " / Incompatible", " / غير متوافقة"));
                    }
                    ui.add_sized([status_width, 20.0], egui::Label::new(rtl::widget(ui, &status)).truncate()).on_hover_text(&status);
                    if button(ui, a, "Inspect", "مراجعة") {
                        requests.push(AiRequest::InspectSkill { id: c.id.clone(), revision: c.revision.clone() });
                    }
                    let mut enabled = c.enabled && c.approved && c.compatible;
                    if ui
                        .add_enabled(c.approved && c.compatible, egui::Checkbox::new(&mut enabled, rtl::widget(ui, text(a, "Enabled", "مفعلة"))))
                        .changed()
                    {
                        requests.push(AiRequest::SetSkillEnabled { id: c.id.clone(), enabled });
                    }
                    if ui
                        .add_enabled(c.approved && !c.active, egui::Button::new(rtl::widget(ui, text(a, "Restore revision", "استعادة الإصدار"))))
                        .clicked()
                    {
                        requests.push(AiRequest::RollbackSkill { id: c.id.clone(), revision: c.revision.clone() });
                    }
                });
                ui.scope(|ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    rtl::label(ui, &c.description);
                });
                ui.separator();
            });
        }
        if let Some(r) = s.snapshot.review.clone() {
            ui.separator();
            rtl::label(ui, format!("{} / {}", r.id, r.revision));
            label(ui, a, "SKILL.md — edits create a new draft", "SKILL.md — التعديل ينشئ مسودة جديدة");
            ui.add(egui::TextEdit::multiline(&mut s.skill_md).desired_rows(8).desired_width(f32::INFINITY));
            label(ui, a, "Manifest JSON", "ملف الوصف JSON");
            ui.add(egui::TextEdit::multiline(&mut s.manifest_json).code_editor().desired_rows(5).desired_width(f32::INFINITY));
            let clean = s.skill_md == r.skill_md && s.manifest_json == r.manifest_json;
            ui.horizontal_wrapped(|ui| {
                if button(ui, a, "Save as draft", "حفظ كمسودة") {
                    requests.push(AiRequest::SaveSkillDraft {
                        id: r.id.clone(),
                        skill_md: s.skill_md.clone(),
                        manifest_json: s.manifest_json.clone(),
                    });
                }
                if ui.add_enabled(clean, egui::Button::new(rtl::widget(ui, text(a, "Run sample", "تشغيل العينة")))).clicked() {
                    requests.push(AiRequest::RunSkillSample { id: r.id.clone(), revision: r.revision.clone() });
                }
                if clean && r.sample_valid && button(ui, a, "Approve this revision", "اعتماد هذا الإصدار") {
                    requests.push(AiRequest::ApproveSkill { id: r.id.clone(), revision: r.revision.clone() });
                }
            });
            label(
                ui,
                a,
                if r.sample_valid {
                    "Sample passed; inspect it before explicit approval."
                } else {
                    "Approval requires a valid sample of this saved revision."
                },
                if r.sample_valid {
                    "نجحت العينة؛ افحصها قبل الاعتماد الصريح."
                } else {
                    "يتطلب الاعتماد عينة ناجحة لهذا الإصدار المحفوظ."
                },
            );
            if let Some(path) = &r.sample_path {
                rtl::label(ui, path);
            }
            if let Some(texture) = &s.preview {
                ui.add(egui::Image::new(texture).max_size(egui::vec2(600.0, 420.0)));
            }
            if !s.preview_error.is_empty() {
                rtl::label(ui, &s.preview_error);
            }
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Files", "الملفات"))).show(ui, |ui| {
                for f in &r.files {
                    rtl::label(ui, f);
                }
            });
            field(ui, a, "Export skill ZIP path", "مسار تصدير المهارة ZIP", &mut s.export_path);
            if ui
                .add_enabled(!s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export skill", "تصدير المهارة"))))
                .clicked()
            {
                requests.push(AiRequest::ExportSkill { id: r.id, revision: r.revision, path: s.export_path.clone() });
            }
        }
        ui.separator();
        label(ui, a, "Generate a reviewable skill draft", "إنشاء مسودة مهارة للمراجعة");
        ui.add(egui::TextEdit::multiline(&mut s.skill_prompt).desired_rows(3).desired_width(f32::INFINITY));
        label(ui, a, "Reference paths (one per line)", "مسارات المراجع (مسار في كل سطر)");
        ui.add(egui::TextEdit::multiline(&mut s.references).desired_rows(3).desired_width(f32::INFINITY));
        ui.horizontal_wrapped(|ui| {
            let ready = !s.skill_prompt.trim().is_empty()
                && !s.settings.text_model.model.trim().is_empty()
                && s.workflow_ready()
                && (s.settings.budget.max_cost_usd.is_none() || s.rates_confirmed);
            if ui.add_enabled(ready, egui::Button::new(rtl::widget(ui, text(a, "Generate draft", "إنشاء المسودة")))).clicked() {
                requests.push(AiRequest::GenerateSkill {
                    prompt: s.skill_prompt.clone(),
                    reference_paths: s.references.lines().map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect(),
                    settings: s.settings.clone(),
                });
            }
            if ui
                .add_enabled(
                    ready && document,
                    egui::Button::new(rtl::widget(ui, text(a, "Learn from current corrected document", "إنشاء مهارة من المستند المصحح الحالي"))),
                )
                .clicked()
            {
                requests.push(AiRequest::GenerateSkillFromDocument { prompt: s.skill_prompt.clone(), settings: s.settings.clone() });
            }
        });
    });
}

fn usage(ui: &mut egui::Ui, s: &AiUiState, a: bool) {
    label(
        ui,
        a,
        "Provider-reported usage and explicit-rate estimates for this session. Estimates may differ from billing.",
        "الاستخدام المبلغ من المزود وتقدير الأسعار المدخلة لهذه الجلسة. قد يختلف التقدير عن الفاتورة.",
    );
    for r in s.snapshot.reports.iter().rev() {
        egui::CollapsingHeader::new(format!("{} · {} · {}", r.title, r.status, r.id)).default_open(true).show(ui, |ui| {
            rtl::label(ui, format!("{} / {} · {} ms · {} actions", provider_name(r.provider), r.model, r.elapsed_ms, r.actions));
            let u = &r.usage;
            rtl::label(
                ui,
                format!(
                    "Requests: {} | Input: {} | Output: {} | Cached: {} | Reasoning: {} | Estimated: {}",
                    u.requests, u.input_tokens, u.output_tokens, u.cached_input_tokens, u.reasoning_tokens, u.estimated_tokens
                ),
            );
            if let Some(cost) = u.estimated_cost_usd {
                rtl::label(ui, format!("Estimated USD: {cost:.6}"));
            }
            if u.has_unreported_usage {
                label(ui, a, "Some provider usage is unreported.", "بعض بيانات الاستخدام لم يبلغها المزود.");
            }
            rtl::label(ui, &r.message);
            for (id, rev) in &r.skills {
                rtl::label(ui, format!("{id} / {rev}"));
            }
            for path in &r.artifacts {
                rtl::label(ui, path);
            }
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Details", "التفاصيل"))).show(ui, |ui| {
                for issue in &r.issues {
                    rtl::label(ui, issue.to_string());
                }
            });
        });
    }
}

fn preview_texture(read: &crate::ReadFn, path: &str, ctx: &egui::Context, name: &str) -> Result<egui::TextureHandle, String> {
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

fn load_preview(app: &mut DesignApp, ctx: &egui::Context) {
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

fn load_proof_preview(app: &mut DesignApp, ctx: &egui::Context) {
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
        return;
    };
    match preview_texture(read, &path, ctx, "ai_batch_proof") {
        Ok(texture) => s.proof_texture = Some(texture),
        Err(e) => s.proof_error = e,
    }
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use designcraft_engine::Session;
    use serde_json::json;
    #[test]
    fn callback_dispatch_and_manual_settings_survive_poll() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.run("ai.production", json!({})).unwrap();
        app.ui.ai.settings.text_model.model = "manual-model".into();
        app.run("ai.request", json!({"method":"poll"})).unwrap();
        assert_eq!(app.ui.ai.settings.text_model.model, "manual-model");
        assert!(app.ui.ai.open);
    }
    #[test]
    fn session_key_drafts_and_prompts_never_serialize() {
        let mut ui = crate::UiState::default();
        ui.ai.api_key.0 = "secret-fixture".into();
        ui.ai.prompt = "private-prompt".into();
        let serialized = serde_json::to_string(&ui).unwrap();
        assert!(!serialized.contains("secret-fixture"));
        assert!(!serialized.contains("private-prompt"));
        assert!(!format!("{:?}", ui.ai).contains("secret-fixture"));
    }
    #[test]
    fn approval_requires_valid_review_and_host_absence_is_an_error() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"x", "revision":"1"})).is_err());
        assert!(dispatch(&mut app, AiRequest::Poll).is_err());
    }

    #[test]
    fn partial_workflow_draft_survives_poll_and_becomes_runnable_when_completed() {
        let mut state = AiUiState::default();
        state.accept(AiSnapshot { available: true, ..Default::default() });
        state.settings.output_mode = OutputMode::NativeWithImages;
        state.settings.comfy_workflow = Some(json!({"stale": true}));
        state.workflow_json = r#"{"node":{"text":"{{prompt}}""#.into();
        state.edit_workflow();
        let partial = state.workflow_json.clone();
        assert!(!state.workflow_error.is_empty());
        assert!(state.settings.comfy_workflow.is_none());
        assert!(!state.workflow_ready());
        state.accept(AiSnapshot { available: true, ..Default::default() });
        assert_eq!(state.workflow_json, partial);
        assert!(!state.workflow_error.is_empty());
        state.workflow_json.push_str(r#", "model":"{{model}}"}}"#);
        state.edit_workflow();
        assert!(state.workflow_error.is_empty());
        assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
        assert!(state.workflow_ready());
        state.accept(AiSnapshot { available: true, ..Default::default() });
        assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
    }

    #[test]
    fn automation_cannot_approve_even_a_valid_review_or_call_the_host() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let host_calls = calls.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                ai: Some(Box::new(move |_, _| {
                    host_calls.set(host_calls.get() + 1);
                    AiSnapshot { available: true, ..Default::default() }
                })),
                ..Default::default()
            },
        );
        app.ui.ai.snapshot.available = true;
        app.ui.ai.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
            id: "fixture".into(),
            revision: "1".into(),
            skill_md: "reviewed".into(),
            manifest_json: "{}".into(),
            sample_path: Some("sample.png".into()),
            sample_valid: true,
            files: vec![],
        });
        assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"fixture", "revision":"1"})).is_err());
        app.ui.ai.snapshot.batch = Some(designcraft_ai_types::protocol::BatchSnapshot {
            job_id: "batch-fixture".into(),
            status: "awaiting_human_proof".into(),
            checkpoint: 7,
            ..Default::default()
        });
        assert!(app.run("ai.request", json!({"method":"approve_batch", "job_id":"batch-fixture", "checkpoint":7})).is_err());
        assert!(
            app.run("ai.request", json!({"method":"remove_api_key", "selection":{"provider":"open_ai","model":"fixture","endpoint":null}})).is_err()
        );
        assert!(app.run("ai.request", json!({"method":"refresh_credentials"})).is_err());
        app.ui.ai.snapshot.print_proof = Some(designcraft_ai_types::protocol::PrintProof {
            token: "frozen-fixture".into(),
            sha256: "fixture-hash".into(),
            bytes: 64,
            pages: 1,
            manifest: json!({}),
            pdf_path: Some("fixture.pdf".into()),
        });
        assert!(app.run("ai.request", json!({"method":"submit_prepared_print", "token":"frozen-fixture", "request_id":"fixture-print"})).is_err());
        assert!(app.run("ai.request", json!({"method":"open_print_proof", "token":"frozen-fixture"})).is_err());
        assert!(app.run("ai.request", json!({"method":"start_local_ollama", "executable":"C:/fixture/ollama.exe"})).is_err());
        assert!(app.run("ai.request", json!({"method":"stop_local_ollama"})).is_err());
        assert_eq!(calls.get(), 0);
    }
}
