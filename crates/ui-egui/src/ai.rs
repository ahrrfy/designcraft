//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{AiRequest, AiSettings, AiSnapshot, BatchSnapshot, ProductionDraft, TaskRequest, TaskTarget, TokenRates};
use designcraft_ai_types::{ModelSelection, OutputMode, ProviderKind};
use serde_json::Value;

const MAX_REQUEST_ALLOWANCE: u32 = 10_000;

/// Never persisted, and never included in a debug dump.
#[derive(Clone, Default)]
struct Secret(String);

struct OpenDocument {
    index: usize,
    uid: u64,
    path: Option<String>,
    title: String,
    facing_pages: bool,
    right_to_left_binding: bool,
    pages: usize,
    remaining_pages: usize,
    continuable: bool,
}
impl OpenDocument {
    fn from_document(index: usize, document: &designcraft_engine::DocState) -> Self {
        let pages = document.doc.page_count();
        let first_blank = (0..pages).find(|page| !document.doc.page_has_items(*page));
        Self {
            index,
            uid: document.uid,
            path: document.path.clone(),
            title: document.title(),
            facing_pages: document.doc.settings.facing_pages,
            right_to_left_binding: document.doc.settings.right_to_left_binding,
            pages,
            remaining_pages: first_blank.map_or(0, |first| pages.saturating_sub(first)),
            continuable: first_blank.is_some_and(|first| first > 0 && (first..pages).all(|page| !document.doc.page_has_items(page))),
        }
    }
}

struct RequestEstimate {
    review_pages: u32,
    remaining_pages: u32,
    remaining_groups: u32,
    book_allowance: u32,
    scripted: bool,
    image_allowance: u32,
    vision_allowance: u32,
}

#[derive(Clone)]
struct CompanionProof {
    path: String,
    texture: Option<egui::TextureHandle>,
    error: String,
}

/// Global zero-based pages: the cover stands alone, followed by 2–3, 4–5, … .
fn facing_proof_pair(page: u32, right_to_left: bool) -> Option<[u32; 2]> {
    if page == 0 {
        return None;
    }
    let first = if page.is_multiple_of(2) { page.checked_sub(1)? } else { page };
    let second = first.checked_add(1)?;
    Some(if right_to_left { [second, first] } else { [first, second] })
}

fn automatic_proof_binding(brief: &str, fallback: bool) -> bool {
    let arabic = |character: char| {
        character.is_alphabetic()
            && matches!(u32::from(character), 0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff)
    };
    let mut content = None;
    for line in brief.lines() {
        let lower = line.to_lowercase();
        if lower.contains("binding") || lower.contains("اتجاه التجليد") || lower.contains("اتجاه الكتاب") {
            if ["righttoleft", "right-to-left", "rtl", "من اليمين"].iter().any(|word| lower.contains(*word)) {
                return true;
            }
            if ["lefttoright", "left-to-right", "ltr", "من اليسار"].iter().any(|word| lower.contains(*word)) {
                return false;
            }
        }
        if ["content copy language", "content language", "لغة المحتوى", "لغة الكتاب"].iter().any(|word| lower.contains(*word)) {
            let value = lower.split_once(':').map_or(lower.as_str(), |(_, value)| value);
            if ["arabic", "العربي", "بالعربي", "عربية"].iter().any(|word| value.contains(*word)) {
                content = Some(true);
            } else if ["english", "إنجليزي", "انجليزي", "إنكليزي", "انكليزي"].iter().any(|word| value.contains(*word)) {
                content = Some(false);
            } else if value.chars().any(arabic) {
                content = Some(true);
            }
        }
    }
    content.unwrap_or_else(|| {
        let lower = brief.to_lowercase();
        if ["باللغة الإنجليزية", "باللغة الانجليزية", "باللغة الإنكليزية", "باللغة الانكليزية", "english-only", "english only"]
            .iter()
            .any(|phrase| lower.contains(*phrase))
        {
            false
        } else if brief.chars().any(arabic) {
            true
        } else if brief.chars().any(|character| character.is_ascii_alphabetic()) {
            false
        } else {
            fallback
        }
    })
}
impl RequestEstimate {
    fn allowance(&self) -> u32 {
        self.review_pages.saturating_add(self.image_allowance).saturating_add(self.vision_allowance)
    }
}

/// A display estimate only: runtime still validates all authored PAGE blocks and fields.
fn explicit_script_pages(brief: &str) -> Option<u32> {
    if brief.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES || brief.contains('\0') {
        return None;
    }
    let mut pages = 0u32;
    let mut fenced = false;
    for line in brief.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim_start();
        if heading.get(..4).is_none_or(|prefix| !prefix.eq_ignore_ascii_case("page")) {
            continue;
        }
        let rest = heading.get(4..)?;
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let boundary = rest.find(|character: char| !character.is_ascii_digit()).unwrap_or(rest.len());
        let number = rest.get(..boundary)?.parse::<u32>().ok()?;
        if number != pages.checked_add(1)? || number > 1000 {
            return None;
        }
        let suffix = rest.get(boundary..)?.trim_start();
        let title = suffix
            .strip_prefix('—')
            .or_else(|| suffix.strip_prefix('–'))
            .or_else(|| suffix.strip_prefix('-'))
            .or_else(|| suffix.strip_prefix(':'))?
            .trim();
        if title.is_empty() || title.chars().count() > 512 {
            return None;
        }
        pages = number;
    }
    (pages > 0).then_some(pages)
}
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
        let max_proofs =
            usize::try_from(current.page_count).ok()?.checked_add(usize::try_from(current.page_count.div_ceil(32)).ok()?)?.checked_add(1)?;
        if current.proof_paths.len() != current.proof_sha256.len()
            || current.proof_paths.len() > max_proofs
            || !(1..=1000).contains(&current.page_count)
        {
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

    fn same_proof_except_revision(&self, other: &Self) -> bool {
        let mut normalized = other.clone();
        normalized.document.1 = self.document.1;
        self == &normalized
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
    restored_target_unconfirmed: bool,
    pages: u32,
    automatic_skills: bool,
    selected_skills: BTreeSet<String>,
    directory: String,
    import_path: String,
    export_path: String,
    skill_export_path: String,
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
    proof_validation_requested: Option<(u64, u64)>,
    proof_document_matches: bool,
    proof_binding: Option<bool>,
    proof_pair_view: bool,
    proof_full_resolution: bool,
    review_input_is_human: bool,
    review_pointer_press: Option<(BatchReviewKey, egui::Id)>,
    batch_approval_gesture: Option<(BatchReviewKey, egui::Context, u64)>,
    proof_path: Option<String>,
    proof_loaded: Option<String>,
    proof_texture: Option<egui::TextureHandle>,
    companion_proof: Option<CompanionProof>,
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
            restored_target_unconfirmed: false,
            pages: 8,
            automatic_skills: true,
            selected_skills: BTreeSet::new(),
            directory: String::new(),
            import_path: String::new(),
            export_path: String::new(),
            skill_export_path: "skill-package.zip".into(),
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
            proof_validation_requested: None,
            proof_document_matches: false,
            proof_binding: None,
            proof_pair_view: true,
            proof_full_resolution: false,
            review_input_is_human: false,
            review_pointer_press: None,
            batch_approval_gesture: None,
            proof_path: None,
            proof_loaded: None,
            proof_texture: None,
            companion_proof: None,
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
    fn load_production_request(&mut self, request: &TaskRequest) {
        self.prompt = request.prompt.clone();
        self.settings = request.settings.clone();
        self.automatic_skills = request.auto_select_skills;
        self.selected_skills = request.skill_ids.iter().cloned().collect();
        self.directory = request.export_directory.clone().unwrap_or_default();
        self.new_document = matches!(request.target, TaskTarget::NewA4 { .. });
        self.continue_book = matches!(request.target, TaskTarget::ContinueBook);
        if let TaskTarget::NewA4 { pages } = request.target {
            self.pages = pages;
        }
        self.rates_confirmed =
            self.settings.budget.rates.as_ref().is_some_and(|rates| rates.input_per_million > 0.0 || rates.output_per_million > 0.0);
        self.workflow_json = self.settings.comfy_workflow.as_ref().map(Value::to_string).unwrap_or_default();
        self.workflow_error.clear();
        self.restored_target_unconfirmed = false;
    }

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
            if let Some(draft) = &snapshot.production_draft {
                self.load_production_request(&draft.request);
                self.restored_target_unconfirmed = !self.new_document;
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
            let preserve_review = self
                .proof_key
                .as_ref()
                .zip(proof_key.as_ref())
                .is_some_and(|(previous, next)| self.proof_document_version == Some(next.document) && previous.same_proof_except_revision(next));
            self.review_pointer_press = None;
            self.batch_approval_gesture = None;
            self.proof_validation_requested = None;
            if !preserve_review {
                self.proof_path = proof_key.as_ref().and_then(|key| key.pages().next()).map(|(path, _)| path.clone());
                self.reviewed_proofs.clear();
                self.proof_binding = None;
                self.proof_loaded = None;
                self.proof_texture = None;
                self.companion_proof = None;
                self.proof_error.clear();
            }
            self.proof_document_matches = false;
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
            if self.proof_document_version.map(|(uid, _)| uid) != document.map(|(uid, _)| uid) {
                self.reviewed_proofs.clear();
            }
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

    /// Navigation never carries a loaded texture or a held review gesture to another page.
    fn select_proof(&mut self, path: &str) -> bool {
        if self.proof_path.as_deref() == Some(path) || !self.proof_key.as_ref().is_some_and(|key| key.pages().any(|(page, _)| page == path)) {
            return false;
        }
        self.proof_path = Some(path.into());
        self.proof_loaded = None;
        self.proof_texture = None;
        self.proof_error.clear();
        self.review_pointer_press = None;
        true
    }

    fn companion_proof_path(&self) -> Option<&str> {
        if !self.settings.facing_pages || !self.proof_pair_view {
            return None;
        }
        let key = self.proof_key.as_ref()?;
        let selected = key.pages().position(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))?;
        let page = key.page_range.0.checked_add(u32::try_from(selected).ok()?)?;
        let pair = facing_proof_pair(page, false)?;
        let companion = pair.into_iter().find(|candidate| *candidate != page)?;
        let index = companion.checked_sub(key.page_range.0)?;
        key.pages().nth(usize::try_from(index).ok()?).map(|(path, _)| path.as_str())
    }

    fn can_review_pair(&self) -> bool {
        self.proof_full_resolution
            && self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof")
            && self.can_review_selected_proof()
            && self.companion_proof_path().is_some_and(|path| {
                self.companion_proof.as_ref().is_some_and(|proof| proof.path == path && proof.texture.is_some() && proof.error.is_empty())
            })
    }

    fn production_task(&self) -> TaskRequest {
        TaskRequest {
            prompt: self.prompt.clone(),
            settings: self.settings.clone(),
            auto_select_skills: self.automatic_skills,
            skill_ids: if self.automatic_skills { Vec::new() } else { self.selected_skills.iter().cloned().collect() },
            target: if self.new_document {
                TaskTarget::NewA4 { pages: self.pages }
            } else if self.continue_book {
                TaskTarget::ContinueBook
            } else {
                TaskTarget::CurrentDocument
            },
            export_directory: (!self.directory.trim().is_empty()).then(|| self.directory.clone()),
        }
    }

    fn review_displayed_pair(&mut self) {
        if !self.can_review_pair() {
            return;
        }
        let companion = self.companion_proof_path().map(str::to_owned);
        self.review_selected_proof();
        if let Some(page) = self.proof_key.as_ref().and_then(|key| key.pages().find(|(path, _)| Some(path.as_str()) == companion.as_deref())) {
            self.reviewed_proofs.insert(page.clone());
        }
        let next =
            self.proof_key.as_ref().and_then(|key| key.pages().find(|page| !self.reviewed_proofs.contains(*page))).map(|(path, _)| path.clone());
        if let Some(next) = next {
            self.select_proof(&next);
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
        } else if self.prompt.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES {
            (
                "The full brief exceeds 1 MiB. Move large reference material into skill references.",
                "تجاوز نص التعليمات الكامل 1 ميبيبايت. انقل المراجع الكبيرة إلى مراجع المهارة.",
            )
        } else if self.settings.text_model.model.trim().is_empty() {
            ("Choose a text model above.", "اختر نموذج النص من الحقل أعلاه.")
        } else if !self.new_document && !document {
            ("Open a document or choose New document.", "افتح مستنداً أو اختر مستنداً جديداً.")
        } else if self.restored_target_unconfirmed {
            (
                "Choose the target book or prepare its remaining pages before producing the saved request.",
                "اختر الكتاب المستهدف أو جهّز صفحاته المتبقية قبل إنتاج التعليمات المحفوظة.",
            )
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

    fn request_estimate(&self, documents: &[OpenDocument], active: Option<usize>) -> Option<RequestEstimate> {
        if (!self.new_document && !self.continue_book) || self.settings.pages_per_batch == 0 || self.prompt.trim().is_empty() {
            return None;
        }
        let scripted_pages = explicit_script_pages(&self.prompt);
        let remaining = if self.new_document {
            if scripted_pages.is_some_and(|pages| pages != self.pages) {
                return None;
            }
            self.pages
        } else {
            let document = documents.iter().find(|document| Some(document.index) == active)?;
            if scripted_pages.is_some_and(|pages| u32::try_from(document.pages).ok() != Some(pages)) || (self.continue_book && !document.continuable)
            {
                return None;
            }
            u32::try_from(if self.continue_book { document.remaining_pages } else { document.pages }).ok()?
        };
        let review_pages = remaining.min(self.settings.pages_per_batch);
        let sources_per_page = if self.settings.output_mode == OutputMode::NativeOnly {
            0
        } else {
            self.settings
                .design_style
                .as_ref()
                .and_then(|id| self.snapshot.skills.iter().rev().find(|card| &card.id == id))
                .and_then(|card| card.design_profile.as_ref())
                .map(|profile| profile.page_families.iter().map(|family| family.image_slots.len()).max().unwrap_or(0))
                .map_or(1, |sources| u32::try_from(sources.min(8)).unwrap_or(8))
        };
        let image_allowance = review_pages.saturating_mul(sources_per_page);
        let vision_allowance = if self.settings.verify_generated_images { image_allowance } else { 0 };
        let per_page = 1u32.saturating_add(sources_per_page).saturating_add(if self.settings.verify_generated_images { sources_per_page } else { 0 });
        (review_pages > 0).then_some(RequestEstimate {
            review_pages,
            remaining_pages: remaining,
            remaining_groups: remaining.div_ceil(self.settings.pages_per_batch),
            book_allowance: remaining.saturating_mul(per_page),
            scripted: scripted_pages.is_some(),
            image_allowance,
            vision_allowance,
        })
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
    let production_request = if let AiRequest::Start(request) = &request { Some(request.clone()) } else { None };
    let snapshot = host(&mut app.session, request);
    let result = serde_json::to_value(&snapshot).map_err(|e| e.to_string());
    app.ui.ai.accept(snapshot);
    if let Some(request) = production_request {
        app.ui.ai.load_production_request(&request);
    }
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
        "preview_production" => AiRequest::PreviewProduction(serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?),
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
    state.proof_binding = app
        .session
        .active()
        .filter(|document| state.proof_document_matches && document.doc.settings.facing_pages)
        .map(|document| document.doc.settings.right_to_left_binding);
    let a = app.ui.language == "ar";
    let mut open = state.open;
    let mut requests = Vec::new();
    let available = app.services.ai.is_some() && state.snapshot.available;
    let documents: Vec<_> =
        app.session.documents().iter().enumerate().map(|(index, document)| OpenDocument::from_document(index, document)).collect();
    let mut activate_document = app.session.active_index();
    let original_document = activate_document;
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
                        usage.estimated_cost_usd.map_or_else(String::new, |cost| format!(" · {} ${cost:.6}", text(a, "Estimated", "التقدير"))),
                        if usage.has_unreported_usage {
                            text(a, " · Some usage unreported", " · بعض الاستخدام غير مبلغ")
                        } else {
                            ""
                        }
                    ),
                );
                if button(ui, a, "Cancel task — keep produced pages", "إلغاء المهمة مع الاحتفاظ بالصفحات المنتجة")
                {
                    requests.push(AiRequest::Cancel);
                }
            }
            let status_message = if state.error.is_empty() { &state.snapshot.message } else { &state.error };
            if !status_message.is_empty() {
                let summary: String = status_message.lines().next().unwrap_or("").chars().take(140).collect();
                rtl::label(ui, summary);
            }
            let reviewing = state.tab == 0 && state.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof");
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
                            rtl::label(ui, serde_json::to_string_pretty(issue).unwrap_or_else(|_| issue.to_string()));
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
            }
            if button(ui, a, "Standard limits", "حدود قياسية") { s.settings.resources = designcraft_ai_types::LocalResourcePolicy::default(); }
        });
        label(ui, a, "Conservative preset: 4096 context tokens and up to 2048 output tokens. Your review page count stays unchanged.", "الإعداد المحافظ: سياق 4096 رمزاً وإخراج حتى 2048 رمزاً. يبقى عدد صفحات مراجعتك كما اخترته.");
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

fn same_document_path(left: &str, right: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        left.replace('\\', "/").eq_ignore_ascii_case(&right.replace('\\', "/"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        left == right
    }
}

fn saved_book_document<'a>(draft: &ProductionDraft, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
    if let Some(path) = &draft.document_path {
        documents.iter().find(|document| document.path.as_ref().is_some_and(|open| same_document_path(path, open)))
    } else if let Some(uid) = draft.document_uid {
        documents.iter().find(|document| document.uid == uid)
    } else {
        // Recovered unsaved documents have no durable identity. Only the user's selected target
        // can be prepared; a matching title never selects a document or starts production.
        documents.iter().find(|document| Some(document.index) == selected)
    }
}

fn preserved_document_geometry<'a>(s: &AiUiState, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
    if s.new_document {
        return None;
    }
    documents.iter().find(|document| Some(document.index) == selected && (!s.continue_book || document.facing_pages))
}

fn production_binding(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, documents: &[OpenDocument], selected: Option<usize>) {
    if let Some(document) = preserved_document_geometry(s, documents, selected) {
        if document.facing_pages {
            let binding = text(
                a,
                if document.right_to_left_binding { "Right to left (RTL)" } else { "Left to right (LTR)" },
                if document.right_to_left_binding {
                    "من اليمين إلى اليسار (RTL)"
                } else {
                    "من اليسار إلى اليمين (LTR)"
                },
            );
            rtl::label(ui, format!("{} {binding}", text(a, "Existing book binding:", "اتجاه تجليد الكتاب الحالي:")));
        } else {
            label(ui, a, "Existing document uses single pages.", "يستخدم المستند الحالي صفحات منفردة.");
        }
    } else if s.settings.facing_pages {
        ui.horizontal_wrapped(|ui| {
            label(ui, a, "Book binding", "اتجاه التجليد");
            for (value, en, ar) in [
                (None, "Automatic (content language)", "تلقائي حسب لغة المحتوى"),
                (Some(true), "Right to left (RTL)", "من اليمين إلى اليسار (RTL)"),
                (Some(false), "Left to right (LTR)", "من اليسار إلى اليمين (LTR)"),
            ] {
                ui.radio_value(&mut s.settings.right_to_left_binding, value, rtl::widget(ui, text(a, en, ar)));
            }
        });
        if s.settings.right_to_left_binding.is_none() {
            label(
                ui,
                a,
                "Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.",
                "اتجاه التجليد تلقائي: العربية من اليمين إلى اليسار والإنجليزية من اليسار إلى اليمين. يبقى الغلاف منفرداً.",
            );
        }
    }
}

fn production_recovery(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    available: bool,
    documents: &[OpenDocument],
    activate_document: &mut Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    if let Some(batch) = &s.snapshot.batch
        && !s.snapshot.busy
        && (batch.status == "awaiting_human_proof" || (batch.status == "failed" && batch.retryable))
        && let Some(document) = documents.iter().find(|document| Some(document.uid) == batch.document_uid)
        && Some(document.index) != *activate_document
        && button(ui, a, "Open proof document", "فتح مستند البروفة")
    {
        *activate_document = Some(document.index);
    }
    if s.task_active() {
        return;
    }
    if let Some(draft) = &s.snapshot.production_draft
        && matches!(draft.request.target, TaskTarget::NewA4 { .. } | TaskTarget::ContinueBook)
    {
        let selected = saved_book_document(draft, documents, *activate_document).filter(|document| document.continuable);
        ui.horizontal_wrapped(|ui| {
            rtl::label(ui, format!("{} {}", text(a, "Saved book instructions:", "تعليمات الكتاب المحفوظة:"), draft.document_title));
            if let Some(document) = selected {
                rtl::label(ui, format!("{} {}", text(a, "Target:", "المستهدف:"), document.title));
            }
        });
        if ui
            .add_enabled(
                available && selected.is_some(),
                egui::Button::new(rtl::widget(ui, text(a, "Prepare saved book continuation", "تجهيز استكمال الكتاب المحفوظ"))),
            )
            .clicked()
            && let Some(document) = selected
        {
            let request = draft.request.clone();
            s.load_production_request(&request);
            s.new_document = false;
            s.continue_book = true;
            *activate_document = Some(document.index);
        }
        if selected.is_none() {
            label(ui, a, "Open and select the saved book with remaining blank pages.", "افتح الكتاب المحفوظ واختره مع وجود صفحات متبقية فارغة.");
        }
    }
    if let (Some(batch), Some(draft)) = (&s.snapshot.batch, &s.snapshot.production_draft)
        && batch.status == "failed"
        && batch.retryable
        && batch.job_id == draft.id
    {
        label(
            ui,
            a,
            "Retry keeps this task's original settings and cumulative usage. To change the brief, model or limits, prepare Continue and press Produce for a new task.",
            "تحتفظ إعادة المحاولة بإعدادات المهمة الأصلية واستخدامها التراكمي. لتغيير التعليمات أو النموذج أو الحدود، جهّز الاستكمال واضغط إنتاج لبدء مهمة جديدة.",
        );
        let unpublished_new_book = matches!(draft.request.target, TaskTarget::NewA4 { .. })
            && batch.completed_pages == 0
            && batch.document_uid.is_none()
            && draft.document_uid.is_none();
        let target_matches = unpublished_new_book
            || documents.iter().any(|document| {
                Some(document.index) == *activate_document && Some(document.uid) == batch.document_uid && Some(document.uid) == draft.document_uid
            });
        if ui
            .add_enabled(
                available && target_matches,
                egui::Button::new(rtl::widget(ui, text(a, "Retry failed review group", "إعادة محاولة مجموعة المراجعة المتعثرة"))),
            )
            .clicked()
        {
            requests.push(AiRequest::RetryBatch { job_id: batch.job_id.clone() });
        }
    }
}

fn production(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    available: bool,
    documents: &[OpenDocument],
    activate_document: &mut Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    production_recovery(ui, s, a, available, documents, activate_document, requests);
    if s.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof") {
        batch_review(ui, s, a, requests);
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Production request", "تعليمات الإنتاج"))).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("ai_review_request").max_height(140.0).show(ui, |ui| {
                rtl::label(ui, &s.prompt);
            });
        });
        return;
    }
    ui.add_enabled_ui(!s.task_active(), |ui| {
        if let Some(document) = documents.iter().find(|document| Some(document.index) == *activate_document && document.continuable) {
            ui.horizontal_wrapped(|ui| {
                let label = if a { format!("استكمال الصفحات {} المتبقية", document.remaining_pages) } else { format!("Continue remaining {} pages", document.remaining_pages) };
                if ui.button(rtl::widget(ui, &label)).clicked() {
                    s.new_document = false;
                    s.continue_book = true;
                    s.restored_target_unconfirmed = false;
                }
                rtl::label(ui, &document.title);
            });
        }
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
        if let Some((_, models, _, _)) = s.local_probe.as_ref().filter(|(selection, models, _, success)| {
            *success && !models.is_empty() && selection.provider == s.settings.text_model.provider && selection.endpoint == s.settings.text_model.endpoint
        }) {
            egui::ComboBox::from_id_salt("production_discovered_models").selected_text(text(a, "Available models", "النماذج المتاحة")).show_ui(
                ui,
                |ui| {
                    for name in models {
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
        rtl::label(
            ui,
            format!(
                "{} {} / {} UTF-8 bytes",
                text(a, "Prompt length:", "طول التعليمات:"),
                s.prompt.len(),
                designcraft_ai_types::protocol::MAX_BRIEF_BYTES
            ),
        );
        if s.prompt.len() > 32768 {
            rtl::label(
                ui,
                format!(
                    "{} {} {}",
                    text(a, "Conservative input budget reservation starts at", "يبدأ حجز ميزانية الإدخال التحفظي من"),
                    s.prompt.len().saturating_add(1024),
                    text(
                        a,
                        "tokens; skill/document context is additional. Actual usage comes from the provider.",
                        "توكن؛ يضاف سياق المهارات والمستند. الاستخدام الفعلي يبلغه المزود."
                    )
                ),
            );
        }
        ui.horizontal_wrapped(|ui| {
            if ui.radio(!s.new_document && !s.continue_book, rtl::widget(ui, text(a, "Edit current document", "تحرير المستند الحالي"))).clicked() {
                s.new_document = false;
                s.continue_book = false;
                s.restored_target_unconfirmed = false;
            }
            if ui
                .radio(s.new_document, rtl::widget(ui, text(a, "New document (style size; A4 default)", "مستند جديد (مقاس النمط؛ الافتراضي A4)")))
                .clicked()
            {
                s.new_document = true;
                s.continue_book = false;
                s.restored_target_unconfirmed = false;
            }
            if ui.radio(s.continue_book, rtl::widget(ui, text(a, "Continue remaining book pages", "استكمال صفحات الكتاب المتبقية"))).clicked()
            {
                s.new_document = false;
                s.continue_book = true;
                s.restored_target_unconfirmed = false;
            }
            if s.new_document {
                ui.add(egui::DragValue::new(&mut s.pages).range(1..=1000).suffix(" pages"));
            }
            label(ui, a, "Pages per review", "الصفحات لكل مراجعة");
            ui.add_enabled(s.new_document || s.continue_book, egui::DragValue::new(&mut s.settings.pages_per_batch).range(1..=1000));
            if let Some(document) = preserved_document_geometry(s, documents, *activate_document) {
                let mut actual = document.facing_pages;
                ui.add_enabled(false, egui::Checkbox::new(&mut actual, rtl::widget(ui, text(a, "Facing pages", "صفحات متقابلة"))));
            } else {
                ui.checkbox(&mut s.settings.facing_pages, rtl::widget(ui, text(a, "Facing pages", "صفحات متقابلة")));
            }
        });
        production_binding(ui, s, a, documents, *activate_document);
        if !s.new_document {
            ui.horizontal_wrapped(|ui| {
                label(ui, a, "Target document", "المستند المستهدف");
                let selected = documents.iter().find(|document| Some(document.index) == *activate_document);
                egui::ComboBox::from_id_salt("ai_target_document")
                    .selected_text(selected.map(|document| document.title.as_str()).unwrap_or(text(a, "Open a document", "افتح مستنداً")))
                    .show_ui(ui, |ui| {
                        for document in documents {
                            if ui.selectable_value(activate_document, Some(document.index), &document.title).clicked() {
                                s.restored_target_unconfirmed = false;
                            }
                        }
                    });
                if let Some(selected) = selected {
                    rtl::label(ui, format!("{} {}", selected.pages, text(a, "pages", "صفحة")));
                }
            });
            if !s.continue_book {
                label(
                    ui,
                    a,
                    "Edits existing content in one task. Choose Continue remaining book pages for production in review groups.",
                    "يحرر المحتوى الموجود بمهمة واحدة. اختر استكمال صفحات الكتاب المتبقية للإنتاج في مجموعات مراجعة.",
                );
            }
        }
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
            "Your review page count stays unchanged. Internal requests build this group, then stop for your approval before the next review group.",
            "يبقى عدد صفحات مراجعتك كما اخترته. تُنفّذ المجموعة بطلبات داخلية، ثم تتوقف لاعتمادك قبل مجموعة المراجعة التالية.",
        );
        if let Some(estimate) = s.request_estimate(documents, *activate_document) {
            if estimate.remaining_pages > estimate.review_pages {
                rtl::label(ui, format!("{}: {} · {} {} · {} {}", text(a, "Remaining book requests (estimate, before repairs)", "طلبات بقية الكتاب (تقدير قبل الإصلاحات)"),
                    estimate.book_allowance, estimate.remaining_pages, text(a, "pages", "صفحة"), estimate.remaining_groups, text(a, "review groups", "مجموعات مراجعة")));
                if s.settings.budget.max_requests < estimate.book_allowance && ui.add_enabled(estimate.book_allowance <= MAX_REQUEST_ALLOWANCE,
                    egui::Button::new(rtl::widget(ui, text(a, "Set request allowance for remaining book", "ضبط حصة الطلبات لبقية الكتاب")))).clicked() {
                    s.settings.budget.max_requests = estimate.book_allowance;
                }
            }
            rtl::label(
                ui,
                format!(
                    "{}: {}{} {} + {} {}{} = {}",
                    if estimate.scripted {
                        text(a, "Requests for this review (estimate)", "طلبات هذه المراجعة (تقدير)")
                    } else {
                        text(a, "Requests for this review (conservative estimate)", "طلبات هذه المراجعة (تقدير تحفظي)")
                    },
                    if estimate.scripted { "" } else { text(a, "up to ", "حتى ") },
                    estimate.review_pages,
                    text(a, "text", "للنص"),
                    estimate.image_allowance,
                    text(a, "images", "للصور"),
                    if estimate.vision_allowance > 0 {
                        format!(" + {} {}", estimate.vision_allowance, text(a, "vision checks", "لفحص الصور"))
                    } else {
                        String::new()
                    },
                    estimate.allowance()
                ),
            );
            label(
                ui,
                a,
                if estimate.scripted {
                    "One text request per PAGE; image allowance is provisional. Repairs and later reviews share the task limit. Token and cost limits still apply."
                } else {
                    "Conservative planning allowance: up to one text request per page before repairs. Internal grouping and cache reuse can reduce actual requests; image counts are provisional. Repairs and later reviews share the task limit. Token and cost limits still apply."
                },
                if estimate.scripted {
                    "طلب نص لكل PAGE؛ حصة الصور تقديرية. تشترك الإصلاحات والمراجعات التالية في حد المهمة. تبقى حدود التوكن والكلفة سارية."
                } else {
                    "حصة تخطيط تحفظية: حتى طلب نص لكل صفحة قبل الإصلاحات. قد يقل العدد الفعلي بالتجميع الداخلي وإعادة استخدام المخزن؛ وعدد الصور تقديري. تشترك الإصلاحات والمراجعات التالية في حد المهمة. تبقى حدود التوكن والكلفة سارية."
                },
            );
            if estimate.vision_allowance > 0 {
                label(
                    ui,
                    a,
                    "Image inspection adds one billed text request per uncached generated source; this estimate assumes no cached reuse.",
                    "يضيف فحص الصور طلب نص مدفوعاً لكل مصدر مولد غير مخزن؛ يفترض هذا التقدير عدم إعادة استخدام صور مخزنة.",
                );
            }
            if s.settings.budget.max_requests < estimate.review_pages {
                rtl::label(
                    ui,
                    format!(
                        "{} {} {} {}.",
                        text(a, "Configured request limit", "حد الطلبات المحدد"),
                        s.settings.budget.max_requests,
                        if estimate.scripted {
                            text(a, "is below the minimum text requests for this review:", "أقل من الحد الأدنى لطلبات نص هذه المراجعة:")
                        } else {
                            text(a, "is below the conservative planning allowance for this review:", "أقل من حصة التخطيط التحفظية لهذه المراجعة:")
                        },
                        estimate.review_pages
                    ),
                );
            }
            if s.settings.budget.max_requests < estimate.allowance() {
                if ui
                    .add_enabled(
                        estimate.allowance() <= MAX_REQUEST_ALLOWANCE,
                        egui::Button::new(rtl::widget(ui, text(a, "Set request allowance for this review", "ضبط حصة الطلبات لهذه المراجعة"))),
                    )
                    .clicked()
                {
                    s.settings.budget.max_requests = estimate.allowance();
                }
                if estimate.allowance() > MAX_REQUEST_ALLOWANCE {
                    label(ui, a, "This provisional estimate exceeds the 10,000-request task ceiling.", "يتجاوز هذا التقدير الأولي سقف المهمة البالغ ١٠٬٠٠٠ طلب.");
                }
            }
        }
        let continuation_issue = (s.continue_book
            && !documents.iter().any(|document| Some(document.index) == *activate_document && document.continuable))
        .then_some(text(
            a,
            "Select a book with completed opening pages and remaining blank pages, or choose Edit current document.",
            "اختر كتاباً فيه صفحات أولى مكتملة ثم صفحات فارغة، أو اختر تحرير المستند الحالي.",
        ));
        let issue = s.production_issue(available, activate_document.is_some(), a).or(continuation_issue);
        if let Some(reason) = issue {
            rtl::label(ui, reason);
        }
        if ui.add_enabled(issue.is_none(), egui::Button::new(rtl::widget(ui, text(a, "Produce", "إنتاج")))).clicked() {
            requests.push(AiRequest::Start(s.production_task()));
        }
        if (s.new_document || s.continue_book) && ui.add_enabled(available && !s.prompt.trim().is_empty(),
            egui::Button::new(rtl::widget(ui,text(a,"Preview curriculum locally (no API request)","معاينة خطة المنهج محلياً (دون طلب API)")))).clicked() {
            requests.push(AiRequest::PreviewProduction(s.production_task()));
        }
        if let Some(preview)=&s.snapshot.production_preview {
            egui::CollapsingHeader::new(rtl::widget(ui,text(a,"Last local curriculum preview","آخر معاينة محلية لخطة المنهج"))).default_open(true).show(ui,|ui| {
                rtl::label(ui,format!("{} · {} · {} · {} {}",preview["blueprint"]["subject_domain"].as_str().unwrap_or(""),
                    preview["style"].as_str().unwrap_or(""),preview["binding"].as_str().unwrap_or(""),preview["total_pages"],text(a,"pages","صفحة")));
                label(ui,a,"Suggested family sequence only; refresh after changing the request. Educator review is required.",
                    "تسلسل عائلات مقترح؛ أعد المعاينة بعد تغيير الطلب. يلزم فحص تربوي بشري.");
                egui::ScrollArea::vertical().id_salt("ai_curriculum_summary").max_height(100.0).show(ui,|ui| {
                    for page in preview["blueprint"]["scoped_page_plan"].as_array().into_iter().flatten().take(32) {
                        if let Some(number)=page["page"].as_u64() {
                            rtl::label(ui,format!("{} {}: {}",text(a,"Page","الصفحة"),number.saturating_add(1),page["recommended_family"].as_str().unwrap_or("")));
                        }
                    }
                });
            });
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
                image_inspection_setting(ui, s, a);
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
            export_print(ui, s, a, available && activate_document.is_some(), requests);
        });
    });
    ui.separator();
    if s.snapshot.batch.as_ref().is_some_and(|batch| matches!(batch.status.as_str(), "running" | "awaiting_human_proof")) {
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

fn proof_image(
    ui: &mut egui::Ui,
    a: bool,
    texture: Option<&egui::TextureHandle>,
    error: &str,
    title: &str,
    size: egui::Vec2,
) -> Option<egui::Response> {
    if let Some(texture) = texture {
        Some(ui.add(egui::Image::new(texture).fit_to_exact_size(size).alt_text(title).sense(egui::Sense::click())))
    } else {
        label(ui, a, "Page preview unavailable", "معاينة الصفحة غير متاحة");
        if !error.is_empty() {
            egui::ScrollArea::vertical().id_salt("ai_proof_error").max_height((size.y - 30.0).max(40.0)).show(ui, |ui| {
                rtl::label(ui, error);
            });
        }
        None
    }
}

fn batch_review(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
    let can_review = s.can_review_selected_proof();
    let pair_visible = s.companion_proof_path().is_some();
    let can_approve = s.can_approve_batch();
    let pages: Vec<_> = s.proof_key.as_ref().map_or_else(Vec::new, |key| key.pages().collect());
    let page_count = pages.len();
    let reviewed_count = pages.iter().filter(|page| s.reviewed_proofs.contains(**page)).count();
    let selected = pages.iter().position(|(path, _)| s.proof_path.as_deref() == Some(path.as_str()));
    let selected_reviewed = selected.and_then(|index| pages.get(index)).is_some_and(|page| s.reviewed_proofs.contains(*page));
    let mut select_page = None;
    let mut confirm_page = false;
    let mut confirm_pair = false;
    let can_review_pair = s.can_review_pair();
    let Some(batch) = &s.snapshot.batch else {
        return;
    };
    ui.separator();
    label(ui, a, "Page proof review", "مراجعة معاينات الصفحات");
    rtl::label(
        ui,
        format!(
            "{} {} / {} · {} {} / {} · {}",
            text(a, "Review group", "مجموعة المراجعة"),
            batch.current_batch,
            batch.total_batches,
            text(a, "Pages", "الصفحات"),
            batch.completed_pages,
            batch.total_pages,
            batch.status
        ),
    );
    if let Some(current) = batch.batches.iter().find(|report| report.batch == batch.current_batch) {
        egui::ScrollArea::horizontal().id_salt("ai_review_page_strip").max_height(30.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (index, page) in pages.iter().enumerate() {
                    let number = u64::from(current.page_start).saturating_add(index as u64).saturating_add(1);
                    let title =
                        format!("{} {number}{}", text(a, "Review page", "مراجعة الصفحة"), if s.reviewed_proofs.contains(*page) { " ✓" } else { "" });
                    if ui.selectable_label(selected == Some(index), rtl::widget(ui, title)).clicked() {
                        select_page = Some(page.0.clone());
                    }
                }
            });
        });
        let pair = selected
            .and_then(|index| current.page_start.checked_add(u32::try_from(index).ok()?))
            .filter(|_| s.settings.facing_pages && s.proof_pair_view)
            .and_then(|page| {
                facing_proof_pair(page, s.proof_binding.or(s.settings.right_to_left_binding).unwrap_or_else(|| automatic_proof_binding(&s.prompt, a)))
            });
        let height = (ui.available_height() - 88.0).clamp(300.0, 480.0);
        let action_width = (ui.available_width() * 0.36).clamp(190.0, 255.0);
        let image_width = (ui.available_width() - action_width - ui.spacing().item_spacing.x).max(100.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(image_width, height), egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.set_width(image_width);
                ui.set_min_height(height);
                egui::ScrollArea::both().id_salt("ai_proof_zoom_canvas").auto_shrink([false, false]).max_width(image_width).max_height(height).show(
                    ui,
                    |ui| {
                        if let Some(pair) = pair {
                            let page_width = if s.proof_full_resolution {
                                s.proof_texture.as_ref().map_or(image_width, |texture| texture.size_vec2().x)
                            } else {
                                ((image_width - ui.spacing().item_spacing.x) * 0.5).max(40.0)
                            };
                            ui.allocate_ui_with_layout(egui::vec2(image_width, height), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                                for page in pair {
                                    ui.push_id(("facing_proof_page", page), |ui| {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(page_width, height),
                                            egui::Layout::top_down(egui::Align::Center),
                                            |ui| {
                                                ui.set_width(page_width);
                                                let page_number = page.saturating_add(1);
                                                let proof = page
                                                    .checked_sub(current.page_start)
                                                    .and_then(|index| usize::try_from(index).ok())
                                                    .and_then(|index| pages.get(index));
                                                if let Some(proof) = proof {
                                                    let (path, _) = proof;
                                                    let is_selected = s.proof_path.as_deref() == Some(path.as_str());
                                                    let title = format!(
                                                        "{} {}{}{}",
                                                        text(a, "Page", "الصفحة"),
                                                        page_number,
                                                        if is_selected { text(a, " · Selected", " · محددة") } else { "" },
                                                        if s.reviewed_proofs.contains(*proof) {
                                                            text(a, " · Reviewed", " · تمت المراجعة")
                                                        } else {
                                                            ""
                                                        }
                                                    );
                                                    if ui.selectable_label(is_selected, rtl::widget(ui, title)).clicked() {
                                                        select_page = Some(path.clone());
                                                    }
                                                    let (texture, error) = if is_selected {
                                                        (s.proof_texture.as_ref(), s.proof_error.as_str())
                                                    } else if let Some(companion) = &s.companion_proof
                                                        && companion.path.as_str() == path.as_str()
                                                    {
                                                        (companion.texture.as_ref(), companion.error.as_str())
                                                    } else {
                                                        (None, "")
                                                    };
                                                    let alt =
                                                        format!("{} {} {}", text(a, "Page", "الصفحة"), page_number, text(a, "proof", "للمراجعة"));
                                                    let size = if s.proof_full_resolution {
                                                        texture.map_or(egui::vec2(page_width, height), |texture| texture.size_vec2())
                                                    } else {
                                                        egui::vec2(page_width, height - 48.0)
                                                    };
                                                    if proof_image(ui, a, texture, error, &alt, size).is_some_and(|response| response.clicked()) {
                                                        select_page = Some(path.clone());
                                                    }
                                                } else if page < batch.total_pages {
                                                    rtl::label(ui, format!("{} {}", text(a, "Page", "الصفحة"), page_number));
                                                    label(ui, a, "Outside this review group", "خارج مجموعة المراجعة الحالية");
                                                } else {
                                                    label(ui, a, "End of book", "نهاية الكتاب");
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        } else {
                            let _ = proof_image(
                                ui,
                                a,
                                s.proof_texture.as_ref(),
                                &s.proof_error,
                                text(a, "Selected page proof", "معاينة الصفحة المحددة"),
                                if s.proof_full_resolution {
                                    s.proof_texture.as_ref().map_or(egui::vec2(image_width, height), |texture| texture.size_vec2())
                                } else {
                                    egui::vec2(image_width, height)
                                },
                            );
                        }
                    },
                );
            });
            ui.allocate_ui_with_layout(egui::vec2(action_width, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(action_width);
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                rtl::label(ui, format!("{} {} / {}", text(a, "Pages reviewed:", "الصفحات المراجعة:"), reviewed_count, page_count));
                if s.settings.facing_pages {
                    if ui.checkbox(&mut s.proof_pair_view, rtl::widget(ui, text(a, "Show facing pair", "عرض الصفحتين المتقابلتين"))).changed()
                    {
                        s.review_pointer_press = None;
                    }
                    if ui.checkbox(&mut s.proof_full_resolution, rtl::widget(ui, text(a, "100% image preview", "معاينة الصورة بحجم 100%"))).changed()
                    {
                        s.review_pointer_press = None;
                    }
                }
                if let Some(index) = selected {
                    rtl::label(
                        ui,
                        format!(
                            "{} {} / {} · {} {}",
                            text(a, "Preview", "المعاينة"),
                            index.saturating_add(1),
                            page_count,
                            text(a, "Document page", "صفحة المستند"),
                            current.page_start.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)).saturating_add(1)
                        ),
                    );
                    ui.horizontal(|ui| {
                        if ui.add_enabled(index > 0, egui::Button::new(rtl::widget(ui, text(a, "Previous", "السابقة")))).clicked() {
                            select_page = index.checked_sub(1).and_then(|previous| pages.get(previous)).map(|(path, _)| path.clone());
                        }
                        if ui
                            .add_enabled(index.saturating_add(1) < page_count, egui::Button::new(rtl::widget(ui, text(a, "Next", "التالية"))))
                            .clicked()
                        {
                            select_page = pages.get(index.saturating_add(1)).map(|(path, _)| path.clone());
                        }
                    });
                    egui::ComboBox::from_id_salt("ai_proof_page")
                        .selected_text(format!(
                            "{} {}",
                            text(a, "Page", "الصفحة"),
                            u64::from(current.page_start).saturating_add(index as u64).saturating_add(1)
                        ))
                        .show_ui(ui, |ui| {
                            for (page_index, page) in pages.iter().enumerate() {
                                let (path, _) = page;
                                let reviewed = s.reviewed_proofs.contains(*page);
                                let title = format!(
                                    "{} {}{}",
                                    text(a, "Page", "الصفحة"),
                                    u64::from(current.page_start).saturating_add(page_index as u64).saturating_add(1),
                                    if reviewed { text(a, " · Reviewed", " · تمت المراجعة") } else { "" }
                                );
                                if ui.selectable_label(page_index == index, rtl::widget(ui, title)).clicked() {
                                    select_page = Some(path.clone());
                                }
                            }
                        });
                }
                if batch.status == "awaiting_human_proof" {
                    if pair_visible {
                        ui.push_id(("ai_review_pair", s.proof_path.as_deref()), |ui| {
                            let response = ui.add_enabled(
                                can_review_pair,
                                egui::Button::new(rtl::widget(
                                    ui,
                                    text(a, "I reviewed both displayed pages; next preview", "راجعت الصفحتين المعروضتين؛ المعاينة التالية"),
                                ))
                                .wrap(),
                            );
                            confirm_pair = AiUiState::fresh_review_gesture(
                                s.review_input_is_human,
                                s.proof_key.as_ref(),
                                &mut s.review_pointer_press,
                                &response,
                            );
                        });
                    }
                    let review_label = if selected_reviewed {
                        text(a, "Reviewed", "تمت المراجعة")
                    } else {
                        text(a, "I reviewed the displayed page", "أؤكد مراجعة الصفحة المعروضة")
                    };
                    // A held press on one page must never confirm a different preview.
                    ui.push_id(("ai_review_page", s.proof_path.as_deref()), |ui| {
                        let response = ui.add_enabled(can_review && !selected_reviewed, egui::Button::new(rtl::widget(ui, review_label)).wrap());
                        confirm_page =
                            AiUiState::fresh_review_gesture(s.review_input_is_human, s.proof_key.as_ref(), &mut s.review_pointer_press, &response);
                    });
                    ui.separator();
                    let (en, ar) = if batch.current_batch >= batch.total_batches {
                        ("Approve final proof and finish", "اعتماد المراجعة النهائية وإنهاء المهمة")
                    } else {
                        ("Approve and generate next review group", "اعتماد وإنشاء مجموعة المراجعة التالية")
                    };
                    let response = ui.add_enabled(can_approve, egui::Button::new(rtl::widget(ui, text(a, en, ar))).wrap());
                    if AiUiState::fresh_review_gesture(s.review_input_is_human, s.proof_key.as_ref(), &mut s.review_pointer_press, &response) {
                        s.batch_approval_gesture = s.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
                        requests.push(AiRequest::ApproveBatch { job_id: batch.job_id.clone(), checkpoint: batch.checkpoint });
                    }
                    if page_count == 0 {
                        label(ui, a, "Verified previews for every group page are required.", "يلزم توفر صور موثقة لجميع صفحات المجموعة.");
                    } else if !s.proof_document_matches {
                        label(
                            ui,
                            a,
                            "Return to the proof's document revision. Changes require a new proof.",
                            "ارجع إلى إصدار المستند المطابق للبروفة. التعديلات تتطلب بروفة جديدة.",
                        );
                    } else if reviewed_count < page_count {
                        label(ui, a, "Confirm each displayed page to enable approval.", "أكد مراجعة كل صفحة معروضة لتفعيل الاعتماد.");
                    }
                }
            });
        });
    } else {
        label(ui, a, "Current group usage and proof have not yet been reported.", "لم تُبلغ بيانات استخدام المجموعة الحالية ومعاينتها بعد.");
    }
    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Batch details", "تفاصيل الدفعات"))).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("ai_batch_details").max_height(160.0).show(ui, |ui| {
            rtl::label(
                ui,
                format!(
                    "{} / checkpoint {} / document {:?} / revision {:?}",
                    batch.job_id, batch.checkpoint, batch.document_uid, batch.document_revision
                ),
            );
            if let Some(style) = s
                .snapshot
                .reports
                .iter()
                .rev()
                .find(|report| report.id.starts_with(&format!("{}-batch-", batch.job_id)))
                .and_then(|report| report.design_style.as_ref())
            {
                rtl::label(ui, format!("{}: {} · {}", text(a, "Pinned design style", "نمط التصميم المثبّت"), style.name, style.revision));
            }
            label(ui, a, "Cumulative task usage", "الاستخدام التراكمي للمهمة");
            usage_line(ui, a, &batch.cumulative_usage, batch.elapsed_ms);
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
    });
    if confirm_page {
        s.review_selected_proof();
        ui.ctx().request_repaint();
    }
    if confirm_pair {
        s.review_displayed_pair();
        ui.ctx().request_repaint();
    }
    if let Some(path) = select_page
        && s.select_proof(&path)
    {
        ui.ctx().request_repaint();
    }
}

fn image_inspection_setting(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
    let supported_provider = s.settings.text_model.provider == ProviderKind::Gemini;
    ui.add_enabled(
        supported_provider,
        egui::Checkbox::new(
            &mut s.settings.verify_generated_images,
            rtl::widget(
                ui,
                text(
                    a,
                    "Inspect generated images with Gemini (extra billed vision requests)",
                    "فحص الصور المولدة مع Gemini (طلبات رؤية إضافية مدفوعة)",
                ),
            ),
        ),
    );
    label(
        ui,
        a,
        "Optional advisory inspection; human proof review is still required. Requires a supported Gemini text model, validated before use.",
        "فحص استشاري اختياري؛ تبقى مراجعة البروفة البشرية مطلوبة. يتطلب نموذج نص Gemini مدعوماً، ويُتحقق من دعمه قبل الاستخدام.",
    );
    if !supported_provider && s.settings.verify_generated_images && button(ui, a, "Turn off image inspection", "إيقاف فحص الصور") {
        s.settings.verify_generated_images = false;
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
        ui.add(egui::DragValue::new(&mut b.max_requests).range(1..=MAX_REQUEST_ALLOWANCE));
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
            field(ui, a, "Export skill ZIP path", "مسار تصدير المهارة ZIP", &mut s.skill_export_path);
            if ui
                .add_enabled(!s.skill_export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export skill", "تصدير المهارة"))))
                .clicked()
            {
                requests.push(AiRequest::ExportSkill { id: r.id, revision: r.revision, path: s.skill_export_path.clone() });
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

fn load_companion_proof(app: &mut DesignApp, ctx: &egui::Context) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use designcraft_engine::Session;
    use egui_kittest::kittest::{NodeT, Queryable};
    use serde_json::json;

    #[test]
    fn ai23_remaining_book_allowance_is_explicit_and_does_not_change_review_size() {
        let mut state = AiUiState { prompt: "Create a science book".into(), pages: 48, ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.pages_per_batch = 2;
        state.settings.output_mode = OutputMode::NativeWithImages;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1600.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 96);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
        assert_eq!(panel.state().pages, 48);
    }

    #[test]
    fn ai23_curriculum_preview_uses_the_injected_host_without_starting_production() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                ai: Some(Box::new(move |_, _| {
                    seen.set(seen.get() + 1);
                    AiSnapshot { available: true, message: "local preview".into(), ..Default::default() }
                })),
                ..Default::default()
            },
        );
        let request = TaskRequest {
            prompt: "Science book".into(),
            settings: AiSettings::default(),
            target: TaskTarget::NewA4 { pages: 48 },
            skill_ids: vec![],
            auto_select_skills: true,
            export_directory: None,
        };
        let result = request_command(&mut app, &json!({"method":"preview_production","request":request}));
        assert!(result.is_ok(), "Local curriculum preview should be available: {result:?}");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn ai23_proof_page_strip_navigates_without_reviewing_or_approving() {
        let fixture = ProofFixture::new();
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Review page 2").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert_eq!(calls.get(), 0);
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    }

    #[test]
    fn ai23_spread_confirmation_requires_both_current_previews_and_separate_approval() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("I reviewed both displayed pages; next preview").accesskit_node().is_disabled());
        panel.get_by_label("100% image preview").click();
        panel.run_steps(3);
        panel.get_by_label("I reviewed both displayed pages; next preview").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-1.png"));
        assert_eq!(calls.get(), 0, "spread review never launches the next group");
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    }

    #[test]
    fn ai20_ui_completed_discovery_stays_selectable_for_its_provider_and_endpoint() {
        let mut state = AiUiState { initialized: true, ..Default::default() };
        state.settings.text_model =
            ModelSelection { provider: ProviderKind::Gemini, model: "original".into(), endpoint: Some("https://models.example".into()) };
        let selection = state.settings.text_model.clone();
        state.pending_models = Some(selection.clone());
        state.accept(AiSnapshot { available: true, busy: true, stage: "discovering".into(), ..Default::default() });
        state.accept(AiSnapshot { available: true, stage: "completed".into(), models: vec!["text-a".into(), "text-b".into()], ..Default::default() });
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
        panel.run_steps(2);
        panel.get_by_label("text-a").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.text_model.model, "text-a");
        panel.state_mut().accept(AiSnapshot { available: true, stage: "completed".into(), ..Default::default() });
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
        panel.run_steps(2);
        panel.get_by_label("text-b").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.text_model.model, "text-b");
        panel.state_mut().settings.text_model.endpoint = Some("https://other.example".into());
        panel.run_steps(3);
        assert!(
            panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none()
        );
        panel.state_mut().settings.text_model.endpoint = selection.endpoint;
        panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
        panel.run_steps(3);
        assert!(
            panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none()
        );
    }

    #[test]
    fn ai20_ui_natural_book_estimates_preserve_review_size_remaining_pages_and_explicit_budget() {
        let mut state = AiUiState { prompt: "Create a clear science book for children.".into(), pages: 12, ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.pages_per_batch = 6;
        let estimate = state.request_estimate(&[], None).expect("natural book briefs need a conservative planning allowance");
        assert_eq!(estimate.review_pages, 6);
        assert_eq!(estimate.allowance(), 6);
        let original_tokens = state.settings.budget.token_limit;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_none());
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 6);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().pages, 12);
        assert_eq!(panel.state().settings.budget.token_limit, original_tokens);

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":48,"facingPages":false})).unwrap();
        for spread in 0..12 {
            session.execute("frame.create", &json!({"spread":spread,"rect":[20,20,100,80]})).unwrap();
        }
        let document = OpenDocument::from_document(3, session.active().unwrap());
        let mut continuing =
            AiUiState { prompt: "Finish the remaining science lessons.".into(), new_document: false, continue_book: true, ..Default::default() };
        continuing.settings.pages_per_batch = 40;
        continuing.settings.output_mode = OutputMode::NativeWithImages;
        continuing.settings.verify_generated_images = true;
        let estimate = continuing.request_estimate(&[document], Some(3)).unwrap();
        assert_eq!(estimate.review_pages, 36);
        assert_eq!(estimate.image_allowance, 36);
        assert_eq!(estimate.vision_allowance, 36);
        assert_eq!(estimate.allowance(), 108);
        assert_eq!(continuing.settings.pages_per_batch, 40);
        assert_eq!(continuing.settings.budget.max_requests, 4);
        continuing.continue_book = false;
        assert!(continuing.request_estimate(&[], None).is_none(), "ordinary edits do not use book planning estimates");
    }

    #[test]
    fn ai20_ui_binding_choices_are_visible_and_existing_book_binding_is_authoritative() {
        let mut state = AiUiState { prompt: "Create a bilingual facing-page book.".into(), ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.facing_pages = true;
        state.settings.right_to_left_binding = Some(false);
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1400.0))
            .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
        panel.run_steps(3);
        assert!(panel.query_by_label("Left to right (LTR)").is_some());
        assert!(
            panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_none()
        );
        for (label, expected) in [("Automatic (content language)", None), ("Right to left (RTL)", Some(true)), ("Left to right (LTR)", Some(false))] {
            panel.get_by_label(label).click();
            panel.run_steps(3);
            assert_eq!(panel.state().0.settings.right_to_left_binding, expected);
            panel.get_by_label("Produce").click();
            panel.run_steps(3);
            assert!(
                matches!(panel.state().1.last(), Some(AiRequest::Start(request)) if request.settings.right_to_left_binding == expected && request.settings.facing_pages)
            );
        }

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4,"facingPages":true})).unwrap();
        session.execute("layout.documentSetup", &json!({"binding":"rightToLeft"})).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
        let documents = vec![OpenDocument::from_document(0, session.active().unwrap())];
        let mut state = AiUiState { prompt: "Continue this existing book.".into(), new_document: false, continue_book: true, ..Default::default() };
        state.settings.right_to_left_binding = Some(false);
        let mut existing = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1400.0))
            .build_ui_state(move |ui, state| production(ui, state, false, true, &documents, &mut Some(0), &mut Vec::new()), state);
        existing.run_steps(3);
        assert!(existing.query_by_label("Existing book binding: Right to left (RTL)").is_some());
        assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
        assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
        assert!(existing.query_by_label("Automatic (content language)").is_none());
        assert_eq!(
            existing.state().settings.right_to_left_binding,
            Some(false),
            "viewing the existing binding does not rewrite new-book preferences"
        );
        assert!(!existing.state().settings.facing_pages, "displaying preserved geometry does not rewrite new-book preferences");
        existing.state_mut().continue_book = false;
        existing.run_steps(3);
        assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
        assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
    }

    #[test]
    fn ai20_ui_document_and_skill_export_destinations_are_independent_and_session_only() {
        let mut state = AiUiState { export_path: "book.pdf".into(), ..Default::default() };
        state.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
            id: "fixture-skill".into(),
            revision: "revision-one".into(),
            skill_md: String::new(),
            manifest_json: String::new(),
            sample_path: None,
            sample_valid: false,
            files: Vec::new(),
        });
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1800.0)).build_ui_state(
            |ui, (state, requests, skill_tab)| {
                if *skill_tab {
                    skills(ui, state, false, true, false, requests);
                } else {
                    export_print(ui, state, false, true, requests);
                }
            },
            (state, Vec::new(), true),
        );
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("skill-package.zip")).focus();
        panel.run_steps(1);
        panel.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        panel.event(egui::Event::Text("portable.zip".into()));
        panel.run_steps(3);
        assert_eq!(panel.state().0.export_path, "book.pdf");
        panel.get_by_label("Export skill").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportSkill { path, .. }) if path == "portable.zip"));
        panel.state_mut().2 = false;
        panel.run_steps(3);
        panel.get_by_label("Export IDML").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportIdml { path }) if path == "book.idml"));
        panel.state_mut().2 = true;
        panel.run_steps(3);
        assert!(panel.query_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("portable.zip")).is_some());
        let ui = crate::UiState { ai: panel.state().0.clone(), ..Default::default() };
        let saved = serde_json::to_string(&ui).unwrap();
        assert!(!saved.contains("portable.zip") && !saved.contains("book.idml"));
    }

    fn continuation_request(target: TaskTarget) -> TaskRequest {
        let mut settings = AiSettings::default();
        settings.text_model.model = "continuation-fixture".into();
        settings.pages_per_batch = 3;
        TaskRequest {
            prompt: "Keep the approved opening pages and finish the original book.".into(),
            settings,
            skill_ids: vec!["original-book-style".into()],
            auto_select_skills: false,
            target,
            export_directory: Some("book-output".into()),
        }
    }

    fn saved_production_snapshot(request: &TaskRequest, document: &designcraft_engine::DocState) -> AiSnapshot {
        let mut snapshot = serde_json::to_value(AiSnapshot::default()).unwrap();
        snapshot["available"] = json!(true);
        snapshot["production_draft"] = json!({
            "id": "saved-book", "request": request, "document_uid": document.uid,
            "document_path": document.path, "document_title": document.title()
        });
        serde_json::from_value(snapshot).unwrap()
    }

    #[test]
    fn continuation_ui_control_start_populates_the_composer_without_serializing_secrets() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        let request = continuation_request(TaskTarget::ContinueBook);
        app.ui.ai.api_key.0 = "private-key-fixture".into();
        app.run("ai.request", json!({"method":"start", "request": request})).unwrap();
        assert_eq!(app.ui.ai.prompt, request.prompt);
        assert!(!app.ui.ai.new_document);
        assert!(app.ui.ai.continue_book);
        assert_eq!(app.ui.ai.settings.pages_per_batch, 3);
        assert!(!app.ui.ai.automatic_skills);
        assert!(app.ui.ai.selected_skills.contains("original-book-style"));
        assert_eq!(app.ui.ai.directory, "book-output");
        let saved_ui = serde_json::to_string(&app.ui).unwrap();
        assert!(!saved_ui.contains("private-key-fixture"));
        assert!(!saved_ui.contains(&request.prompt));
        assert!(app.ui.ai.reviewed_proofs.is_empty());
        assert!(app.ui.ai.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_saved_request_hydrates_once_and_keeps_subsequent_edits() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        let snapshot = saved_production_snapshot(&request, session.active().unwrap());
        let mut state = AiUiState::default();
        state.accept(snapshot.clone());
        assert_eq!(state.prompt, request.prompt);
        assert_eq!(state.settings.pages_per_batch, 3);
        assert_eq!(state.pages, 4);
        state.prompt = "A manual revision awaiting Produce".into();
        state.accept(snapshot);
        assert_eq!(state.prompt, "A manual revision awaiting Produce");
        assert!(state.reviewed_proofs.is_empty());
        assert!(state.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_explicit_action_keeps_current_document_edits_separate() {
        let mut state = AiUiState { new_document: false, ..Default::default() };
        state.prompt = "Edit the selected document".into();
        state.settings.text_model.model = "fixture".into();
        let documents = vec![OpenDocument {
            index: 0,
            uid: 1,
            path: None,
            title: "Book".into(),
            facing_pages: false,
            right_to_left_binding: false,
            pages: 4,
            remaining_pages: 3,
            continuable: true,
        }];
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), Some(0)),
        );
        panel.run_steps(3);
        panel.get_by_label("Continue remaining 3 pages").click();
        panel.run_steps(3);
        assert!(panel.state().1.is_empty(), "choosing continuation is not permission to start production");
        assert_eq!(panel.state().2, Some(0));
        assert_eq!(panel.state().0.prompt, "Edit the selected document");
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::ContinueBook, .. }))));
        panel.get_by_label("Edit current document").click();
        panel.run_steps(3);
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::CurrentDocument, .. }))));
    }

    #[test]
    fn continuation_ui_busy_one_shot_keeps_failed_book_in_previous_task() {
        let state = AiUiState {
            snapshot: AiSnapshot { busy: true, batch: Some(BatchSnapshot { status: "failed".into(), ..Default::default() }), ..Default::default() },
            ..Default::default()
        };
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(panel.query_by_label("Previous task").is_some());
        assert!(panel.query_by_label("Page proof review").is_none(), "a failed old book is not the running one-shot task");
    }

    #[test]
    fn continuation_ui_saved_book_preparation_requires_a_fresh_produce() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        app.run("file.new", json!({"pages":4,"facingPages":false})).unwrap();
        app.run("frame.create", json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        app.ui.ai.accept(saved_production_snapshot(&request, app.session.active().unwrap()));
        app.ui.ai.open = true;
        let starts = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_starts = starts.clone();
        let snapshot = app.ui.ai.snapshot.clone();
        app.services.ai = Some(Box::new(move |_, request| {
            if matches!(request, AiRequest::Start(_)) {
                host_starts.set(host_starts.get() + 1);
            }
            snapshot.clone()
        }));
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 1200.0)).build_ui_state(|ui, app| show(app, ui.ctx()), app);
        panel.run_steps(3);
        panel.get_by_label("Prepare saved book continuation").click();
        panel.run_steps(3);
        assert_eq!(starts.get(), 0);
        assert!(panel.state().ui.ai.continue_book);
        assert!(!panel.state().ui.ai.new_document);
        assert_eq!(panel.state().ui.ai.prompt, request.prompt);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn continuation_ui_proof_document_navigation_does_not_review_or_approve() {
        let mut fixture = ProofFixture::new();
        let proof_uid = fixture.app.session.active().unwrap().uid;
        fixture.app.run("file.new", json!({"pages":2})).unwrap();
        assert_ne!(fixture.app.session.active().unwrap().uid, proof_uid);
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("Open proof document").click();
        panel.run_steps(3);
        assert_eq!(panel.state().session.active().unwrap().uid, proof_uid);
        assert!(panel.state().ui.ai.proof_document_matches);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert!(!panel.state().ui.ai.can_approve_batch());
        assert_eq!(calls.get(), 0, "document navigation must not call the AI host");
    }

    #[test]
    fn continuation_ui_saved_path_precedes_reused_uid_and_restored_targets_require_choice() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::ContinueBook);
        let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["production_draft"]["document_path"] = json!("original-book.designcraft");
        snapshot = serde_json::from_value(value).unwrap();
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let mut wrong = OpenDocument::from_document(0, session.active().unwrap());
        wrong.path = Some("different-book.designcraft".into());
        wrong.continuable = true;
        let documents = [wrong];
        assert!(saved_book_document(state.snapshot.production_draft.as_ref().unwrap(), &documents, Some(0)).is_none());
        assert_eq!(
            state.production_issue(true, true, false),
            Some("Choose the target book or prepare its remaining pages before producing the saved request.")
        );
    }

    #[test]
    fn continuation_ui_retry_requires_live_retryability_and_exact_selected_document() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let document = session.active().unwrap();
        let request = continuation_request(TaskTarget::ContinueBook);
        let mut snapshot = saved_production_snapshot(&request, document);
        snapshot.batch = Some(BatchSnapshot {
            job_id: "saved-book".into(),
            status: "failed".into(),
            retryable: true,
            document_uid: Some(document.uid),
            document_revision: Some(document.revision),
            ..Default::default()
        });
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let documents = vec![OpenDocument::from_document(0, document)];
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
            |ui, (state, requests, selected, documents)| production(ui, state, false, true, documents, selected, requests),
            (state, Vec::new(), Some(0), documents),
        );
        panel.run_steps(3);
        assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.get_by_label("Retry failed review group").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
        let batch = panel.state_mut().0.snapshot.batch.as_mut().unwrap();
        batch.document_revision = batch.document_revision.map(|revision| revision + 1);
        panel.run_steps(3);
        assert!(
            !panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
            "view revisions do not change the selected book's identity"
        );
        panel.state_mut().3.first_mut().unwrap().uid += 1;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "a different book cannot retry this task");
        panel.state_mut().2 = None;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().retryable = false;
        panel.run_steps(3);
        assert!(panel.query_by_label("Retry failed review group").is_none(), "a persisted failure never grants retry authority");
        assert!(panel.state().0.reviewed_proofs.is_empty());
        assert!(panel.state().0.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_unpublished_new_book_retry_does_not_require_an_open_document() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
        snapshot.production_draft.as_mut().unwrap().document_uid = None;
        snapshot.batch = Some(BatchSnapshot { job_id: "saved-book".into(), status: "failed".into(), retryable: true, ..Default::default() });
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
        panel.run_steps(3);
        assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.get_by_label("Retry failed review group").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
        panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::ContinueBook;
        panel.run_steps(3);
        assert!(
            panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
            "an existing book still needs an exact selected document"
        );
        panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::NewA4 { pages: 4 };
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().completed_pages = 1;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "published pages cannot use document-free retry");
    }

    #[test]
    fn continuation_ui_view_only_revision_requests_one_repin_and_preserves_exact_proof_review() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        let original = fixture.app.session.active().unwrap().doc.clone();
        let pinned = original.clone();
        let mut checkpoint = fixture.checkpoint.clone();
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_polls = polls.clone();
        fixture.app.services.ai = Some(Box::new(move |session, request| {
            assert!(matches!(request, AiRequest::Poll), "revision validation cannot approve or start work");
            host_polls.set(host_polls.get() + 1);
            let current = session.active().unwrap();
            if std::sync::Arc::ptr_eq(&current.doc, &pinned) {
                checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
            }
            checkpoint.clone()
        }));
        let mut panel = fixture.panel();
        panel.run_steps(3);
        let reviewed = panel.state().ui.ai.reviewed_proofs.clone();
        let proof_path = panel.state().ui.ai.proof_path.clone();
        assert_eq!(reviewed.len(), 2);
        for (expected_polls, command, params) in [(1, "selection.set", json!({"ids":[]})), (2, "layout.selectPage", json!({"page":1}))] {
            let before = panel.state().session.active().unwrap().revision;
            panel.state_mut().run(command, params).unwrap();
            assert_ne!(panel.state().session.active().unwrap().revision, before);
            assert!(std::sync::Arc::ptr_eq(&panel.state().session.active().unwrap().doc, &original));
            panel.run_steps(5);
            assert_eq!(polls.get(), expected_polls, "view-only revision changes request one local validation");
            assert_eq!(panel.state().ui.ai.reviewed_proofs, reviewed);
            assert_eq!(panel.state().ui.ai.proof_path, proof_path);
            assert!(panel.state().ui.ai.can_approve_batch());
            assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
        }
        panel.state_mut().run("frame.create", json!({"spread":0,"rect":[20,20,80,80]})).unwrap();
        panel.run_steps(5);
        assert_eq!(polls.get(), 3);
        assert!(!panel.state().ui.ai.proof_document_matches);
        assert!(!panel.state().ui.ai.can_approve_batch(), "an actual content edit cannot be repinned by the host");
        panel.run_steps(5);
        assert_eq!(polls.get(), 3, "a rejected content revision must not trigger repeated validation polls");
        assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_changed_view_can_return_to_its_book_before_revision_validation() {
        let mut fixture = ProofFixture::new();
        let original = fixture.app.session.active().unwrap().clone();
        let uid = original.uid;
        let mut checkpoint = fixture.checkpoint.clone();
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_polls = polls.clone();
        fixture.app.services.ai = Some(Box::new(move |session, request| {
            assert!(matches!(request, AiRequest::Poll));
            host_polls.set(host_polls.get() + 1);
            let current = session.active().unwrap();
            if current.uid == uid && std::sync::Arc::ptr_eq(&current.doc, &original.doc) {
                checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
            }
            checkpoint.clone()
        }));
        fixture.app.run("selection.set", json!({"ids":[]})).unwrap();
        fixture.app.run("file.new", json!({"pages":2})).unwrap();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert_eq!(polls.get(), 0);
        panel.get_by_label("Open proof document").click();
        panel.run_steps(5);
        assert_eq!(panel.state().session.active().unwrap().uid, uid);
        assert_eq!(polls.get(), 1);
        assert!(panel.state().ui.ai.proof_document_matches);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert!(!panel.state().ui.ai.can_approve_batch(), "returning to the book never reviews its pages");
        assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
    }
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
    fn explicit_page_review_estimate_changes_request_allowance_only_after_the_button() {
        let mut state = AiUiState { prompt: (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect(), ..Default::default() };
        state.pages = 8;
        state.settings.pages_per_batch = 6;
        state.settings.text_model.model = "fixture-model".into();
        let token_limit = state.settings.budget.token_limit;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 1100.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 0 images = 6").is_some());
        assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 4, "an estimate never raises the spending allowance itself");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 6);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().pages, 8);
        assert_eq!(panel.state().settings.budget.token_limit, token_limit);

        panel.state_mut().settings.output_mode = OutputMode::NativeWithImages;
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images = 12").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 6, "provisional image allowance requires another explicit choice");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 12);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
        panel.state_mut().settings.verify_generated_images = true;
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images + 6 vision checks = 18").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 12, "opting into inspection still does not raise request allowance implicitly");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 18);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().settings.budget.token_limit, token_limit);
    }

    #[test]
    fn image_inspection_requires_explicit_gemini_choice_and_can_be_disabled_after_provider_change() {
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 400.0))
            .build_ui_state(|ui, state| image_inspection_setting(ui, state, false), AiUiState::default());
        let label = "Inspect generated images with Gemini (extra billed vision requests)";
        panel.run_steps(3);
        assert!(panel.get_by_label(label).accesskit_node().is_disabled());
        assert!(!panel.state().settings.verify_generated_images);
        panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
        panel.run_steps(3);
        assert!(!panel.get_by_label(label).accesskit_node().is_disabled());
        panel.get_by_label(label).click();
        panel.run_steps(3);
        assert!(panel.state().settings.verify_generated_images);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
        panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
        panel.run_steps(3);
        assert!(panel.get_by_label(label).accesskit_node().is_disabled());
        assert!(panel.state().settings.verify_generated_images, "provider changes do not silently change an explicit inspection choice");
        panel.get_by_label("Turn off image inspection").click();
        panel.run_steps(3);
        assert!(!panel.state().settings.verify_generated_images);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
    }

    #[test]
    fn continuation_request_estimate_counts_only_remaining_pages_and_ignores_fenced_examples() {
        let prompt = format!(
            "```markdown\n### PAGE 99 — Example only\n```\n{}",
            (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect::<String>()
        );
        let mut state = AiUiState { prompt, new_document: false, continue_book: true, ..Default::default() };
        state.settings.pages_per_batch = 6;
        state.settings.output_mode = OutputMode::NativeWithImages;
        let document = OpenDocument {
            index: 3,
            uid: 1,
            path: None,
            title: "Book".into(),
            facing_pages: false,
            right_to_left_binding: false,
            pages: 8,
            remaining_pages: 2,
            continuable: true,
        };
        let estimate = state.request_estimate(&[document], Some(3)).unwrap();
        assert_eq!(estimate.review_pages, 2);
        assert_eq!(estimate.image_allowance, 2);
        assert_eq!(state.settings.budget.max_requests, 4);
    }

    #[test]
    fn facing_page_choice_preserves_automatic_binding_without_another_direction_prompt() {
        let state = AiUiState::default();
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 1100.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(!panel.state().settings.facing_pages);
        panel.get_by_label("Facing pages").click();
        panel.run_steps(3);
        assert!(panel.state().settings.facing_pages);
        assert_eq!(panel.state().settings.right_to_left_binding, None);
        assert!(
            panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_some()
        );
    }
    #[test]
    fn facing_book_continuation_counts_blank_pages_inside_a_partly_filled_pair() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":5,"facingPages":true})).unwrap();
        let add_page_item = |session: &mut Session, absolute| {
            let document = session.doc().unwrap();
            let (spread, page) = document.doc.page_loc(absolute).unwrap();
            let x = document.doc.spreads[spread].pages[page].x;
            session.execute("frame.create", &json!({"spread":spread,"rect":[x+20.0,20.0,x+100.0,80.0]})).unwrap();
        };
        add_page_item(&mut session, 0);
        add_page_item(&mut session, 1);
        assert!(session.doc().unwrap().doc.spreads.iter().any(|spread| spread.pages.len() == 2));
        let book = OpenDocument::from_document(0, session.doc().unwrap());
        assert!(book.continuable);
        assert_eq!(book.pages, 5);
        assert_eq!(book.remaining_pages, 3, "the blank right page of the first pair is part of the suffix");
        add_page_item(&mut session, 3);
        assert!(!OpenDocument::from_document(0, session.doc().unwrap()).continuable, "a later filled page cannot bypass a blank gap");
    }

    #[test]
    fn automatic_proof_binding_uses_print_content_metadata_before_instruction_language() {
        assert!(!automatic_proof_binding("صمّم كتاباً للأطفال\nContent Copy Language: English", true));
        assert!(automatic_proof_binding("Script Language: English\nContent Copy Language: English LTR + Vocalized Arabic RTL", false));
        assert!(!automatic_proof_binding("تعليمات عربية: محتوى الكتاب باللغة الإنجليزية", true));
        assert!(automatic_proof_binding("Binding: RTL\nContent Copy Language: English", false));
    }

    #[test]
    fn long_bilingual_composer_keeps_the_full_brief_and_uses_the_shared_limit() {
        let mut state = AiUiState { prompt: "تعليمات المنهج Mathematics\n".repeat(4000), ..Default::default() };
        state.settings.text_model.model = "fixture-model".into();
        let original = state.prompt.clone();
        assert!(original.len() > 32768);
        assert_eq!(state.production_issue(true, false, false), None);
        assert_eq!(state.prompt, original);
        state.prompt = "x".repeat(designcraft_ai_types::protocol::MAX_BRIEF_BYTES + 1);
        assert!(state.production_issue(true, false, false).is_some());
    }
    #[test]
    fn local_resource_preset_keeps_the_requested_human_review_size() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.ui.ai.open = true;
        app.ui.ai.initialized = true;
        app.ui.ai.snapshot.available = true;
        app.ui.ai.tab = 4;
        app.ui.ai.settings.pages_per_batch = 12;
        let mut harness = crate::test_window::open(app, egui::vec2(1800.0, 1500.0));
        harness.run_steps(3);
        harness.get_by_label("Conservative CPU (Ollama)").click();
        harness.run_steps(3);
        assert_eq!(harness.state().app.ui.ai.settings.pages_per_batch, 12);
        assert_eq!(harness.state().app.ui.ai.settings.resources.cpu_threads, 2);
    }
    #[test]
    fn opened_document_target_is_visible_without_creating_or_resizing_it() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":3,"facingPages":false})).unwrap();
        let mut app = DesignApp::new(session, Services::default());
        app.ui.ai.open = true;
        app.ui.ai.initialized = true;
        app.ui.ai.new_document = false;
        let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
        harness.run_steps(3);
        assert!(harness.query_by_label("Target document").is_some());
        let state = harness.state().app.session.doc().unwrap();
        assert_eq!(state.doc.page_count(), 3);
        assert_eq!(state.doc.settings.page_width, 360.0);
        assert!(!state.is_dirty());
    }

    #[test]
    fn requested_large_review_group_keeps_every_page_in_the_human_proof() {
        let fixture = ProofFixture::new();
        let mut batch = fixture.checkpoint.batch.clone().unwrap();
        batch.total_pages = 64;
        batch.pages_per_batch = 64;
        batch.total_batches = 1;
        batch.completed_pages = 64;
        let report = &mut batch.batches[0];
        report.page_count = 64;
        report.proof_paths = vec!["document.designcraft".into(), "proof-1.pdf".into(), "proof-2.pdf".into()];
        report.proof_paths.extend((1..=64).map(|page| format!("page-{page}.png")));
        report.proof_sha256 = vec!["a".repeat(64); report.proof_paths.len()];
        let key = BatchReviewKey::from_batch(&batch).expect("A complete 64-page review group must not be rejected by an internal 32-page limit");
        assert_eq!(key.pages().count(), 64);
        let report = &mut batch.batches[0];
        report.proof_paths.pop();
        report.proof_sha256.pop();
        assert!(BatchReviewKey::from_batch(&batch).is_none(), "Every requested page still needs its own proof");
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
            app.run("file.new", json!({"facingPages":false})).unwrap();
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

        fn panel(mut self) -> egui_kittest::Harness<'static, DesignApp> {
            self.app.ui.ai.open = true;
            self.app.ui.ai.prompt = "A long production request\n".repeat(1000);
            egui_kittest::Harness::builder().with_size(egui::vec2(1024.0, 768.0)).build_ui_state(|ui, app| show(app, ui.ctx()), self.app)
        }
    }

    #[test]
    fn production_review_keeps_actions_beside_the_proof_and_tracks_each_displayed_page() {
        let fixture = ProofFixture::new();
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        let image = panel.get_by_label("Selected page proof").rect();
        let review = panel.get_by_label("I reviewed the displayed page").rect();
        let approval = panel.get_by_label("Approve and generate next review group").rect();
        assert!(review.min.x >= image.max.x, "page confirmation belongs beside its displayed preview");
        assert!(approval.min.x >= image.max.x, "group approval belongs beside the preview");
        assert!(image.contains(egui::pos2(image.center().x, review.center().y)));
        assert!(image.contains(egui::pos2(image.center().x, approval.center().y)));
        assert!(image.max.y < 768.0 && approval.max.y < 768.0, "long requests and file lists must not require scrolling to approve");
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert!(panel.get_by_label("Previous").accesskit_node().is_disabled());
        assert!(panel.query_by_label("Pages reviewed: 0 / 2").is_some());

        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Reviewed").is_some());
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-1.png".into(), "2".repeat(64))]));
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());

        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
        assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-2.png"));
        assert!(panel.query_by_label("Pages reviewed: 1 / 2").is_some());
        assert!(panel.get_by_label("Next").accesskit_node().is_disabled());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1, "navigation and loading never review the next page");

        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(!panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert_eq!(calls.get(), 0, "per-page review never submits group approval");
    }

    #[test]
    fn busy_production_shows_live_usage_and_keeps_full_diagnostics_in_details() {
        let mut fixture = ProofFixture::new();
        fixture.app.ui.ai.snapshot.busy = true;
        fixture.app.ui.ai.snapshot.current_usage = designcraft_ai_types::protocol::UsageSummary {
            requests: 3,
            input_tokens: 1042,
            output_tokens: 91,
            estimated_cost_usd: Some(0.0123),
            ..Default::default()
        };
        let issue = json!({"page":7,"field":"body","reason":"Native text exceeds frame","solution":"Increase frame height"});
        fixture.app.ui.ai.snapshot.current_issues = vec![issue.clone()];
        fixture.app.ui.ai.error = "Native text exceeds frame\nPage 7 body needs another line.".into();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.query_by_label("Live requests: 3 / 4 · Input 1042 · Output 91 · Estimated $0.012300").is_some());
        assert!(panel.query_by_label("Native text exceeds frame").is_some());
        assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_none());
        let diagnostic = serde_json::to_string_pretty(&issue).unwrap();
        assert!(panel.query_by_label(&diagnostic).is_none());
        panel.get_by_label("Details").click();
        panel.run_steps(3);
        assert!(panel.query_by_label(&diagnostic).is_some());
        assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_some());
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert!(panel.get_by_label("Approve and generate next review group").rect().max.y < 768.0);
    }

    #[test]
    fn facing_proofs_keep_cover_single_order_pairs_and_review_each_selected_page_separately() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.query_by_label("Selected page proof").is_some(), "the first cover remains a single page");
        assert!(panel.query_by_label("Page 2 proof").is_none());
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 2 proof").rect().center().x < panel.get_by_label("Page 3 proof").rect().center().x);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "showing a pair reviews neither page");
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
        panel.get_by_label("Page 3").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-3.png"));
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(
            panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(),
            "the cover still requires its own review"
        );
        panel.state_mut().ui.ai.settings.right_to_left_binding = Some(true);
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2, "changing physical reading order cannot add a review");
        panel.get_by_label("Show facing pair").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Selected page proof").is_some());
        assert!(panel.state().ui.ai.companion_proof.is_none());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        panel.get_by_label("Show facing pair").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        panel.get_by_label("Previous").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Reviewed").accesskit_node().is_disabled());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    }

    #[test]
    fn facing_partner_outside_review_group_is_a_placeholder_without_implied_review() {
        let mut fixture = ProofFixture::new();
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Page 2 proof").is_some());
        assert!(panel.query_by_label("Page 3 proof").is_none());
        assert!(panel.query_by_label("Outside this review group").is_some());
        assert!(panel.state().ui.ai.companion_proof.is_none());
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
        panel.get_by_label("Previous").click();
        panel.run_steps(3);
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(
            !panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(),
            "only actual pages in the current group are required"
        );
    }

    #[test]
    fn facing_proof_prefers_matching_document_binding_and_drops_it_after_revision_change() {
        let mut fixture = ProofFixture::new();
        fixture.app.run("layout.documentSetup", json!({"facingPages":true,"binding":"rightToLeft"})).unwrap();
        let document = fixture.app.session.active().unwrap();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.document_uid = Some(document.uid);
        batch.document_revision = Some(document.revision);
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_binding, Some(true));
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        panel.state_mut().run("layout.documentSetup", json!({"binding":"leftToRight"})).unwrap();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_binding, None);
        assert!(!panel.state().ui.ai.can_review_selected_proof());
        assert!(!panel.state().ui.ai.can_approve_batch());
    }

    #[test]
    fn proof_navigation_rejects_foreign_pages_and_discards_a_press_on_the_previous_page() {
        let mut panel = ProofFixture::new().panel();
        panel.run_steps(3);
        assert!(!panel.state_mut().ui.ai.select_proof("foreign.png"));
        assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-1.png"));
        let pos = panel.get_by_label("I reviewed the displayed page").rect().center();
        panel.event(egui::Event::PointerMoved(pos));
        panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        panel.run_steps(1);
        assert!(panel.state().ui.ai.review_pointer_press.is_some());
        assert!(panel.state_mut().ui.ai.select_proof("page-2.png"));
        assert!(panel.state().ui.ai.proof_texture.is_none());
        assert!(panel.state().ui.ai.review_pointer_press.is_none());
        panel.run_steps(2);
        panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
        panel.run_steps(3);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "releasing a press from another page cannot confirm the new preview");
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
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
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 2, "review evidence stays disabled until the host validates this revision");
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
