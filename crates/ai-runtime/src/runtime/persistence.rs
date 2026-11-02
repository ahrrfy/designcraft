use super::{
    AiSettings, MAX_BRIEF_BYTES, Path, ProductionDraft, ProductionRunner, ProviderKind, Session, SystemTime, TaskRequest, TaskTarget, UNIX_EPOCH,
    UsageLedger, Value, file_access, validate_endpoint,
};

impl ProductionRunner {
    pub(super) fn save_production_draft(&mut self, session: &Session, task: &TaskRequest) -> Result<(), String> {
        let current = match task.target {
            TaskTarget::NewA4 { .. } => None,
            TaskTarget::NewDocument { .. } => session.doc().ok(),
            TaskTarget::CurrentDocument | TaskTarget::ContinueBook => Some(session.doc().map_err(|e| e.to_string())?),
        };
        let completed_pages = current
            .map(|doc| {
                crate::facing_pages::canonicalize(doc, &session.prefs)
                    .map(|canon| u32::try_from(canon.doc.spreads.iter().take_while(|spread| !spread.items.is_empty()).count()).unwrap_or(0))
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let target_total_pages = match task.target {
            TaskTarget::NewA4 { pages } | TaskTarget::NewDocument { pages, .. } => Some(pages),
            TaskTarget::ContinueBook => current.and_then(|doc| u32::try_from(doc.doc.page_count()).ok()),
            TaskTarget::CurrentDocument => None,
        };
        self.sequence = self.sequence.saturating_add(1);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis();
        let draft = ProductionDraft {
            id: format!("draft-{stamp}-{}-{}", std::process::id(), self.sequence),
            request: task.clone(),
            document_uid: current.map(|doc| doc.uid),
            document_path: current.and_then(|doc| doc.path.clone()),
            document_title: current.map(|doc| doc.title()).unwrap_or_default(),
            progress: designcraft_ai_types::protocol::SavedProductionProgress { completed_pages, target_total_pages },
        };
        validate_production_draft(&draft)?;
        self.persist_production_draft(&draft)?;
        self.snapshot.production_draft = Some(draft);
        Ok(())
    }

    pub(super) fn persist_production_draft(&self, draft: &ProductionDraft) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(draft).map_err(|e| e.to_string())?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("Saved production instructions exceed their 2 MiB storage limit".into());
        }
        write_profile(&self.profile_access, &self.root.join("production-draft.json"), &bytes)
    }

    pub(super) fn bind_production_draft(&mut self, session: &Session, id: &str) -> Result<(), String> {
        let Some(mut draft) = self.snapshot.production_draft.clone() else { return Ok(()) };
        let current = session.doc().map_err(|e| e.to_string())?;
        let prior_total = match draft.request.target {
            TaskTarget::NewA4 { pages } | TaskTarget::NewDocument { pages, .. } => Some(pages),
            _ => None,
        };
        let completed_pages = if let Some(job) = self.batch.as_ref().filter(|job| job.snapshot.job_id == id) {
            job.snapshot.completed_pages
        } else {
            crate::facing_pages::canonicalize(current, &session.prefs)
                .map(|canon| {
                    u32::try_from(canon.doc.spreads.iter().take_while(|spread| !spread.items.is_empty()).count())
                        .unwrap_or(draft.progress.completed_pages)
                })
                .unwrap_or(draft.progress.completed_pages)
        };
        draft.id = id.into();
        draft.document_uid = Some(current.uid);
        draft.document_path = current.path.clone();
        draft.document_title = current.title();
        draft.request.target = TaskTarget::ContinueBook;
        draft.progress.completed_pages = completed_pages;
        draft.progress.target_total_pages =
            draft.progress.target_total_pages.or(prior_total).or_else(|| u32::try_from(current.doc.page_count()).ok());
        draft.request.settings.facing_pages |= current.doc.settings.facing_pages;
        if current.doc.settings.facing_pages || current.doc.settings.right_to_left_binding {
            draft.request.settings.right_to_left_binding = Some(current.doc.settings.right_to_left_binding);
        }
        self.persist_production_draft(&draft)?;
        self.snapshot.production_draft = Some(draft);
        Ok(())
    }
}

pub(super) fn validate_settings(settings: &AiSettings) -> Result<(), String> {
    settings.resources.validate()?;
    if matches!(settings.text_model.provider, ProviderKind::Ollama | ProviderKind::LocalOpenAi)
        && settings.budget.max_output_tokens >= settings.resources.context_tokens
    {
        return Err("Ollama output token limit must be smaller than the local context window".into());
    }
    if settings.pages_per_batch > 1000 {
        return Err("Human review groups must contain 1–1000 pages, or zero for a short one-shot task".into());
    }
    validate_endpoint(&settings.text_model)?;
    if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages
        && settings.visual_density != designcraft_ai_types::protocol::VisualDensityMode::PureNativeVector
    {
        validate_endpoint(&settings.image_model)?;
    }
    if let Some(hybrid) = &settings.hybrid_local_model {
        validate_endpoint(hybrid)?;
        if hybrid.model.len() > 256 {
            return Err("Model name is too long".into());
        }
        if !hybrid.provider.is_local() || hybrid.provider == ProviderKind::ComfyUi {
            return Err("SmartHybrid local continuation model must use Ollama or LocalOpenAi".into());
        }
    }
    if settings.active_knowledge_packs.len() > 32 || settings.active_knowledge_packs.iter().any(|id| id.len() > 256) {
        return Err("Too many or oversized active knowledge pack identifiers".into());
    }
    if settings.text_model.model.len() > 256 || settings.image_model.model.len() > 256 {
        return Err("Model name is too long".into());
    }
    if !settings.text_model.model.trim().is_empty() && designcraft_ai_types::is_non_text_model(&settings.text_model.model) {
        return Err("The selected text model is an image/media model; choose a text model for planning".into());
    }
    UsageLedger::new(settings.budget.clone())?;
    if let Some(rates) = &settings.image_rates {
        crate::ledger::validate_rates(rates)?;
    }
    Ok(())
}
pub(super) fn validate_production_draft(draft: &ProductionDraft) -> Result<(), String> {
    validate_settings(&draft.request.settings)?;
    if let TaskTarget::NewDocument { pages, ref geometry } = draft.request.target {
        if !(1..=1000).contains(&pages) {
            return Err("Saved production instructions contain invalid page count".into());
        }
        crate::intent_geometry::validate_geometry_spec(geometry)?;
    }
    for fmt in &draft.request.target_formats {
        crate::intent_geometry::validate_geometry_spec(fmt)?;
    }
    if draft.id.len() > 256
        || draft.request.prompt.trim().is_empty()
        || draft.request.prompt.len() > MAX_BRIEF_BYTES
        || draft.document_title.len() > 4096
        || draft.document_path.as_ref().is_some_and(|path| path.len() > 32_768)
        || draft.request.export_directory.as_ref().is_some_and(|path| path.len() > 32_768)
        || draft.request.skill_ids.len() > 8
        || draft.request.skill_ids.iter().any(|id| id.len() > 256)
        || draft.request.attachments.len() > 32
        || draft.request.attachments.iter().any(|a| a.path.len() > 32_768)
        || draft.request.knowledge_pack_ids.len() > 32
        || draft.request.knowledge_pack_ids.iter().any(|id| id.len() > 256)
        || draft.request.target_formats.len() > 16
        || matches!(draft.request.target, TaskTarget::NewA4 { pages } if !(1..=1000).contains(&pages))
        || draft.progress.completed_pages > 1000
        || draft.progress.target_total_pages.is_some_and(|pages| !(1..=1000).contains(&pages))
    {
        return Err("Saved production instructions contain invalid or oversized fields".into());
    }
    Ok(())
}
pub(super) fn apply_execution_settings_to_draft(draft: &mut ProductionDraft, settings: &AiSettings) {
    let is_bound_book = draft.document_uid.is_some() || draft.document_path.is_some() || matches!(draft.request.target, TaskTarget::ContinueBook);
    if is_bound_book {
        draft.request.settings.budget = settings.budget.clone();
        draft.request.settings.text_model = settings.text_model.clone();
        draft.request.settings.image_model = settings.image_model.clone();
        draft.request.settings.output_mode = settings.output_mode;
        draft.request.settings.visual_density = settings.visual_density;
        draft.request.settings.execution_pacing = settings.execution_pacing;
        draft.request.settings.execution_mode = settings.execution_mode;
        draft.request.settings.hybrid_local_model = settings.hybrid_local_model.clone();
        draft.request.settings.multi_format_mode = settings.multi_format_mode;
        draft.request.settings.active_knowledge_packs = settings.active_knowledge_packs.clone();
        draft.request.settings.verify_generated_images = settings.verify_generated_images;
        draft.request.settings.allow_paid_repairs = settings.allow_paid_repairs;
        draft.request.settings.image_rates = settings.image_rates.clone();
        draft.request.settings.comfy_workflow = settings.comfy_workflow.clone();
        draft.request.settings.resources = settings.resources.clone();
    } else {
        draft.request.settings = settings.clone();
    }
}
pub(crate) fn enforce_execution_mode(settings: &mut AiSettings) -> Result<(), String> {
    use designcraft_ai_types::protocol::AiExecutionMode;
    match settings.execution_mode {
        AiExecutionMode::Auto => Ok(()),
        AiExecutionMode::LocalOnly => {
            if !settings.text_model.provider.is_local() {
                return Err("LocalOnly execution mode forbids cloud text providers; select Ollama or LocalOpenAi".into());
            }
            if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages
                && settings.visual_density != designcraft_ai_types::protocol::VisualDensityMode::PureNativeVector
                && !settings.image_model.provider.is_local()
            {
                return Err("LocalOnly execution mode forbids cloud image providers; select ComfyUI or switch Output Mode to Native Only".into());
            }
            if !settings.image_model.provider.is_local() {
                settings.image_model = designcraft_ai_types::ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: None };
                settings.comfy_workflow = None;
            }
            if !designcraft_ai_types::supports_gemini_image_review(settings.text_model.provider, &settings.text_model.model) {
                settings.verify_generated_images = false;
            }
            Ok(())
        }
        AiExecutionMode::CloudOnly => {
            if settings.text_model.provider.is_local() {
                return Err("CloudOnly execution mode forbids local text endpoints; select Gemini or OpenAI".into());
            }
            if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages
                && settings.visual_density != designcraft_ai_types::protocol::VisualDensityMode::PureNativeVector
                && settings.image_model.provider.is_local()
            {
                return Err("CloudOnly execution mode forbids local image endpoints; select a cloud image provider or Native Only".into());
            }
            Ok(())
        }
        AiExecutionMode::SmartHybrid => {
            if settings.text_model.provider.is_local() {
                return Err("SmartHybrid execution mode expects a cloud text model for Batch 1 foundation planning".into());
            }
            let Some(hybrid) = &settings.hybrid_local_model else {
                return Err("SmartHybrid execution mode requires hybrid_local_model (Ollama or LocalOpenAi) for Batch 2+ continuation".into());
            };
            if !hybrid.provider.is_local() || hybrid.provider == ProviderKind::ComfyUi || hybrid.model.trim().is_empty() {
                return Err("SmartHybrid local continuation model must specify a valid Ollama or LocalOpenAi text model".into());
            }
            validate_endpoint(hybrid)?;
            Ok(())
        }
    }
}
pub(crate) fn require_text_model(settings: &AiSettings) -> Result<(), String> {
    if settings.text_model.model.trim().is_empty() {
        return Err("Choose a text model before starting generation".into());
    }
    if matches!(settings.text_model.provider, ProviderKind::ComfyUi) {
        return Err("ComfyUI is an image provider; choose a text provider for planning".into());
    }
    if designcraft_ai_types::is_non_text_model(&settings.text_model.model) {
        return Err("The selected text model is an image/media model; choose a text model for planning".into());
    }
    Ok(())
}
pub(crate) fn require_generation_models(settings: &AiSettings) -> Result<(), String> {
    if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages
        && settings.visual_density != designcraft_ai_types::protocol::VisualDensityMode::PureNativeVector
    {
        if settings.image_model.provider != ProviderKind::ComfyUi && settings.image_model.model.trim().is_empty() {
            return Err("Choose an image model before starting illustrated generation".into());
        }
        if settings.image_model.provider == ProviderKind::Ollama {
            return Err("Ollama image generation is unsupported; choose an explicit image provider or switch Output Mode to Native Only".into());
        }
        if settings.image_model.provider == ProviderKind::LocalOpenAi {
            return Err(
                "LocalOpenAi image generation is unsupported; choose ComfyUI, a cloud image provider, or switch Output Mode to Native Only".into()
            );
        }
        if !settings.image_model.model.trim().is_empty()
            && designcraft_ai_types::is_non_image_model(settings.image_model.provider, &settings.image_model.model)
        {
            return Err(
                "The selected image model is a text/non-image model; choose a dedicated image generation model (such as gemini-3.1-flash-image or gpt-image-1) or switch Output Mode to Native Only"
                    .into(),
            );
        }
        if settings.verify_generated_images
            && !designcraft_ai_types::supports_gemini_image_review(settings.text_model.provider, &settings.text_model.model)
        {
            return Err(
                "Generated-image inspection requires a supported Gemini text model with image input; no provider or model fallback is enabled".into(),
            );
        }
    }
    Ok(())
}

pub(super) fn read_json(access: &std::sync::Arc<file_access::AutomationRoots>, path: &Path, max: u64) -> Result<Option<Value>, String> {
    access.check(path, file_access::Access::Read)?;
    match file_access::confine(Some(access), || designcraft_engine::io::read_capped(path, max)) {
        Ok(bytes) if bytes.len() as u64 > max => Err(format!("Configuration file exceeds limit: {}", path.display())),
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub(super) fn write_json(access: &file_access::AutomationRoots, path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    write_profile(access, path, &bytes)
}
pub(super) fn write_profile(access: &file_access::AutomationRoots, path: &Path, bytes: &[u8]) -> Result<(), String> {
    access.check(path, file_access::Access::Write)?;
    designcraft_engine::atomic::write_atomic(path, bytes).map_err(|e| e.to_string())
}

pub(crate) fn allocate_knowledge_pack_id(profile_access: &file_access::AutomationRoots, ai_root: &Path, base_slug: &str) -> Result<String, String> {
    let packs_root = ai_root.join("knowledge-packs");
    profile_access.check(&packs_root, file_access::Access::Write)?;
    std::fs::create_dir_all(&packs_root).map_err(|e| e.to_string())?;
    let mut pack_id = base_slug.to_string();
    let mut suffix = 2u32;
    while packs_root.join(&pack_id).exists() && suffix <= 100 {
        pack_id = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    crate::rag::validate_pack_id(&pack_id)?;
    Ok(pack_id)
}

pub(crate) fn persist_knowledge_pack_files(
    profile_access: &file_access::AutomationRoots,
    ai_root: &Path,
    pack_id: &str,
    manifest_bytes: &[u8],
    chunks_bytes: &[u8],
) -> Result<(), String> {
    crate::rag::validate_pack_id(pack_id)?;
    let pack_dir = ai_root.join("knowledge-packs").join(pack_id);
    profile_access.check(&pack_dir, file_access::Access::Write)?;
    std::fs::create_dir_all(&pack_dir).map_err(|e| e.to_string())?;
    write_profile(profile_access, &pack_dir.join("manifest.json"), manifest_bytes)?;
    write_profile(profile_access, &pack_dir.join("chunks.json"), chunks_bytes)?;
    Ok(())
}

pub(crate) fn remove_knowledge_pack_dir(profile_access: &file_access::AutomationRoots, ai_root: &Path, id: &str) -> Result<(), String> {
    crate::rag::validate_pack_id(id)?;
    let pack_dir = ai_root.join("knowledge-packs").join(id);
    profile_access.check(&pack_dir, file_access::Access::Write)?;
    let Ok(meta) = std::fs::symlink_metadata(&pack_dir) else {
        return Err(format!("Knowledge pack '{id}' does not exist"));
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err("Refusing to remove symlinked or invalid knowledge pack path".into());
    }
    std::fs::remove_dir_all(&pack_dir).map_err(|e| e.to_string())
}

pub(crate) fn list_knowledge_pack_dir_ids(profile_access: &file_access::AutomationRoots, ai_root: &Path) -> Result<Vec<String>, String> {
    let packs_root = ai_root.join("knowledge-packs");
    profile_access.check(&packs_root, file_access::Access::Read)?;
    let entries = match std::fs::read_dir(&packs_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut ids = Vec::new();
    for entry in entries.flatten().take(128) {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let Some(id_str) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if crate::rag::validate_pack_id(&id_str).is_ok() {
            ids.push(id_str);
        }
    }
    Ok(ids)
}
