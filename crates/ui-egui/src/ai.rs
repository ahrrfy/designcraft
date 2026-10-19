//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{AiRequest, AiSettings, AiSnapshot, BatchSnapshot, TaskRequest, TaskTarget, TokenRates};
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

/// Human review belongs to one complete, immutable proof manifest and document revision.
#[derive(Clone, PartialEq, Eq)]
struct BatchReviewKey {
    job_id: String,
    checkpoint: u32,
    batch: u32,
    document: (u64, u64),
    page_range: (u32, u32),
    task_shape: (u32, u32, u32),
    proofs: Vec<(String, String)>,
}
impl BatchReviewKey {
    fn from_batch(batch: &BatchSnapshot) -> Option<Self> {
        if batch.status != "awaiting_human_proof" {
            return None;
        }
        let mut reports = batch.batches.iter().filter(|report| report.batch == batch.current_batch);
        let current = reports.next()?;
        if reports.next().is_some() {
            return None;
        }
        if current.proof_paths.len() != current.proof_sha256.len() || current.proof_paths.len() > 34 || !(1..=32).contains(&current.page_count) {
            return None;
        }
        let mut pages = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for (path, sha) in current.proof_paths.iter().zip(&current.proof_sha256) {
            if path.is_empty()
                || !paths.insert(path.replace('\\', "/").to_ascii_lowercase())
                || sha.len() != 64
                || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            if path.to_ascii_lowercase().ends_with(".png") && !pages.insert(path) {
                return None;
            }
        }
        if u32::try_from(pages.len()).ok() != Some(current.page_count) {
            return None;
        }
        Some(Self {
            job_id: batch.job_id.clone(),
            checkpoint: batch.checkpoint,
            batch: batch.current_batch,
            document: (batch.document_uid?, batch.document_revision?),
            page_range: (current.page_start, current.page_count),
            task_shape: (batch.total_pages, batch.pages_per_batch, batch.total_batches),
            proofs: current.proof_paths.iter().cloned().zip(current.proof_sha256.iter().cloned()).collect(),
        })
    }

    fn pages(&self) -> impl Iterator<Item = &(String, String)> {
        self.proofs.iter().filter(|(path, _)| path.to_ascii_lowercase().ends_with(".png"))
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
    continue_book: bool,
    pages: u32,
    automatic_skills: bool,
    selected_skills: BTreeSet<String>,
    directory: String,
    import_path: String,
    export_path: String,
    skill_prompt: String,
    build_design_style: bool,
    correction_before: String,
    correction_after: String,
    correction_reason: String,
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
    proof_key: Option<BatchReviewKey>,
    reviewed_proofs: BTreeSet<(String, String)>,
    proof_document_version: Option<(u64, u64)>,
    proof_document_matches: bool,
    review_input_is_human: bool,
    review_pointer_press: Option<(BatchReviewKey, egui::Id)>,
    batch_approval_gesture: Option<(BatchReviewKey, egui::Context, u64)>,
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
            continue_book: false,
            pages: 8,
            automatic_skills: true,
            selected_skills: BTreeSet::new(),
            directory: String::new(),
            import_path: String::new(),
            export_path: String::new(),
            skill_prompt: String::new(),
            build_design_style: true,
            correction_before: String::new(),
            correction_after: String::new(),
            correction_reason: String::new(),
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
            reviewed_proofs: BTreeSet::new(),
            proof_document_version: None,
            proof_document_matches: false,
            review_input_is_human: false,
            review_pointer_press: None,
            batch_approval_gesture: None,
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
            // Reuse explicitly configured prices; restarting does not require the same setup again.
            self.rates_confirmed =
                self.settings.budget.rates.as_ref().is_some_and(|rates| rates.input_per_million > 0.0 || rates.output_per_million > 0.0);
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
        let proof_key = snapshot.batch.as_ref().and_then(BatchReviewKey::from_batch);
        if self.proof_key != proof_key {
            self.review_pointer_press = None;
            self.batch_approval_gesture = None;
            self.proof_path = proof_key.as_ref().and_then(|key| key.pages().next()).map(|(path, _)| path.clone());
            self.reviewed_proofs.clear();
            self.proof_document_matches = false;
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

    fn sync_proof_document(&mut self, document: Option<(u64, u64)>) {
        if self.proof_document_version != document {
            self.reviewed_proofs.clear();
            self.proof_document_version = document;
        }
        self.proof_document_matches = self.proof_key.as_ref().is_some_and(|key| Some(key.document) == document);
    }

    fn can_review_selected_proof(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_texture.is_some()
            && self.proof_error.is_empty()
            && self.proof_loaded == self.proof_path
            && self.proof_path.as_ref().is_some_and(|path| self.proof_key.as_ref().is_some_and(|key| key.pages().any(|(page, _)| page == path)))
    }

    /// Only an explicit per-page UI gesture records review; preview loading never does.
    fn review_selected_proof(&mut self) {
        if !self.can_review_selected_proof() {
            return;
        }
        if let Some(page) = self.proof_key.as_ref().and_then(|key| key.pages().find(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))) {
            self.reviewed_proofs.insert(page.clone());
        }
    }

    fn can_approve_batch(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_error.is_empty()
            && self.proof_key.as_ref().is_some_and(|key| key.pages().all(|page| self.reviewed_proofs.contains(page)))
    }

    /// A clicked response alone may come from accessibility or stale widget state.
    /// Bind a primary press/release (or non-repeated focused key press) to this proof.
    fn fresh_review_gesture(
        human: bool,
        key: Option<&BatchReviewKey>,
        pointer_press: &mut Option<(BatchReviewKey, egui::Id)>,
        response: &egui::Response,
    ) -> bool {
        if !human {
            *pointer_press = None;
            return false;
        }
        let Some(key) = key else { return false };
        let pointer_down = response.is_pointer_button_down_on();
        let pointer_clicked = response.clicked_by(egui::PointerButton::Primary);
        let keyboard_clicked = response.clicked() && response.has_focus();
        response.ctx.input(|input| {
            let mut pointer = false;
            for event in &input.events {
                if let egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. } = event {
                    if *pressed {
                        // A coalesced press+release has no held-button state at frame end.
                        // Require egui to attribute the interaction to this widget as well.
                        if response.rect.contains(*pos) && (pointer_down || pointer_clicked) {
                            *pointer_press = Some((key.clone(), response.id));
                        } else if pointer_press.as_ref().is_some_and(|(_, id)| *id == response.id) {
                            *pointer_press = None;
                        }
                    } else if pointer_press.as_ref().is_some_and(|(_, id)| *id == response.id) {
                        pointer = pointer_clicked
                            && response.rect.contains(*pos)
                            && pointer_press.as_ref().is_some_and(|(pressed_key, _)| pressed_key == key);
                        *pointer_press = None;
                    }
                }
            }
            let keyboard = keyboard_clicked
                && input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { key: egui::Key::Enter | egui::Key::Space, pressed: true, repeat: false, .. }));
            pointer || keyboard
        })
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

    fn production_issue(&self, available: bool, document: bool, arabic: bool) -> Option<&'static str> {
        let (en, ar) = if !available {
            ("Native AI host is unavailable. Open the desktop application.", "خدمة الذكاء غير متاحة. افتح تطبيق سطح المكتب.")
        } else if self.snapshot.busy {
            ("A task is running. Wait for it to finish or cancel it.", "توجد مهمة قيد التشغيل. انتظر انتهاءها أو ألغها.")
        } else if self.task_active() {
            (
                "Review and approve the current batch, or cancel the task before starting another.",
                "راجع الدفعة الحالية واعتمدها، أو ألغ المهمة قبل بدء مهمة أخرى.",
            )
        } else if self.prompt.trim().is_empty() {
            ("Enter production instructions.", "أدخل تعليمات الإنتاج.")
        } else if self.settings.text_model.model.trim().is_empty() {
            ("Choose a text model above.", "اختر نموذج النص من الحقل أعلاه.")
        } else if !self.new_document && !document {
            ("Open a document or choose New A4 document.", "افتح مستنداً أو اختر مستند A4 جديداً.")
        } else if self.settings.output_mode == OutputMode::NativeWithImages
            && self.settings.image_model.provider != ProviderKind::ComfyUi
            && self.settings.image_model.model.trim().is_empty()
        {
            ("Choose an image model in Providers and keys.", "اختر نموذج الصور من تبويب المزودين والمفاتيح.")
        } else if !self.workflow_ready() {
            ("Complete a valid ComfyUI API workflow in Providers and keys.", "أكمل مخطط ComfyUI API صالحاً في تبويب المزودين والمفاتيح.")
        } else if self.settings.budget.max_cost_usd.is_some() && !self.rates_confirmed {
            ("Set and confirm provider prices in Advanced settings.", "أدخل أسعار المزود وأكّدها من الإعدادات المتقدمة.")
        } else {
            return None;
        };
        Some(text(arabic, en, ar))
    }
}

/// Typed UI path shared by all panel buttons. Credentials move directly to the host without JSON,
/// logs, serialization, document metadata or preference storage.
pub fn dispatch(app: &mut DesignApp, request: AiRequest) -> Result<Value, String> {
    if !human_review_input(app)
        && matches!(
            &request,
            AiRequest::ApproveSkill { .. }
                | AiRequest::ApproveBatch { .. }
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
    let snapshot = host(&mut app.session, request);
    let result = serde_json::to_value(&snapshot).map_err(|e| e.to_string());
    app.ui.ai.accept(snapshot);
    result
}

fn human_review_input(app: &DesignApp) -> bool {
    !app.in_control && !app.control_input_frame.load(std::sync::atomic::Ordering::Relaxed)
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
        "prepare_skills" => AiRequest::PrepareSkills { prompt: field("prompt")? },
        "save_skill_draft" => AiRequest::SaveSkillDraft { id: field("id")?, skill_md: field("skill_md")?, manifest_json: field("manifest_json")? },
        "add_design_correction" => AiRequest::AddDesignCorrection {
            id: field("id")?,
            revision: field("revision")?,
            correction: serde_json::from_value(decode("correction")?).map_err(|e| e.to_string())?,
        },
        "generate_skill" => AiRequest::GenerateSkill {
            design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
            prompt: field("prompt")?,
            reference_paths: serde_json::from_value(decode("reference_paths")?).map_err(|e| e.to_string())?,
            settings: serde_json::from_value(decode("settings")?).map_err(|e| e.to_string())?,
        },
        "generate_skill_from_document" => AiRequest::GenerateSkillFromDocument {
            design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
            prompt: field("prompt")?,
            settings: serde_json::from_value(decode("settings")?).map_err(|e| e.to_string())?,
        },
        "run_skill_sample" => AiRequest::RunSkillSample { id: field("id")?, revision: field("revision")? },
        "run_design_template" => AiRequest::RunDesignTemplate { id: field("id")?, revision: field("revision")?, template: field("template")? },
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
    state.batch_approval_gesture = None;
    state.review_input_is_human = human_review_input(app);
    state.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
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
    if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        state.review_pointer_press = None;
    }
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
    app.ui.ai.batch_approval_gesture = None;
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
                label(
                    ui,
                    a,
                    if credential.protected_storage_available { "OS-protected storage available" } else { "OS-protected storage unavailable" },
                    if credential.protected_storage_available {
                        "مخزن حماية نظام التشغيل متاح"
                    } else {
                        "مخزن حماية نظام التشغيل غير متاح"
                    },
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
    ui.add_enabled_ui(!s.task_active(), |ui| {
        ui.horizontal_wrapped(|ui| {
            provider(ui, "production_text_provider", &mut s.settings.text_model.provider, false);
            label(ui, a, "Model or agent", "النموذج أو الوكيل");
            ui.add(egui::TextEdit::singleline(&mut s.settings.text_model.model).desired_width(230.0));
            if button(ui, a, "Discover", "اكتشاف") {
                s.pending_models = Some(s.settings.text_model.clone());
                requests.push(AiRequest::ListModels { selection: s.settings.text_model.clone() });
            }
            if button(ui, a, "Keys and providers", "المفاتيح والمزودون") {
                s.tab = 3;
            }
        });
        if !s.snapshot.models.is_empty() && s.pending_models.as_ref() == Some(&s.settings.text_model) {
            egui::ComboBox::from_id_salt("production_discovered_models").selected_text(text(a, "Available models", "النماذج المتاحة")).show_ui(
                ui,
                |ui| {
                    for name in &s.snapshot.models {
                        ui.selectable_value(&mut s.settings.text_model.model, name.clone(), name);
                    }
                },
            );
        }
        label(ui, a, "What would you like to create?", "ما الذي تريد إنتاجه؟");
        ui.horizontal_wrapped(|ui| {
            label(ui, a, "Design style", "نمط التصميم");
            let selected = s
                .settings
                .design_style
                .as_ref()
                .and_then(|id| s.snapshot.skills.iter().rev().find(|card| &card.id == id))
                .and_then(|card| card.design_profile.as_ref())
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| {
                    s.settings.design_style.clone().unwrap_or_else(|| text(a, "Automatic from the request", "تلقائي حسب المطلوب").into())
                });
            egui::ComboBox::from_id_salt("production_design_style").selected_text(selected).width(260.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut s.settings.design_style, None, rtl::widget(ui, text(a, "Automatic from the request", "تلقائي حسب المطلوب")));
                let mut seen = BTreeSet::new();
                for card in s.snapshot.skills.iter().rev().filter(|card| card.enabled && card.compatible) {
                    if let Some(profile) = &card.design_profile
                        && seen.insert(&card.id)
                    {
                        ui.selectable_value(&mut s.settings.design_style, Some(card.id.clone()), rtl::widget(ui, &profile.name));
                    }
                }
            });
            if button(ui, a, "Style library", "مكتبة الأنماط") {
                s.tab = 1;
            }
        });
        // Long pasted briefs scroll inside the composer instead of hiding the production button.
        egui::ScrollArea::vertical().id_salt("ai_prompt_composer").max_height(180.0).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut s.prompt).desired_rows(7).desired_width(f32::INFINITY).hint_text(text(
                a,
                "Describe your design, audience and desired result...",
                "اكتب التصميم والفئة العمرية والنتيجة المطلوبة…",
            )));
        });
        ui.horizontal_wrapped(|ui| {
            if ui.radio(!s.new_document && !s.continue_book, rtl::widget(ui, text(a, "Current document", "المستند الحالي"))).clicked() {
                s.new_document = false;
                s.continue_book = false;
            }
            if ui.radio(s.new_document, rtl::widget(ui, text(a, "New A4 document", "مستند A4 جديد"))).clicked() {
                s.new_document = true;
                s.continue_book = false;
            }
            if ui.radio(s.continue_book, rtl::widget(ui, text(a, "Continue remaining book pages", "استكمال صفحات الكتاب المتبقية"))).clicked()
            {
                s.new_document = false;
                s.continue_book = true;
            }
            if s.new_document {
                ui.add(egui::DragValue::new(&mut s.pages).range(1..=1000).suffix(" pages"));
            }
            label(ui, a, "Pages per batch", "الصفحات في كل دفعة");
            ui.add_enabled(s.new_document || s.continue_book, egui::DragValue::new(&mut s.settings.pages_per_batch).range(1..=32));
        });
        if s.automatic_skills {
            label(ui, a, "Relevant skills are selected, checked and activated automatically.", "تُختار المهارات المناسبة وتُفحص وتُفعّل تلقائياً.");
        }
        if !s.snapshot.skill_warnings.is_empty() {
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Skipped skill drafts — details", "مسودات مهارات مستبعدة — التفاصيل"))).show(
                ui,
                |ui| {
                    for warning in &s.snapshot.skill_warnings {
                        rtl::label(ui, warning);
                    }
                },
            );
        }
        label(
            ui,
            a,
            "Template styles use the model for content; native drawing and formatting commands are assembled locally.",
            "أنماط القوالب تستخدم النموذج للمحتوى؛ أوامر الرسم والتنسيق تُركّب وتُنفّذ محلياً بالمحرك.",
        );
        if let Some(cost) = s.settings.budget.max_cost_usd {
            rtl::label(ui, format!("{} ${cost:.2}", text(a, "Task cost ceiling (estimate):", "سقف كلفة المهمة (تقديري):")));
        }
        label(
            ui,
            a,
            "Each batch stops for your visual approval before another provider request.",
            "تتوقف كل دفعة لمراجعتك البصرية قبل أي طلب جديد إلى المزود.",
        );
        let issue = s.production_issue(available, document, a);
        if let Some(reason) = issue {
            rtl::label(ui, reason);
        }
        if ui.add_enabled(issue.is_none(), egui::Button::new(rtl::widget(ui, text(a, "Produce", "إنتاج")))).clicked() {
            requests.push(AiRequest::Start(TaskRequest {
                prompt: s.prompt.clone(),
                settings: s.settings.clone(),
                auto_select_skills: s.automatic_skills,
                skill_ids: if s.automatic_skills { Vec::new() } else { s.selected_skills.iter().cloned().collect() },
                target: if s.new_document {
                    TaskTarget::NewA4 { pages: s.pages }
                } else if s.continue_book {
                    TaskTarget::ContinueBook
                } else {
                    TaskTarget::CurrentDocument
                },
                export_directory: (!s.directory.trim().is_empty()).then(|| s.directory.clone()),
            }));
        }
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Advanced settings", "إعدادات متقدمة"))).show(ui, |ui| {
            budgets(ui, s, a);
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut s.settings.output_mode, OutputMode::NativeOnly, text(a, "Editable elements", "عناصر قابلة للتحرير"));
                ui.selectable_value(
                    &mut s.settings.output_mode,
                    OutputMode::NativeWithImages,
                    text(a, "Editable elements and images", "عناصر قابلة للتحرير وصور"),
                );
            });
            if s.settings.output_mode == OutputMode::NativeWithImages {
                model(ui, a, "production_image_provider", &mut s.settings.image_model, true, requests);
            }
            ui.checkbox(&mut s.automatic_skills, rtl::widget(ui, text(a, "Automatic skills", "المهارات التلقائية")));
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
            if ui
                .add_enabled(
                    available && !s.directory.trim().is_empty(),
                    egui::Button::new(rtl::widget(ui, text(a, "Create original 8-page science example", "إنشاء مثال علوم أصلي من ٨ صفحات"))),
                )
                .clicked()
            {
                requests.push(AiRequest::CreateScienceSample { directory: s.directory.clone() });
            }
        });
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Save, export and print", "الحفظ والتصدير والطباعة"))).show(ui, |ui| {
            export_print(ui, s, a, available && document, requests);
        });
    });
    ui.separator();
    if s.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof") || s.task_active() {
        batch_review(ui, s, a, requests);
    } else if s.snapshot.batch.is_some() {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Previous task", "المهمة السابقة"))).show(ui, |ui| batch_review(ui, s, a, requests));
    }
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
    let can_review = s.can_review_selected_proof();
    let can_approve = s.can_approve_batch();
    let page_count = s.proof_key.as_ref().map_or(0, |key| key.pages().count());
    let mut confirm_page = false;
    let Some(batch) = &s.snapshot.batch else {
        return;
    };
    ui.separator();
    label(ui, a, "Batch production and proof", "إنتاج الدفعات والمراجعة");
    if let Some(style) = s
        .snapshot
        .reports
        .iter()
        .rev()
        .find(|report| report.id.starts_with(&format!("{}-batch-", batch.job_id)))
        .and_then(|report| report.design_style.as_ref())
    {
        rtl::label(
            ui,
            format!(
                "{}: {} · {}",
                text(a, "Pinned design style", "نمط التصميم المثبّت"),
                style.name,
                style.revision.chars().take(12).collect::<String>()
            ),
        );
        if style.execution_mode == "native_templates" {
            label(
                ui,
                a,
                "Native template execution — one content plan; no automatic paid command repairs.",
                "تنفيذ قوالب أصلية — تخطيط محتوى واحد، دون تصحيح أوامر مدفوع تلقائياً.",
            );
        }
    }
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
                    s.proof_loaded = None;
                    s.proof_texture = None;
                }
                if s.reviewed_proofs.iter().any(|(page, _)| page == path) {
                    label(ui, a, "Reviewed", "تمت المراجعة");
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
            "افتح كل صورة وأكد مراجعتها. تخضع الدفعة التالية لحدود المهمة التراكمية نفسها.",
        );
        rtl::label(ui, format!("{} {} / {}", text(a, "Pages confirmed:", "الصفحات المؤكد مراجعتها:"), s.reviewed_proofs.len(), page_count));
        if page_count == 0 {
            label(ui, a, "Verified previews for every batch page are required.", "يلزم توفر صور موثقة لجميع صفحات الدفعة.");
        } else if !s.proof_document_matches {
            label(
                ui,
                a,
                "The active document does not match this proof revision. Return to that document or restart the task after changes.",
                "المستند النشط لا يطابق إصدار البروفة. ارجع إلى المستند نفسه أو أعد بدء المهمة بعد تعديلاته.",
            );
        }
        let review_response =
            ui.add_enabled(can_review, egui::Button::new(rtl::widget(ui, text(a, "I reviewed the displayed page", "أؤكد مراجعة الصفحة المعروضة"))));
        if AiUiState::fresh_review_gesture(s.review_input_is_human, s.proof_key.as_ref(), &mut s.review_pointer_press, &review_response) {
            confirm_page = true;
        }
        let final_batch = batch.current_batch >= batch.total_batches;
        let (en, ar) = if final_batch {
            ("Approve final proof and finish", "اعتماد المراجعة النهائية وإنهاء المهمة")
        } else {
            ("Approve proof and generate next batch", "اعتماد المراجعة وإنشاء الدفعة التالية")
        };
        let approve_response = ui.add_enabled(can_approve, egui::Button::new(rtl::widget(ui, text(a, en, ar))));
        if AiUiState::fresh_review_gesture(s.review_input_is_human, s.proof_key.as_ref(), &mut s.review_pointer_press, &approve_response) {
            s.batch_approval_gesture = s.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
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
    if confirm_page {
        s.review_selected_proof();
        ui.ctx().request_repaint();
    }
}

fn budgets(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
    ui.checkbox(
        &mut s.settings.allow_paid_repairs,
        rtl::widget(ui, text(a, "Allow extra automatic planning requests (may be billed)", "السماح بطلبات تخطيط إضافية تلقائية قد تكون مدفوعة")),
    );
    let repairs = s.settings.allow_paid_repairs;
    let b = &mut s.settings.budget;
    egui::Grid::new("ai_limits").num_columns(2).show(ui, |ui| {
        label(ui, a, "Total tokens", "مجموع الرموز");
        ui.add(egui::DragValue::new(&mut b.token_limit).range(256..=4_000_000));
        ui.end_row();
        label(ui, a, "Output tokens per request", "رموز الإخراج لكل طلب");
        ui.add(egui::DragValue::new(&mut b.max_output_tokens).range(1..=65_536));
        ui.end_row();
        label(ui, a, "Maximum requests", "أقصى عدد للطلبات");
        ui.add(egui::DragValue::new(&mut b.max_requests).range(1..=64));
        ui.end_row();
        label(ui, a, "Maximum extra planning requests", "أقصى طلبات تخطيط إضافية");
        ui.add_enabled(repairs, egui::DragValue::new(&mut b.repair_attempts).range(0..=3));
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
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Design style library", "مكتبة أنماط التصميم"))).default_open(true).show(ui, |ui| {
            label(
                ui,
                a,
                "Reusable rules, native page families and recorded corrections. Technical checks do not replace educator or visual review.",
                "قواعد وقوالب أصلية وتصحيحات محفوظة. الفحص التقني لا يغني عن المراجعة التربوية والبصرية.",
            );
            let mut seen = BTreeSet::new();
            for card in s.snapshot.skills.iter().rev() {
                if let Some(profile) = &card.design_profile
                    && seen.insert(&card.id)
                {
                    ui.horizontal_wrapped(|ui| {
                        rtl::label(ui, &profile.name);
                        rtl::label(
                            ui,
                            format!(
                                "{}–{} · {} × {} mm · {} mm · {} {}",
                                profile.age_range[0],
                                profile.age_range[1],
                                profile.page_mm[0],
                                profile.page_mm[1],
                                profile.margin_mm,
                                profile.page_families.len(),
                                text(a, "page families", "قوالب صفحات")
                            ),
                        );
                        if button(ui, a, "Inspect style", "فحص النمط") {
                            requests.push(AiRequest::InspectSkill { id: card.id.clone(), revision: card.revision.clone() });
                        }
                        if ui
                            .add_enabled(card.enabled && card.compatible, egui::Button::new(rtl::widget(ui, text(a, "Use style", "استخدام النمط"))))
                            .clicked()
                        {
                            s.settings.design_style = Some(card.id.clone());
                            s.tab = 0;
                        }
                    });
                }
            }
        });
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
                    let mut status = if c.automatically_validated {
                        text(a, "Auto checked", "فُحصت تلقائياً")
                    } else {
                        text(a, if c.approved { "Approved" } else { "Draft" }, if c.approved { "معتمدة" } else { "مسودة" })
                    }
                    .to_owned();
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
                    let mut enabled = c.enabled && c.compatible;
                    if ui
                        .add_enabled(c.compatible, egui::Checkbox::new(&mut enabled, rtl::widget(ui, text(a, "Use skill", "استخدام المهارة"))))
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
            if let Some(profile) =
                s.snapshot.skills.iter().find(|card| card.id == r.id && card.revision == r.revision).and_then(|card| card.design_profile.as_ref())
            {
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Style rules and correction examples", "قواعد النمط وأمثلة التصحيح")))
                    .default_open(true)
                    .show(ui, |ui| {
                        for (en, ar, rules) in [
                            ("Typography", "الخطوط", &profile.typography),
                            ("Layout", "التوزيع", &profile.layout),
                            ("Pedagogy", "التدرج التعليمي", &profile.pedagogy),
                            ("Illustrations", "الرسوم", &profile.illustration),
                        ] {
                            label(ui, a, en, ar);
                            for rule in rules {
                                ui.add(egui::Label::new(rtl::widget(ui, rule)).wrap());
                            }
                        }
                        for family in &profile.page_families {
                            ui.horizontal_wrapped(|ui| {
                                rtl::label(ui, format!("{}: {}", family.id, family.purpose));
                                if button(ui, a, "Preview page family", "معاينة قالب الصفحة") {
                                    requests.push(AiRequest::RunDesignTemplate {
                                        id: r.id.clone(),
                                        revision: r.revision.clone(),
                                        template: family.template.clone(),
                                    });
                                }
                            });
                        }
                        for correction in &profile.corrections {
                            ui.add(
                                egui::Label::new(rtl::widget(ui, format!("{} → {} ({})", correction.before, correction.after, correction.reason)))
                                    .wrap(),
                            );
                        }
                    });
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Save a design correction", "حفظ تصحيح للنمط"))).show(ui, |ui| {
                    field(ui, a, "Before", "قبل التصحيح", &mut s.correction_before);
                    field(ui, a, "After", "المطلوب بعد التصحيح", &mut s.correction_after);
                    field(ui, a, "Reason", "سبب التصحيح", &mut s.correction_reason);
                    let ready = [&s.correction_before, &s.correction_after, &s.correction_reason]
                        .iter()
                        .all(|value| !value.trim().is_empty() && value.len() <= 512);
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(rtl::widget(ui, text(a, "Save correction as a new revision", "حفظ التصحيح بإصدار جديد"))),
                        )
                        .clicked()
                    {
                        requests.push(AiRequest::AddDesignCorrection {
                            id: r.id.clone(),
                            revision: r.revision.clone(),
                            correction: designcraft_ai_types::DesignCorrection {
                                before: s.correction_before.clone(),
                                after: s.correction_after.clone(),
                                reason: s.correction_reason.clone(),
                            },
                        });
                    }
                    label(
                        ui,
                        a,
                        "Explicit reusable guidance; this does not train model weights or approve a page batch.",
                        "توجيه محفوظ للاستعمال لاحقاً؛ لا يدرّب أوزان النموذج ولا يعتمد دفعة صفحات.",
                    );
                });
            }
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
        ui.checkbox(
            &mut s.build_design_style,
            rtl::widget(ui, text(a, "Build a design style with multiple native page templates", "بناء نمط تصميم بقواعد وقوالب صفحات متعددة")),
        );
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
                    design_style: s.build_design_style,
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
                requests.push(AiRequest::GenerateSkillFromDocument {
                    prompt: s.skill_prompt.clone(),
                    settings: s.settings.clone(),
                    design_style: s.build_design_style,
                });
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
            if let Some(style) = &r.design_style {
                rtl::label(
                    ui,
                    format!("{}: {} · {}", text(a, "Design style", "نمط التصميم"), style.name, style.revision.chars().take(12).collect::<String>()),
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use designcraft_engine::Session;
    use egui_kittest::kittest::{NodeT, Queryable};
    use serde_json::json;
    #[test]
    fn production_composer_exposes_model_prompt_and_action_without_advanced_settings() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        app.ui.ai.open = true;
        app.ui.ai.snapshot.available = true;
        app.ui.ai.initialized = true;
        app.ui.ai.prompt = "A long request\n".repeat(200);
        let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
        harness.run_steps(3);
        assert!(harness.query_by_label("Model or agent").is_some());
        assert!(harness.query_by_label("What would you like to create?").is_some());
        assert!(harness.query_by_label("Produce").is_some());
        assert!(harness.get_by_label("Produce").rect().max.y < 650.0, "long briefs must not push the action off screen");
        assert!(harness.query_by_label("Input").is_none(), "pricing belongs in advanced settings");
    }
    #[test]
    fn disabled_production_button_explains_the_missing_model() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.run("ai.production", json!({})).unwrap();
        app.ui.ai.prompt = "Create an editable activity".into();
        let mut harness = crate::test_window::open(app, egui::vec2(1500.0, 1100.0));
        harness.run_steps(3);
        assert!(harness.get_by_label("Produce").accesskit_node().is_disabled());
        assert!(harness.query_by_label("Choose a text model above.").is_some(), "a disabled button must explain what to fix");
        harness.state_mut().app.ui.ai.settings.text_model.model = "fixture-model".into();
        harness.run_steps(3);
        assert!(!harness.get_by_label("Produce").accesskit_node().is_disabled());
        assert!(harness.query_by_label("Choose a text model above.").is_none());
    }

    #[test]
    fn production_readiness_preserves_workflow_budget_and_human_review_guards() {
        let mut state = AiUiState { prompt: "Native activity".into(), ..Default::default() };
        state.settings.text_model.model = "text-model".into();
        state.settings.output_mode = OutputMode::NativeWithImages;
        state.settings.image_model.provider = ProviderKind::Gemini;
        assert_eq!(state.production_issue(true, false, false), Some("Choose an image model in Providers and keys."));
        state.settings.image_model.model = "image-model".into();
        state.settings.budget.max_cost_usd = Some(1.8);
        assert_eq!(state.production_issue(true, false, true), Some("أدخل أسعار المزود وأكّدها من الإعدادات المتقدمة."));
        state.rates_confirmed = true;
        assert_eq!(state.production_issue(true, false, false), None);
        state.settings.image_model.provider = ProviderKind::ComfyUi;
        state.settings.image_model.model.clear();
        assert_eq!(state.production_issue(true, false, false), Some("Complete a valid ComfyUI API workflow in Providers and keys."));
        state.workflow_json = r#"{"node":{"text":"{{prompt}}"}}"#.into();
        state.edit_workflow();
        assert_eq!(state.production_issue(true, false, false), None, "a workflow can specify its checkpoint without a model placeholder");
        state.snapshot.batch = Some(BatchSnapshot { status: "awaiting_human_proof".into(), ..Default::default() });
        assert_eq!(
            state.production_issue(true, false, false),
            Some("Review and approve the current batch, or cancel the task before starting another.")
        );
        assert!(!state.can_approve_batch());
    }
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

    struct ProofFixture {
        app: DesignApp,
        ctx: egui::Context,
        checkpoint: AiSnapshot,
        calls: std::rc::Rc<std::cell::Cell<u32>>,
    }
    impl ProofFixture {
        fn new() -> Self {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let host_calls = calls.clone();
            let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255])));
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).unwrap();
            let png = png.into_inner();
            let mut app = DesignApp::new(
                Session::new(),
                Services {
                    read: Some(Box::new(move |_| Ok(png.clone()))),
                    ai: Some(Box::new(move |_, _| {
                        host_calls.set(host_calls.get() + 1);
                        AiSnapshot { available: true, ..Default::default() }
                    })),
                    ..Default::default()
                },
            );
            app.run("file.new", json!({})).unwrap();
            let document = app.session.active().map(|d| (d.uid, d.revision)).unwrap();
            let checkpoint = AiSnapshot {
                available: true,
                batch: Some(BatchSnapshot {
                    job_id: "two-page-proof".into(),
                    total_pages: 4,
                    pages_per_batch: 2,
                    current_batch: 1,
                    total_batches: 2,
                    completed_pages: 2,
                    status: "awaiting_human_proof".into(),
                    checkpoint: 1,
                    document_uid: Some(document.0),
                    document_revision: Some(document.1),
                    batches: vec![designcraft_ai_types::protocol::BatchReport {
                        batch: 1,
                        page_count: 2,
                        proof_paths: vec!["book.designcraft".into(), "proof.pdf".into(), "page-1.png".into(), "page-2.png".into()],
                        proof_sha256: vec!["0".repeat(64), "1".repeat(64), "2".repeat(64), "3".repeat(64)],
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            };
            app.ui.ai.accept(checkpoint.clone());
            app.ui.ai.sync_proof_document(Some(document));
            app.ui.ai.review_input_is_human = true;
            Self { app, ctx: egui::Context::default(), checkpoint, calls }
        }

        fn view(&mut self, path: &str) {
            self.app.ui.ai.proof_path = Some(path.into());
            self.app.ui.ai.proof_loaded = None;
            load_proof_preview(&mut self.app, &self.ctx);
        }

        fn confirm_all(&mut self) {
            let pages: Vec<_> = self.app.ui.ai.proof_key.as_ref().unwrap().pages().map(|(path, _)| path.clone()).collect();
            for path in pages {
                self.view(&path);
                self.app.ui.ai.review_selected_proof();
            }
        }
    }

    #[test]
    fn batch_approval_requires_explicit_review_of_every_page_not_first_autoload() {
        let mut fixture = ProofFixture::new();
        load_proof_preview(&mut fixture.app, &fixture.ctx);
        assert!(fixture.app.ui.ai.proof_texture.is_some());
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "autoload is not human review");
        assert!(!fixture.app.ui.ai.can_approve_batch());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
        assert!(!fixture.app.ui.ai.can_approve_batch(), "first page alone must not approve a two-page batch");
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 0);
        fixture.app.ui.ai.proof_path = Some("page-2.png".into());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "the previous texture cannot confirm the newly selected page");
        fixture.view("page-2.png");
        assert!(!fixture.app.ui.ai.can_approve_batch(), "loading the second page is not confirmation");
        fixture.app.ui.ai.review_selected_proof();
        assert!(fixture.app.ui.ai.can_approve_batch());
    }

    #[test]
    fn batch_review_resets_for_checkpoint_proof_fingerprints_and_document_revision() {
        for change in ["checkpoint", "png hash", "pdf hash", "path", "page range", "task shape", "document uid", "document revision"] {
            let mut fixture = ProofFixture::new();
            fixture.confirm_all();
            assert!(fixture.app.ui.ai.can_approve_batch());
            let mut changed = fixture.checkpoint.clone();
            let batch = changed.batch.as_mut().unwrap();
            match change {
                "checkpoint" => batch.checkpoint += 1,
                "png hash" => batch.batches[0].proof_sha256[2] = "a".repeat(64),
                "pdf hash" => batch.batches[0].proof_sha256[1] = "a".repeat(64),
                "path" => batch.batches[0].proof_paths[2] = "replacement.png".into(),
                "page range" => batch.batches[0].page_start += 1,
                "task shape" => batch.total_pages += 1,
                "document uid" => batch.document_uid = batch.document_uid.map(|uid| uid + 1),
                "document revision" => batch.document_revision = batch.document_revision.map(|revision| revision + 1),
                _ => panic!("Unknown proof regression case: {change}"),
            }
            let document = Some((batch.document_uid.unwrap(), batch.document_revision.unwrap()));
            fixture.app.ui.ai.accept(changed);
            fixture.app.ui.ai.sync_proof_document(document);
            assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "{change}");
            assert!(fixture.app.ui.ai.proof_texture.is_none(), "{change}");
            assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
        }
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        let document = fixture.app.ui.ai.proof_key.as_ref().unwrap().document;
        fixture.app.ui.ai.sync_proof_document(Some((document.0, document.1 + 1)));
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
        assert!(!fixture.app.ui.ai.can_review_selected_proof());
        assert!(!fixture.app.ui.ai.can_approve_batch());
    }

    #[test]
    fn empty_missing_duplicate_or_unfingerprinted_batch_pages_cannot_approve() {
        for change in ["empty", "missing png", "missing hash", "duplicate png", "duplicate pdf", "case alias png", "invalid hash"] {
            let mut fixture = ProofFixture::new();
            let mut changed = fixture.checkpoint.clone();
            let report = &mut changed.batch.as_mut().unwrap().batches[0];
            match change {
                "empty" => {
                    report.page_count = 0;
                    report.proof_paths.clear();
                    report.proof_sha256.clear();
                }
                "missing png" => {
                    report.proof_paths.pop();
                    report.proof_sha256.pop();
                }
                "missing hash" => {
                    report.proof_sha256.pop();
                }
                "duplicate png" => report.proof_paths[3] = report.proof_paths[2].clone(),
                "case alias png" => report.proof_paths[3] = "PAGE-1.PNG".into(),
                "duplicate pdf" => {
                    report.proof_paths.push("proof.pdf".into());
                    report.proof_sha256.push("1".repeat(64));
                }
                "invalid hash" => report.proof_sha256[2] = "missing".into(),
                _ => panic!("Unknown proof regression case: {change}"),
            }
            fixture.app.ui.ai.accept(changed);
            assert!(fixture.app.ui.ai.proof_key.is_none(), "{change}");
            assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
        }
    }

    #[test]
    fn failed_page_reload_revokes_its_previous_human_confirmation() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        assert!(fixture.app.ui.ai.can_approve_batch());
        fixture.app.services.read = Some(Box::new(|_| Err("synthetic proof read failure".into())));
        fixture.view("page-1.png");
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
        assert!(!fixture.app.ui.ai.can_approve_batch());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "a failed preview cannot be confirmed");
    }

    #[test]
    fn control_cannot_approve_after_all_pages_reviewed_but_typed_human_action_can() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        assert!(fixture.app.ui.ai.can_approve_batch());
        assert!(fixture.app.run("ai.request", json!({"method":"approve_batch", "job_id":"two-page-proof", "checkpoint":1})).is_err());
        assert_eq!(fixture.calls.get(), 0);
        assert!(
            dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err(),
            "absence of control input is not a human gesture"
        );
        let ctx = fixture.ctx.clone();
        let mut focused = false;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = ui.button("Approve proof");
            response.request_focus();
            focused = response.has_focus();
        });
        output.textures_delta.clear();
        assert!(focused);
        let input = egui::RawInput {
            events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let response = ui.button("Approve proof");
            assert!(AiUiState::fresh_review_gesture(
                true,
                fixture.app.ui.ai.proof_key.as_ref(),
                &mut fixture.app.ui.ai.review_pointer_press,
                &response
            ));
            fixture.app.ui.ai.batch_approval_gesture =
                fixture.app.ui.ai.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
            dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).unwrap();
        });
        output.textures_delta.clear();
        assert_eq!(fixture.calls.get(), 1);
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "the human approval is consumed once");
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 1);
    }

    #[test]
    fn idle_batch_review_frames_never_confirm_or_approve_and_expired_gesture_is_rejected() {
        let mut fixture = ProofFixture::new();
        fixture.view("page-1.png");
        let ctx = fixture.ctx.clone();
        for reviewed in [false, true] {
            if reviewed {
                fixture.confirm_all();
            }
            let before = fixture.app.ui.ai.reviewed_proofs.clone();
            for _ in 0..4 {
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let mut requests = Vec::new();
                    batch_review(ui, &mut fixture.app.ui.ai, false, &mut requests);
                    assert!(requests.is_empty());
                    assert!(fixture.app.ui.ai.batch_approval_gesture.is_none());
                });
                output.textures_delta.clear();
                assert_eq!(fixture.app.ui.ai.reviewed_proofs, before);
            }
        }
        fixture.app.ui.ai.batch_approval_gesture = fixture.app.ui.ai.proof_key.clone().map(|key| (key, ctx.clone(), ctx.cumulative_frame_nr()));
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 0);
    }

    #[test]
    fn review_gesture_rejects_repeated_keys_and_pointer_press_from_previous_proof() {
        fn frame(fixture: &mut ProofFixture, events: Vec<egui::Event>) -> (egui::Rect, bool) {
            let ctx = fixture.ctx.clone();
            let mut result = (egui::Rect::NOTHING, false);
            let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                let response = ui.button("Confirm proof");
                response.request_focus();
                result = (
                    response.rect,
                    AiUiState::fresh_review_gesture(
                        true,
                        fixture.app.ui.ai.proof_key.as_ref(),
                        &mut fixture.app.ui.ai.review_pointer_press,
                        &response,
                    ),
                );
            });
            output.textures_delta.clear();
            result
        }
        let mut fixture = ProofFixture::new();
        let (rect, activated) = frame(&mut fixture, vec![]);
        assert!(!activated);
        let key = |repeat| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat, modifiers: Default::default() };
        assert!(frame(&mut fixture, vec![key(false)]).1);
        assert!(!frame(&mut fixture, vec![key(true)]).1);
        assert!(
            !frame(
                &mut fixture,
                vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() }]
            )
            .1
        );
        assert!(frame(&mut fixture, vec![key(false)]).1);
        let pointer =
            |pressed| egui::Event::PointerButton { pos: rect.center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        assert!(!frame(&mut fixture, vec![egui::Event::PointerMoved(rect.center()), pointer(true)]).1);
        assert!(fixture.app.ui.ai.review_pointer_press.is_some());
        let mut changed = fixture.checkpoint.clone();
        changed.batch.as_mut().unwrap().checkpoint += 1;
        fixture.app.ui.ai.accept(changed);
        assert!(!frame(&mut fixture, vec![pointer(false)]).1, "release cannot approve a replacement proof");
        assert!(!frame(&mut fixture, vec![pointer(true)]).1);
        assert!(frame(&mut fixture, vec![pointer(false)]).1, "a fresh press and release on the current proof remains usable");
        assert!(frame(&mut fixture, vec![pointer(true), pointer(false)]).1, "coalesced fresh pointer input remains usable");
        let outside = egui::Event::PointerButton {
            pos: rect.right_bottom() + egui::vec2(50.0, 50.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        assert!(!frame(&mut fixture, vec![outside, pointer(false)]).1, "a press originating outside the proof widget is not approval");
    }

    #[test]
    fn synthetic_control_frames_cannot_confirm_pages_or_submit_batch_approval() {
        let mut fixture = ProofFixture::new();
        fixture.view("page-1.png");
        fixture.app.control_input_frame.store(true, std::sync::atomic::Ordering::Relaxed);
        fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
        fixture.app.ui.ai.review_selected_proof();
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
        fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
        fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
        fixture.confirm_all();
        for control_request in [true, false] {
            fixture.app.in_control = control_request;
            fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
            assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
            assert_eq!(fixture.calls.get(), 0);
        }
    }

    #[test]
    fn human_only_typed_dispatch_rejects_control_requests_and_synthetic_frames() {
        let mut fixture = ProofFixture::new();
        for control_request in [true, false] {
            fixture.app.in_control = control_request;
            fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
            for request in [
                AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() },
                AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() },
                AiRequest::SaveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone(), key: "fixture".into() },
                AiRequest::RemoveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone() },
                AiRequest::RefreshCredentials,
                AiRequest::OpenPrintProof { token: "fixture".into() },
                AiRequest::StartLocalOllama { executable: "C:/fixture/ollama.exe".into() },
                AiRequest::StopLocalOllama,
            ] {
                assert!(dispatch(&mut fixture.app, request).is_err());
                assert_eq!(fixture.calls.get(), 0);
            }
        }
        fixture.app.in_control = false;
        fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
        dispatch(&mut fixture.app, AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() }).unwrap();
        dispatch(&mut fixture.app, AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() }).unwrap();
        assert_eq!(fixture.calls.get(), 2, "ordinary human typed actions still reach the mock host");
    }
}
