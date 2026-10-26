use crate::vault::{SessionKey, Vault};
use crate::{ledger::UsageLedger, science, worker};
use designcraft_ai_providers::{SecretString, validate_endpoint};
use designcraft_ai_skills::{ApprovalEvidence, Provenance, RevisionStatus, SkillRegistry, SkillSnapshot};
use designcraft_ai_types::protocol::*;
use designcraft_ai_types::{EngineAction, ProviderKind};
use designcraft_engine::{DocState, Prefs, Session, file_access};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, mpsc::TryRecvError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_REPORTS: usize = 100;
#[path = "batch.rs"]
mod batch;
#[path = "output.rs"]
mod output;

struct Pending {
    roots: Option<std::sync::Arc<file_access::AutomationRoots>>,
    worker: worker::Worker,
    id: String,
    started: Instant,
    task: Option<TaskRequest>,
    expected: Option<(u64, u64)>,
    expected_prefs: Option<Prefs>,
    skills: Vec<(String, String)>,
    settings: AiSettings,
}
struct SampleEvidence {
    id: String,
    revision: String,
    actions: Vec<EngineAction>,
    path: PathBuf,
}

/// Native service injected by the desktop host. `handle` never waits on a provider.
pub struct ProductionRunner {
    root: PathBuf,
    profile_access: std::sync::Arc<file_access::AutomationRoots>,
    registry: SkillRegistry,
    snapshot: AiSnapshot,
    keys: Vec<SessionKey>,
    vault: Vault,
    pending: Option<Pending>,
    batch: Option<batch::BatchJob>,
    sample: Option<SampleEvidence>,
    sequence: u64,
    print_attempts: BTreeSet<String>,
    print_contracts: BTreeMap<u64, crate::print_contract::PrintContract>,
    local_server: crate::localserver::LocalOllama,
}

impl Drop for ProductionRunner {
    fn drop(&mut self) {
        if let Some(job) = &self.pending {
            job.worker.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl ProductionRunner {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        // Pin the host's private profile independently of command automation roots.
        // Every profile access resolves links against these canonical boundaries.
        let profile_access = file_access::AutomationRoots::new(Some(&root), Some(&root))?.ok_or("Missing AI profile boundary")?;
        let vault = Vault::for_profile(&root)?;
        let mut registry = SkillRegistry::open(root.join("skills")).map_err(|e| e.to_string())?;
        registry.install_builtin_drafts().map_err(|e| e.to_string())?;
        let mut settings = read_json(&profile_access, &root.join("settings.json"), 1024 * 1024)?
            .map(serde_json::from_value::<AiSettings>)
            .transpose()
            .map_err(|e| format!("AI settings: {e}"))?
            .unwrap_or_default();
        let mut startup_warning = String::new();
        if !settings.text_model.model.trim().is_empty() && designcraft_ai_types::is_non_text_model(&settings.text_model.model) {
            startup_warning = format!(
                "Cleared legacy non-text model '{}' from saved AI text settings; choose a text model for planning",
                settings.text_model.model.trim()
            );
            settings.text_model.model.clear();
        }
        validate_settings(&settings)?;
        let reports = read_json(&profile_access, &root.join("history.json"), 8 * 1024 * 1024)?
            .map(serde_json::from_value::<Vec<TaskReport>>)
            .transpose()
            .map_err(|e| format!("AI history: {e}"))?
            .unwrap_or_default();
        let print_attempts = read_json(&profile_access, &root.join("print-attempts.json"), 1024 * 1024)?
            .map(serde_json::from_value::<BTreeSet<String>>)
            .transpose()
            .map_err(|e| format!("Print attempt history: {e}"))?
            .unwrap_or_default();
        let recovered_batch = batch::recover_snapshot(&profile_access, &root)?;
        let settings_mtime = std::fs::metadata(root.join("settings.json")).and_then(|m| m.modified()).ok();
        let draft_mtime = std::fs::metadata(root.join("production-draft.json")).and_then(|m| m.modified()).ok();
        let (production_draft, mut draft_warning) = match read_json(&profile_access, &root.join("production-draft.json"), 2 * 1024 * 1024)
            .and_then(|value| value.map(serde_json::from_value::<ProductionDraft>).transpose().map_err(|e| e.to_string()))
            .and_then(|mut draft| {
                if let Some(draft) = &mut draft {
                    if !draft.request.settings.text_model.model.trim().is_empty()
                        && designcraft_ai_types::is_non_text_model(&draft.request.settings.text_model.model)
                    {
                        draft.request.settings.text_model = settings.text_model.clone();
                    }
                    validate_production_draft(draft)?;
                }
                Ok(draft)
            }) {
            Ok(mut draft) => {
                if let Some(draft) = &mut draft {
                    // UIDs are process-local. A saved path helps the human choose a book;
                    // neither it nor this draft authorizes publication or paid requests.
                    draft.document_uid = None;
                    if let (Some(s_time), Some(d_time)) = (settings_mtime, draft_mtime)
                        && s_time >= d_time
                    {
                        apply_execution_settings_to_draft(draft, &settings);
                    } else if designcraft_ai_types::is_non_image_model(
                        draft.request.settings.image_model.provider,
                        &draft.request.settings.image_model.model,
                    ) && (settings.image_model.provider == ProviderKind::ComfyUi
                        || (!settings.image_model.model.trim().is_empty()
                            && !designcraft_ai_types::is_non_image_model(settings.image_model.provider, &settings.image_model.model)))
                    {
                        draft.request.settings.image_model = settings.image_model.clone();
                    }
                }
                (draft, String::new())
            }
            Err(error) => (None, format!("Saved production instructions could not be restored: {error}")),
        };
        if !startup_warning.is_empty() {
            if draft_warning.is_empty() {
                draft_warning = startup_warning;
            } else {
                draft_warning = format!("{startup_warning}; {draft_warning}");
            }
        }
        let mut initial_stage = String::new();
        if draft_warning.is_empty()
            && recovered_batch.as_ref().is_some_and(|batch| batch.status == "failed")
            && let Some(last_failed) = reports.last().filter(|report| report.status == "failed" && !report.message.trim().is_empty())
        {
            draft_warning = last_failed.message.clone();
            initial_stage = "failed".into();
        }
        let mut host = Self {
            root,
            profile_access,
            registry,
            snapshot: AiSnapshot {
                available: true,
                stage: initial_stage,
                settings,
                reports,
                production_draft,
                message: draft_warning,
                batch: recovered_batch,
                ..Default::default()
            },
            keys: Vec::new(),
            vault,
            pending: None,
            batch: None,
            sample: None,
            sequence: 0,
            print_attempts,
            print_contracts: BTreeMap::new(),
            local_server: Default::default(),
        };
        if let Some(dir) = std::env::var_os("DESIGNCRAFT_SKILLS_DIR").filter(|val| !val.is_empty())
            && let Err(error) = host.install_portable_catalog(Path::new(&dir))
        {
            host.snapshot.skill_warnings.push(format!("Portable skills catalog warning: {error}"));
        }
        host.refresh_catalog();
        host.refresh_credentials();
        Ok(host)
    }

    /// Install verified portable skill packages from the first discovered `skills/catalog.json`
    /// directory (`DESIGNCRAFT_SKILLS_DIR`, adjacent to executable, or workspace `skills/` in debug
    /// builds). Never searches the untrusted current working directory. Returns `Ok(0)` when absent.
    pub fn install_default_portable_catalog(&mut self) -> Result<usize, String> {
        let mut candidates = Vec::new();
        if let Some(dir) = std::env::var_os("DESIGNCRAFT_SKILLS_DIR").filter(|val| !val.is_empty()) {
            candidates.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            candidates.push(parent.join("skills"));
            if let Some(grandparent) = parent.parent() {
                candidates.push(grandparent.join("skills"));
                if let Some(workspace_root) = grandparent.parent() {
                    candidates.push(workspace_root.join("skills"));
                }
            }
        }
        #[cfg(debug_assertions)]
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills"));
        for dir in candidates {
            let manifest = dir.join("catalog.json");
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir())
                && std::fs::symlink_metadata(&manifest).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
            {
                return self.install_portable_catalog(&dir);
            }
        }
        Ok(0)
    }

    /// Verify every listed file's SHA-256 digest and symlink-free path against `dir/catalog.json`
    /// before importing each package as a draft into the profile's `SkillRegistry`.
    pub fn install_portable_catalog(&mut self, dir: &Path) -> Result<usize, String> {
        let dir_meta = std::fs::symlink_metadata(dir).map_err(|e| format!("skills catalog directory: {e}"))?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err("Portable skills directory must be a non-symlink directory".into());
        }
        let catalog_bytes = read_catalog_file(&dir.join("catalog.json"), 2 * 1024 * 1024)?;
        let manifest_sha = hex_sha256(&catalog_bytes);
        let authentic_catalog = manifest_sha == hex_sha256(EMBEDDED_PORTABLE_CATALOG);
        let provenance_source = if authentic_catalog { "Smart Design portable catalog" } else { "External portable catalog" };
        let manifest: PortableCatalogManifest = serde_json::from_slice(&catalog_bytes).map_err(|e| format!("skills catalog.json: {e}"))?;
        if manifest.schema_version != 1 {
            return Err(format!("Unsupported portable skills catalog schema_version {}", manifest.schema_version));
        }
        if manifest.packages.len() > 256 {
            return Err("Portable skills catalog exceeds the 256-package limit".into());
        }
        let mut imported = 0usize;
        for pkg in manifest.packages {
            let pkg_id = pkg.id.clone();
            let outcome = (|| -> Result<(), String> {
                validate_portable_package_id(&pkg.id)?;
                if pkg.files.is_empty() || pkg.files.len() > 64 {
                    return Err("package must declare 1 to 64 files".into());
                }
                let pkg_dir = dir.join(&pkg.id);
                let pkg_meta = std::fs::symlink_metadata(&pkg_dir).map_err(|e| e.to_string())?;
                if pkg_meta.file_type().is_symlink() || !pkg_meta.is_dir() {
                    return Err("package path must be a non-symlink directory".into());
                }
                let mut files = designcraft_ai_skills::PackageFiles::new();
                let mut total_bytes = 0usize;
                for (rel_path, expected_sha) in &pkg.files {
                    if expected_sha.len() != 64 || !expected_sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                        return Err(format!("invalid SHA-256 digest for {rel_path}"));
                    }
                    let full_path = resolve_portable_package_file(&pkg_dir, rel_path)?;
                    let bytes = read_catalog_file(&full_path, 512 * 1024)?;
                    total_bytes = total_bytes
                        .checked_add(bytes.len())
                        .filter(|sum| *sum <= 4 * 1024 * 1024)
                        .ok_or_else(|| "package exceeds 4-MiB total size limit".to_string())?;
                    let actual_sha = hex_sha256(&bytes);
                    if actual_sha != *expected_sha {
                        return Err(format!("SHA-256 mismatch for {rel_path}"));
                    }
                    files.insert(rel_path.clone(), bytes);
                }
                self.registry
                    .import_files(
                        files,
                        Provenance { source: provenance_source.into(), author: None, license: None, source_sha256: Some(manifest_sha.clone()) },
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            })();
            match outcome {
                Ok(()) => imported = imported.saturating_add(1),
                Err(error) => {
                    if self.snapshot.skill_warnings.len() < 64 {
                        self.snapshot.skill_warnings.push(format!(
                            "Skipped portable skill {}: {}",
                            pkg_id.chars().take(64).collect::<String>(),
                            error.chars().take(512).collect::<String>()
                        ));
                    }
                }
            }
        }
        self.refresh_catalog();
        Ok(imported)
    }

    pub fn handle(&mut self, session: &mut Session, request: AiRequest) -> AiSnapshot {
        // Cancellation must win even when a completed provider response is queued.
        if matches!(request, AiRequest::Cancel)
            && let Some(job) = &self.pending
        {
            job.worker.cancel.store(true, Ordering::Relaxed);
        }
        self.poll(session);
        if let Err(error) = self.refresh_unchanged_proof(session) {
            self.snapshot.message = format!("Unchanged proof could not be refreshed: {error}");
        }
        self.refresh_local_server();
        let failed_job = self.batch.as_ref().is_some_and(|job| job.snapshot.status == "failed") && self.pending.is_none();
        if self.batch.is_some()
            && !failed_job
            && !matches!(
                request,
                AiRequest::Poll
                    | AiRequest::Cancel
                    | AiRequest::ApproveBatch { .. }
                    | AiRequest::RefreshBatchProofs { .. }
                    | AiRequest::InspectSkill { .. }
                    | AiRequest::RefreshCredentials
                    | AiRequest::RefreshResources
                    | AiRequest::ExportSkill { .. }
            )
        {
            self.snapshot.message = "Finish reviewing or cancel the current production batch before another operation".into();
            return self.snapshot.clone();
        }
        if self.snapshot.busy && !matches!(request, AiRequest::Poll | AiRequest::Cancel | AiRequest::InspectSkill { .. }) {
            self.snapshot.message = "An AI task is running; cancel or wait before starting another operation".into();
            return self.snapshot.clone();
        }
        if let Err(error) = self.dispatch(session, request) {
            self.snapshot.message = error;
            if !self.snapshot.busy {
                if self.batch.as_ref().is_some_and(|job| job.snapshot.status == "awaiting_human_proof") {
                    self.snapshot.stage = "awaiting_human_proof".into();
                } else {
                    self.snapshot.stage = "failed".into();
                }
            }
        }
        self.refresh_catalog();
        self.snapshot.clone()
    }

    fn dispatch(&mut self, session: &mut Session, request: AiRequest) -> Result<(), String> {
        match request {
            AiRequest::Poll => {}
            AiRequest::Cancel => {
                if let Some(job) = &self.pending {
                    job.worker.cancel.store(true, Ordering::Relaxed);
                }
                self.cancel_batch();
                self.snapshot.message = "Cancellation requested; a request already sent may still be billed".into();
            }
            AiRequest::ApproveBatch { job_id, checkpoint } => self.approve_batch(session, &job_id, checkpoint)?,
            AiRequest::RefreshBatchProofs { job_id } => self.refresh_batch_proofs(session, &job_id)?,
            AiRequest::RetryBatch { job_id } => self.retry_batch(session, &job_id)?,
            AiRequest::UpdateSettings(settings) => {
                validate_settings(&settings)?;
                let mut next_draft = self.snapshot.production_draft.clone();
                if let Some(draft) = &mut next_draft {
                    apply_execution_settings_to_draft(draft, &settings);
                    validate_production_draft(draft)?;
                }
                let prev_batch = self.batch.clone();
                let prev_snap_batch = self.snapshot.batch.clone();
                self.sync_failed_batch_settings(&settings)?;
                if let Err(error) = write_json(&self.profile_access, &self.root.join("settings.json"), &settings).and_then(|()| {
                    if let Some(draft) = &next_draft {
                        self.persist_production_draft(draft)?;
                    }
                    if let Some(job) = &self.batch
                        && job.snapshot.status == "failed"
                    {
                        self.persist_batch(&job.snapshot)?;
                    }
                    Ok(())
                }) {
                    self.batch = prev_batch;
                    self.snapshot.batch = prev_snap_batch;
                    return Err(error);
                }
                self.snapshot.production_draft = next_draft;
                self.snapshot.settings = settings;
                self.refresh_credentials();
                if self.snapshot.stage == "failed" {
                    self.snapshot.stage = "idle".into();
                }
                self.snapshot.message = "AI settings saved; API keys remain outside settings and reports".into();
            }
            AiRequest::SetApiKey { provider, key } => {
                let selection = self.configured_key_selection(provider)?;
                let secret = SecretString::new(key);
                if !secret.expose().trim().is_empty() {
                    self.store_session_key(&selection, secret)?;
                } else {
                    self.keys.retain(|stored| !stored.matches(&selection));
                }
                self.refresh_credentials();
                self.snapshot.message = "Session credential updated".into();
            }
            AiRequest::SaveApiKey { selection, key } => {
                let secret = SecretString::new(key);
                self.vault.save(&selection, &secret)?;
                self.store_session_key(&selection, secret)?;
                self.refresh_credentials();
                self.snapshot.message = "API key saved in the Windows protected credential store for this endpoint".into();
            }
            AiRequest::RemoveApiKey { selection } => {
                self.vault.remove(&selection)?;
                self.keys.retain(|stored| !stored.matches(&selection));
                self.refresh_credentials();
                self.snapshot.message = "Stored and session API key removed for this endpoint".into();
            }
            AiRequest::RefreshCredentials => {
                self.refresh_credentials();
                self.snapshot.message = "Protected credential presence refreshed; key values are never displayed".into();
            }
            AiRequest::Start(task) => self.start(session, task)?,
            AiRequest::ListModels { selection } => {
                validate_endpoint(&selection)?;
                self.launch(worker::Work::Models(selection), None, None, Vec::new(), self.snapshot.settings.clone())?;
            }
            AiRequest::RefreshResources => {
                self.launch(worker::Work::Resources, None, None, Vec::new(), self.snapshot.settings.clone())?;
                self.refresh_local_server();
            }
            AiRequest::StartLocalOllama { executable } => {
                self.snapshot.settings.resources.validate()?;
                self.local_server.start(Path::new(&executable), &self.snapshot.settings.resources)?;
                self.refresh_local_server();
                self.snapshot.message = "Owned local Ollama server started; use model discovery to verify readiness. No model was downloaded".into();
            }
            AiRequest::StopLocalOllama => {
                self.local_server.stop()?;
                self.refresh_local_server();
                self.snapshot.message = "Stopped only the Ollama server started by this window".into();
            }
            AiRequest::ImportSkill { path } => {
                let source = Path::new(&path);
                file_access::check_read(source)?;
                let provenance = Provenance {
                    source: source.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Local import".into()),
                    ..Default::default()
                };
                let revision =
                    if source.is_dir() { self.registry.import_folder(source, provenance) } else { self.registry.import_zip(source, provenance) }
                        .map_err(|e| e.to_string())?;
                self.inspect_skill(&revision.skill_id, &revision.revision)?;
                self.snapshot.message = "Skill imported as a draft; run and review its sample before approval".into();
            }
            AiRequest::InspectSkill { id, revision } => self.inspect_skill(&id, &revision)?,
            AiRequest::AddDesignCorrection { id, revision, correction } => {
                let latest = self
                    .registry
                    .catalog("")
                    .into_iter()
                    .find(|e| e.skill_id == id)
                    .and_then(|entry| entry.revisions.last().map(|r| r.revision.clone()));
                if latest.as_deref() != Some(revision.as_str()) {
                    return Err("Inspect the latest style revision before adding a correction".into());
                }
                let mut files = self.registry.review_files(&id, &revision).map_err(|e| e.to_string())?;
                let mut manifest = self.registry.revision(&id, &revision).map_err(|e| e.to_string())?.manifest.clone();
                let profile = manifest.design_profile.as_mut().ok_or("This skill has no design profile")?;
                profile.corrections.push(correction);
                profile.validate()?;
                files.insert("designcraft.json".into(), serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?);
                let draft = self
                    .registry
                    .import_files(files, Provenance { source: "Explicit local design correction".into(), ..Default::default() })
                    .map_err(|e| e.to_string())?;
                self.sample = None;
                self.inspect_skill(&id, &draft.revision)?;
                self.snapshot.message = "Correction saved in a new design-style draft; the active version and running task stay unchanged".into();
            }
            AiRequest::PrepareSkills { prompt } => {
                let ids = self.prepare_automatic_skills(session, &prompt)?;
                self.snapshot.message = format!("Automatically prepared {} compatible skills locally; no provider request", ids.len());
            }
            AiRequest::PreviewProduction(task) => self.preview_production(session, task)?,
            AiRequest::SaveSkillDraft { id, skill_md, manifest_json } => {
                let review = self.snapshot.review.as_ref().filter(|r| r.id == id).ok_or("Inspect the skill before editing")?;
                let mut files = self.registry.review_files(&id, &review.revision).map_err(|e| e.to_string())?;
                let manifest: designcraft_ai_skills::NativeManifest =
                    serde_json::from_str(&manifest_json).map_err(|e| format!("Skill manifest: {e}"))?;
                if manifest.skill_id != id {
                    return Err("Editing must preserve the skill ID; create a new skill to use another ID".into());
                }
                files.insert("SKILL.md".into(), skill_md.into_bytes());
                files.insert("designcraft.json".into(), manifest_json.into_bytes());
                let revised = self
                    .registry
                    .import_files(files, Provenance { source: "Local reviewed edit".into(), ..Default::default() })
                    .map_err(|e| e.to_string())?;
                self.sample = None;
                self.inspect_skill(&revised.skill_id, &revised.revision)?;
                self.snapshot.message = "Draft saved; the previous approved revision remains active".into();
            }
            AiRequest::GenerateSkill { prompt, reference_paths, settings, design_style } => {
                self.start_skill(prompt, reference_paths, None, settings, design_style)?
            }
            AiRequest::GenerateSkillFromDocument { prompt, settings, design_style } => {
                let context = if design_style { corrected_design_context(session)? } else { compact_context(session)? };
                self.start_skill(prompt, Vec::new(), Some(context), settings, design_style)?;
            }
            AiRequest::RunSkillSample { id, revision } => self.run_sample(session, &id, &revision)?,
            AiRequest::RunDesignTemplate { id, revision, template } => self.run_design_template(session, &id, &revision, Some(&template))?,
            AiRequest::ApproveSkill { id, revision } => {
                let evidence = self
                    .sample
                    .as_ref()
                    .filter(|s| s.id == id && s.revision == revision)
                    .ok_or("Run and visually review this exact skill revision first")?;
                validate_skill_templates(&self.registry, session, &id, &revision)?;
                self.registry
                    .approve(
                        &id,
                        &revision,
                        ApprovalEvidence {
                            reviewer: "Local user".into(),
                            sample_artifact: evidence.path.clone(),
                            executed_actions: evidence.actions.clone(),
                            supported_commands: supported_commands(),
                            notes: "Explicit user approval after native sample execution and preview".into(),
                        },
                    )
                    .map_err(|e| e.to_string())?;
                self.inspect_skill(&id, &revision)?;
                self.snapshot.message = "Skill approved for future tasks".into();
            }
            AiRequest::SetSkillEnabled { id, enabled } => self.registry.set_enabled(&id, enabled).map_err(|e| e.to_string())?,
            AiRequest::RollbackSkill { id, revision } => self.registry.rollback(&id, &revision).map_err(|e| e.to_string())?,
            AiRequest::ExportSkill { id, revision, path } => {
                file_access::check_write(&path)?;
                if Path::new(&path).extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
                    self.registry.export_zip(&id, &revision, &path)
                } else {
                    self.registry.export_folder(&id, &revision, &path)
                }
                .map_err(|e| e.to_string())?;
                self.snapshot.message = "Skill package exported".into();
            }
            AiRequest::CreateScienceSample { directory } => {
                let artifacts = science::build_science_sample(session, Path::new(&directory))?;
                self.snapshot.message = format!("Original editable science sample exported: {}", artifacts.join("; "));
            }
            AiRequest::ExportDocument { path, pdf } => {
                file_access::check_write(&path)?;
                let contract = self.print_contracts.get(&session.doc().map_err(|e| e.to_string())?.uid).cloned();
                if pdf {
                    ensure_preflight_at(session, contract.as_ref().and_then(|c| c.min_image_ppi()).unwrap_or(150.0))?;
                }
                let params = if pdf {
                    match &contract {
                        Some(contract) => contract.pdf_export_params(Some(&path))?,
                        None => json!({"path":path,"spreads":false,"bleed":false,"marks":false}),
                    }
                } else {
                    json!({"path":path})
                };
                session.execute(if pdf { "file.exportPdf" } else { "file.saveAs" }, &params).map_err(|e| e.to_string())?;
                self.snapshot.message = format!("Exported {path}");
                if pdf
                    && let Some(contract) = contract
                    && let Err(warning) = contract.validate_final_export()
                {
                    self.snapshot.message.push_str(&format!(". Review PDF saved; {warning}"));
                }
            }
            AiRequest::ExportIdml { path } => {
                file_access::check_write(&path)?;
                ensure_preflight(session)?;
                session.execute("file.exportIdml", &json!({"path":path,"embedImages":true})).map_err(|e| e.to_string())?;
                self.snapshot.message = format!("Exported editable IDML: {path}");
            }
            AiRequest::ExportIndd { path } => {
                let prepared = crate::indesign::prepare(session, Path::new(&path))?;
                self.launch(worker::Work::Indesign(prepared), None, None, Vec::new(), self.snapshot.settings.clone())?;
                self.snapshot.stage = "exporting_indd".into();
                self.snapshot.message = "Exporting through installed InDesign; reopening and verifying native content and geometry".into();
            }
            AiRequest::PreparePrint { printer, grayscale, duplex } => self.prepare_print(session, printer, grayscale, duplex)?,
            AiRequest::SubmitPreparedPrint { token, request_id } => self.submit_prepared_print(session, &token, request_id)?,
            AiRequest::SavePreparedPrint { token, path } => self.save_prepared_print(session, &token, &path)?,
            AiRequest::OpenPrintProof { token } => self.open_print_proof(session, &token)?,
            AiRequest::PrintDocument { request_id, .. } => {
                if self.print_attempts.contains(&request_id) {
                    return Err("This print operation was already attempted; check the print queue before creating a new operation".into());
                }
                return Err("Prepare and review the frozen PDF, then submit it with the human print action".into());
            }
        }
        Ok(())
    }

    fn start(&mut self, session: &mut Session, mut task: TaskRequest) -> Result<(), String> {
        if matches!(task.target, TaskTarget::ContinueBook | TaskTarget::CurrentDocument) {
            let current = session.doc().map_err(|e| e.to_string())?;
            task.settings.facing_pages |= current.doc.settings.facing_pages;
            if current.doc.settings.facing_pages || current.doc.settings.right_to_left_binding {
                task.settings.right_to_left_binding = Some(current.doc.settings.right_to_left_binding);
            }
        }
        if let Some(directory) = &task.export_directory {
            file_access::check_write(directory)?;
        }
        validate_settings(&task.settings)?;
        if task.prompt.trim().is_empty() || task.prompt.len() > MAX_BRIEF_BYTES {
            return Err(format!("Enter a nonempty brief of at most {MAX_BRIEF_BYTES} UTF-8 bytes (1 MiB)"));
        }
        self.save_production_draft(session, &task)?;
        require_text_model(&task.settings)?;
        require_generation_models(&task.settings)?;
        let expected = match task.target {
            TaskTarget::CurrentDocument => Some((session.doc().map_err(|e| e.to_string())?.uid, session.doc().map_err(|e| e.to_string())?.revision)),
            TaskTarget::ContinueBook => {
                let current = session.doc().map_err(|e| e.to_string())?;
                if task.settings.pages_per_batch == 0 {
                    return Err("Continuing a book requires reviewed batches".into());
                }
                Some((current.uid, current.revision))
            }
            TaskTarget::NewA4 { pages } => {
                if !(1..=1000).contains(&pages) {
                    return Err("AI new documents support 1–1000 pages".into());
                }
                if task.settings.pages_per_batch == 0 && pages > 8 {
                    return Err("Books over eight pages require reviewed batches".into());
                }
                None
            }
        };
        let ids = if task.auto_select_skills {
            let auto_ids = self.prepare_skills_for_style(session, &task.prompt, task.settings.design_style.as_deref())?;
            let pinned_ids = self.prepare_pinned_skills(session, &task.skill_ids)?;
            merge_pinned_and_automatic_skills(&pinned_ids, auto_ids, task.settings.design_style.as_deref())
        } else {
            self.prepare_selected_skills(session, &task.skill_ids, task.settings.design_style.as_deref())?
        };
        if ids.len() > 8 {
            return Err("Select at most eight skills per task".into());
        }
        let mut snapshots = Vec::new();
        let supported = supported_commands();
        for id in ids {
            let card = self.registry.catalog("").into_iter().find(|e| e.skill_id == id).ok_or_else(|| format!("Unknown skill: {id}"))?;
            let revision = card.active_revision.as_ref().ok_or_else(|| format!("Skill {id} requires approval"))?;
            let record = self.registry.revision(&id, revision).map_err(|e| e.to_string())?;
            if !skill_is_compatible(record, &supported) {
                return Err(format!("Skill {id} is incompatible with the current native engine"));
            }
            let refs = selected_skill_references(record);
            let snapshot = self.registry.snapshot(&id, &refs).map_err(|e| e.to_string())?;
            snapshots.push(snapshot);
        }
        let snapshots = bounded_task_skills(snapshots, &task.target)?;
        let pins = snapshots.iter().map(|s| (s.skill_id.clone(), s.revision.clone())).collect();
        if snapshots.iter().filter(|s| s.design_profile.is_some()).count() > 1 {
            return Err("Choose one design style per task to avoid conflicting identities".into());
        }
        let profile = snapshots.iter().find_map(|s| s.design_profile.as_ref());
        let (base, mut context) = task_document_context(session, &task.target, profile, &task.prompt, &task.settings)?;
        enrich_production_context(&mut context, &self.root, &base, &task)?;
        let print_contract = crate::print_contract::PrintContract::from_prompt(&task.prompt)?;
        if print_contract.is_some() && self.print_contracts.len() >= 1024 {
            return Err("Too many document print contracts in this application session; save open work and restart before creating another".into());
        }
        if let TaskTarget::ContinueBook = task.target {
            batch::continuation_progress(&base, profile, &task.prompt)?;
        }
        if matches!(task.target, TaskTarget::NewA4 { .. })
            && print_contract.is_none()
            && let Some(profile) = profile
        {
            crate::design::check_geometry(Some(&base), &base, profile)?;
        }
        let settings = task.settings.clone();
        if self.batch.as_ref().is_some_and(|job| job.snapshot.status == "failed") {
            // An explicit new Produce may replace a failed task; Retry instead keeps
            // its original pinned skills and cumulative ledger.
            self.cancel_batch();
        }
        if matches!(task.target, TaskTarget::ContinueBook) || matches!(task.target, TaskTarget::NewA4 { .. } if settings.pages_per_batch > 0) {
            return self.start_batches(session, task, snapshots, base, context);
        }
        let work = worker::Work::Production {
            request: task.clone(),
            context,
            catalog: designcraft_engine::cmd::production::catalog(),
            skills: snapshots,
            base,
            prefs: session.prefs.clone(),
        };
        self.launch(work, Some(task), expected, pins, settings)
    }

    fn preview_production(&mut self, session: &Session, task: TaskRequest) -> Result<(), String> {
        self.snapshot.production_preview = None;
        if task.prompt.trim().is_empty() || task.prompt.len() > MAX_BRIEF_BYTES {
            return Err("Enter valid production instructions before previewing the curriculum".into());
        }
        match task.target {
            TaskTarget::NewA4 { pages } if (1..=1000).contains(&pages) => {}
            TaskTarget::ContinueBook => {}
            _ => return Err("Curriculum preview requires a new 1–1000-page book or an existing book to continue".into()),
        }
        let ids = if task.auto_select_skills {
            let auto_ids = self.prepare_skills_for_style(session, &task.prompt, task.settings.design_style.as_deref())?;
            let pinned_ids = self.prepare_pinned_skills(session, &task.skill_ids)?;
            merge_pinned_and_automatic_skills(&pinned_ids, auto_ids, task.settings.design_style.as_deref())
        } else {
            self.prepare_selected_skills(session, &task.skill_ids, task.settings.design_style.as_deref())?
        };
        if ids.len() > 8 {
            return Err("Select at most eight skills per task".into());
        }
        let mut styles = Vec::new();
        for id in ids {
            let revision = self
                .registry
                .catalog("")
                .into_iter()
                .find(|entry| entry.skill_id == id)
                .and_then(|entry| entry.active_revision)
                .ok_or("Preview requires a compatible active skill revision")?;
            let skill = self.registry.snapshot(&id, &[]).map_err(|error| error.to_string())?;
            if skill.revision != revision {
                return Err("Skill revision changed while preparing preview".into());
            }
            if skill.design_profile.is_some() {
                styles.push(skill);
            }
        }
        if styles.is_empty()
            && task.auto_select_skills
            && task.settings.design_style.is_none()
            && let Some(fallback_id) = self.default_preview_style_id()
        {
            self.prepare_style(session, &fallback_id)?;
            styles.push(self.registry.snapshot(&fallback_id, &[]).map_err(|error| error.to_string())?);
        }
        if styles.len() != 1 {
            return Err("Choose one compatible design style to preview its curriculum".into());
        }
        let style = styles.first().ok_or("Missing preview style")?;
        let profile = style.design_profile.as_ref().ok_or("Missing preview profile")?;
        let (base, _) = task_document_context(session, &task.target, Some(profile), &task.prompt, &task.settings)?;
        let total = u32::try_from(base.doc.page_count()).map_err(|_| "Preview page count exceeds its limit")?;
        let start =
            if matches!(task.target, TaskTarget::ContinueBook) { batch::continuation_progress(&base, Some(profile), &task.prompt)?.1 } else { 0 };
        let remaining = total.checked_sub(start).ok_or("Invalid preview range")?;
        let count = remaining.min(task.settings.pages_per_batch.max(1)).min(32);
        let blueprint = crate::template_plan::curriculum_blueprint(&task.prompt, style, worker::BatchScope { start, count }, total)?;
        let rtl = if matches!(task.target, TaskTarget::ContinueBook) {
            base.doc.settings.right_to_left_binding
        } else {
            book_binding(&task.settings, &task.prompt)
        };
        self.snapshot.production_preview = Some(json!({"style":profile.name,"revision":style.revision,"total_pages":total,
            "binding":if rtl {"RTL"} else {"LTR"},"preview_pages":count,"blueprint":blueprint}));
        self.snapshot.message = "Local curriculum preview only; no provider request, document edit or production approval".into();
        Ok(())
    }

    fn save_production_draft(&mut self, session: &Session, task: &TaskRequest) -> Result<(), String> {
        let current = if matches!(task.target, TaskTarget::NewA4 { .. }) { None } else { Some(session.doc().map_err(|e| e.to_string())?) };
        let completed_pages = current
            .map(|doc| {
                crate::facing_pages::canonicalize(doc, &session.prefs)
                    .map(|canon| u32::try_from(canon.doc.spreads.iter().take_while(|spread| !spread.items.is_empty()).count()).unwrap_or(0))
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let target_total_pages = match task.target {
            TaskTarget::NewA4 { pages } => Some(pages),
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

    fn persist_production_draft(&self, draft: &ProductionDraft) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(draft).map_err(|e| e.to_string())?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("Saved production instructions exceed their 2 MiB storage limit".into());
        }
        write_profile(&self.profile_access, &self.root.join("production-draft.json"), &bytes)
    }

    fn bind_production_draft(&mut self, session: &Session, id: &str) -> Result<(), String> {
        let Some(mut draft) = self.snapshot.production_draft.clone() else { return Ok(()) };
        let current = session.doc().map_err(|e| e.to_string())?;
        let prior_total = match draft.request.target {
            TaskTarget::NewA4 { pages } => Some(pages),
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

    fn start_skill(
        &mut self,
        prompt: String,
        paths: Vec<String>,
        context: Option<Value>,
        settings: AiSettings,
        design_style: bool,
    ) -> Result<(), String> {
        for path in &paths {
            file_access::check_read(path)?;
        }
        validate_settings(&settings)?;
        require_text_model(&settings)?;
        if prompt.trim().is_empty() || prompt.len() > MAX_BRIEF_BYTES {
            return Err(format!("Enter a nonempty description of at most {MAX_BRIEF_BYTES} UTF-8 bytes (1 MiB)"));
        }
        let work = worker::Work::Skill {
            design_style,
            prompt,
            reference_paths: paths,
            context,
            settings: settings.clone(),
            catalog: designcraft_engine::cmd::production::catalog(),
            registry_root: self.root.join("skills"),
        };
        self.launch(work, None, None, Vec::new(), settings)
    }

    fn launch(
        &mut self,
        work: worker::Work,
        task: Option<TaskRequest>,
        expected: Option<(u64, u64)>,
        skills: Vec<(String, String)>,
        settings: AiSettings,
    ) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("Only one AI task can run in this window".into());
        }
        self.sequence = self.sequence.saturating_add(1);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis();
        let id = format!("{stamp}-{}-{}", std::process::id(), self.sequence);
        let selections = match &work {
            worker::Work::Indesign(_) => Vec::new(),
            worker::Work::Resources => Vec::new(),
            worker::Work::Models(selection) => vec![selection.clone()],
            worker::Work::Production { request, .. } => {
                let mut selections = vec![request.settings.text_model.clone()];
                if request.settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages {
                    selections.push(request.settings.image_model.clone());
                }
                selections
            }
            worker::Work::Skill { settings, .. } => vec![settings.text_model.clone()],
        };
        self.prepare_credentials(&selections)?;
        let keys = self.worker_keys();
        let roots = file_access::current();
        let expected_prefs = match &work {
            worker::Work::Production { prefs, .. } => Some(prefs.clone()),
            _ => None,
        };
        let worker = worker::spawn(work, keys, roots.clone())?;
        self.pending = Some(Pending { roots, worker, id, started: Instant::now(), task, expected, expected_prefs, skills, settings });
        self.snapshot.busy = true;
        self.snapshot.current_usage = Default::default();
        self.snapshot.current_issues.clear();
        self.snapshot.progress = 0.0;
        self.snapshot.stage = "generating".into();
        self.snapshot.message = "AI task started".into();
        Ok(())
    }

    fn configured_key_selection(&self, provider: ProviderKind) -> Result<designcraft_ai_types::ModelSelection, String> {
        let selections = selected_models(&self.snapshot.settings);
        let mut matching = selections.into_iter().filter(|selection| selection.provider == provider);
        let selected = matching.next().ok_or("Select this provider in the current AI settings before setting its session key")?;
        if matching.any(|other| validate_endpoint(&other).ok() != validate_endpoint(&selected).ok()) {
            return Err("This provider has multiple endpoints; save its key using the endpoint-specific key field".into());
        }
        Ok(selected)
    }

    fn store_session_key(&mut self, selection: &designcraft_ai_types::ModelSelection, secret: SecretString) -> Result<(), String> {
        let stored = SessionKey::new(selection, secret)?;
        self.keys.retain(|key| !key.matches(selection));
        if self.keys.len() >= 32 {
            return Err("Too many session credential endpoints; restart the application to clear unused session keys".into());
        }
        self.keys.push(stored);
        Ok(())
    }

    fn worker_keys(&self) -> Vec<SessionKey> {
        self.keys.iter().map(SessionKey::duplicate).collect()
    }

    fn prepare_credentials(&mut self, selections: &[designcraft_ai_types::ModelSelection]) -> Result<(), String> {
        for selection in selections {
            validate_endpoint(selection)?;
            if self.keys.iter().any(|key| key.matches(selection)) {
                continue;
            }
            if !Vault::available() {
                continue;
            }
            let stored = credential_from_store_or_transient(
                selection,
                || self.vault.load(selection),
                || crate::vault::selected_key(&self.keys, selection).is_some(),
            )?;
            if let Some(secret) = stored {
                self.store_session_key(selection, secret)?;
            }
        }
        Ok(())
    }

    fn refresh_credentials(&mut self) {
        let selections = selected_models(&self.snapshot.settings);
        self.snapshot.credentials = selections
            .into_iter()
            .map(|selection| {
                let presence = if Vault::available() {
                    self.vault.presence(&selection)
                } else {
                    Err("OS-protected credential storage is unavailable; session keys remain in memory only".into())
                };
                let transient_available = crate::vault::selected_key(&self.keys, &selection).is_some();
                credential_status(selection, presence, transient_available)
            })
            .collect();
    }

    fn poll(&mut self, session: &mut Session) {
        let Some(pending) = self.pending.take() else { return };
        if let (Some(job), Some(snapshot)) = (&self.batch, &mut self.snapshot.batch) {
            snapshot.elapsed_ms = job.snapshot_elapsed().saturating_add(pending.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        }
        let mut completed = None;
        let mut disconnected = false;
        for _ in 0..64 {
            match pending.worker.receiver.try_recv() {
                Ok(worker::Message::Progress { stage, fraction }) => {
                    self.snapshot.stage = stage;
                    self.snapshot.progress = fraction.clamp(0.0, 1.0);
                }
                Ok(worker::Message::Usage(usage)) => {
                    self.snapshot.current_usage = usage.clone();
                    if let Some(job) = self.batch.as_mut() {
                        job.snapshot.cumulative_usage = usage.clone();
                    }
                    if let Some(batch) = self.snapshot.batch.as_mut() {
                        batch.cumulative_usage = usage;
                    }
                }
                Ok(worker::Message::Diagnostic(issue)) => {
                    // A long review group may repair many pages. Retain the
                    // latest bounded planning evidence without growing each
                    // poll/history response by a plan for every attempt.
                    if issue.get("kind").and_then(Value::as_str) == Some("nativePlanningValidation")
                        && let Some(previous) = self.snapshot.current_issues.iter_mut().find(|old| old.get("kind") == issue.get("kind"))
                    {
                        *previous = issue;
                    } else if self.snapshot.current_issues.len() < 1024 {
                        self.snapshot.current_issues.push(issue);
                    }
                }
                Ok(worker::Message::Finished(outcome)) => {
                    completed = Some(outcome);
                    break;
                }
                Ok(worker::Message::FinishedWithLedger(outcome, ledger)) => {
                    self.finish_batch(session, pending, outcome, *ledger);
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some(outcome) = completed {
            self.finish(session, pending, outcome);
        } else if disconnected {
            self.finish(
                session,
                pending,
                worker::Outcome {
                    result: Err("AI worker disconnected before reporting a result".into()),
                    usage: UsageSummary { has_unreported_usage: true, ..Default::default() },
                },
            );
        } else {
            self.pending = Some(pending);
        }
    }

    fn finish(&mut self, session: &mut Session, pending: Pending, outcome: worker::Outcome) {
        // A lost batch worker cannot be resumed with a reconstructed or reset budget.
        self.lose_batch_worker();
        self.snapshot.busy = false;
        let committed_export = matches!(&outcome.result, Ok(worker::Product::Indesign(_)));
        let cancelled = pending.worker.cancel.load(Ordering::Relaxed) && !committed_export;
        let mut report = TaskReport {
            design_style: self.style_use(&pending.skills, pending.task.as_ref().map(|task| &task.target)),
            id: pending.id,
            status: "failed".into(),
            title: pending.task.as_ref().map(|t| t.prompt.chars().take(80).collect()).unwrap_or_else(|| "AI service operation".into()),
            provider: pending.settings.text_model.provider,
            model: pending.settings.text_model.model.clone(),
            settings: Some(pending.settings.clone()),
            skills: pending.skills,
            usage: outcome.usage,
            elapsed_ms: pending.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            actions: 0,
            issues: self.snapshot.current_issues.clone(),
            artifacts: Vec::new(),
            message: String::new(),
        };
        let result = if cancelled {
            Err("AI task cancelled; document changes were not published".into())
        } else {
            file_access::confine(pending.roots.as_ref(), || {
                let product = outcome.result?;
                if let Some(expected) = &pending.expected_prefs
                    && serde_json::to_value(expected).map_err(|e| e.to_string())?
                        != serde_json::to_value(&session.prefs).map_err(|e| e.to_string())?
                {
                    return Err(
                        "Application preferences changed during generation; result was not published. Run the task again with current preferences"
                            .into(),
                    );
                }
                self.publish(session, &pending.task, pending.expected, product, &mut report)
            })
        };
        match result {
            Ok(()) => {
                report.status = "completed".into();
                self.snapshot.stage = "completed".into();
            }
            Err(error) => {
                report.status = if cancelled { "cancelled" } else { "failed" }.into();
                report.message = error;
                self.snapshot.stage = report.status.clone();
            }
        }
        if report.message.is_empty() {
            report.message = "AI task completed".into();
        }
        self.snapshot.message = report.message.clone();
        self.snapshot.progress = 1.0;
        self.snapshot.reports.push(report);
        if self.snapshot.reports.len() > MAX_REPORTS {
            self.snapshot.reports.remove(0);
        }
        if let Err(error) = write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports) {
            self.snapshot.message.push_str(&format!("; could not persist usage history: {error}"));
        }
    }

    fn publish(
        &mut self,
        session: &mut Session,
        task: &Option<TaskRequest>,
        expected: Option<(u64, u64)>,
        product: worker::Product,
        report: &mut TaskReport,
    ) -> Result<(), String> {
        match product {
            worker::Product::Indesign(export) => {
                report.title = "Verified native InDesign export".into();
                report.model = format!("External InDesign {}", export.version);
                report.artifacts = vec![export.path.clone(), export.audit_path.clone()];
                report.issues =
                    export.warnings.iter().map(|message| json!({"severity":"warning","kind":"interchangeLoss","message":message})).collect();
                report.message = format!(
                    "Exported real INDD with {} pages, {} editable text frames and {} native shapes; Unicode content and page/object bounds verified. No AI request was made.",
                    export.pages, export.text_frames, export.shapes
                );
                if !export.warnings.is_empty() {
                    report.message.push_str(" Interchange warnings are recorded; keep the native .designcraft source.");
                }
            }
            worker::Product::Resources(resources) => {
                report.message = resources.message.clone();
                self.snapshot.resources = Some(resources);
            }
            worker::Product::BatchPlan { .. } | worker::Product::BatchGroup { .. } => {
                return Err("Batch proof result requires its gated batch job".into());
            }
            worker::Product::Models(models) => {
                self.snapshot.models = models;
                report.message = "Model discovery completed".into();
            }
            worker::Product::Skill { files, analyses } => {
                let revision = self
                    .registry
                    .import_files(
                        files,
                        Provenance { source: "AI-assisted draft from user-selected references or corrected document".into(), ..Default::default() },
                    )
                    .map_err(|e| e.to_string())?;
                for analysis in analyses {
                    self.registry.cache_analysis(&analysis.reference_sha256, &analysis.analyzer, &analysis.text).map_err(|e| e.to_string())?;
                }
                self.sample = None;
                self.inspect_skill(&revision.skill_id, &revision.revision)?;
                report.message = "Skill generated as a draft; review instructions, native sample and output before approval".into();
            }
            worker::Product::Plan(plan) => {
                let task = task.as_ref().ok_or("Production result has no originating task")?;
                ensure_publication_idle(session)?;
                if let Some(directory) = &task.export_directory {
                    file_access::check_write(directory)?;
                    for extension in ["designcraft", "pdf"] {
                        file_access::check_write(Path::new(directory).join(format!("production-{}.{}", report.id, extension)))?;
                    }
                }
                let previous_active = session.active_index();
                let mut switched_tab = false;
                if let Some((uid, revision)) = expected {
                    let Some(target_index) = session.documents().iter().position(|doc| doc.uid == uid) else {
                        return Err(
                            "The document changed during generation; result was not applied. Run the task again against the current document".into(),
                        );
                    };
                    let target = session.documents().get(target_index).ok_or("Target document disappeared")?;
                    if target.interaction.is_some() || target.preview_stash.is_some() {
                        return Err("Finish the current interaction, IME composition or preview before publishing AI production".into());
                    }
                    if target.revision != revision {
                        return Err(
                            "The document changed during generation; result was not applied. Run the task again against the current document".into(),
                        );
                    }
                    if session.active_index() != Some(target_index) {
                        session.set_active(target_index);
                        switched_tab = true;
                    }
                }
                let runs = self.root.join("runs");
                self.profile_access.check(&runs, file_access::Access::Write)?;
                std::fs::create_dir_all(&runs).map_err(|e| e.to_string())?;
                let trace = runs.join(format!("{}.json", report.id));
                let page_params = match task.target {
                    TaskTarget::NewA4 { pages } => {
                        Some(self.pinned_document_params_for_brief(&report.skills, pages, &task.settings, &task.prompt)?)
                    }
                    _ => None,
                };
                let context = match task.target {
                    TaskTarget::CurrentDocument | TaskTarget::ContinueBook => compact_context(session)?,
                    TaskTarget::NewA4 { pages } => json!({"newA4Pages":pages,"documentParams":page_params}),
                };
                if let Err(err) = write_json(
                    &self.profile_access,
                    &trace,
                    &json!({"schemaVersion":1,"taskId":report.id,"settings":task.settings,"skills":report.skills,"designStyle":report.design_style,"base":context,"plan":plan}),
                ) {
                    if switched_tab && let Some(index) = previous_active {
                        session.set_active(index);
                    }
                    return Err(err);
                }
                report.artifacts.push(trace.to_string_lossy().into_owned());
                let new_document = matches!(task.target, TaskTarget::NewA4 { .. });
                let min_ppi =
                    crate::print_contract::PrintContract::from_prompt(&task.prompt)?.and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
                let mut created = false;
                let published = (|| {
                    if let Some(params) = page_params {
                        session.execute("file.new", &params).map_err(|e| e.to_string())?;
                        created = true;
                        initialize_print_document(session, &task.prompt)?;
                        initialize_facing_document(session, &task.settings, &task.prompt)?;
                        // Creation settings form the empty document's baseline.
                        // Only the production edit belongs in the user's undo history.
                        let baseline = session.doc_mut().map_err(|e| e.to_string())?;
                        baseline.history.undo.clear();
                        baseline.history.redo.clear();
                    }
                    let current = session.doc().map_err(|e| e.to_string())?;
                    let mapped = if new_document && task.settings.facing_pages { Some(crate::facing_pages::map_plan(&plan, current)?) } else { None };
                    let applied = mapped.as_ref().unwrap_or(&plan);
                    let params = json!({"commands":applied.commands,"outputMode":task.settings.output_mode,"label":applied.title,"expectedDocumentUid":current.uid,"expectedRevision":current.revision,"requirePreflight":true,"minPpi":min_ppi});
                    session.execute("production.apply", &params).map_err(|e| e.to_string())?;
                    Ok::<_, String>((applied.commands.len(), applied.title.clone()))
                })();
                let (actions, title) = match published {
                    Ok(value) => value,
                    Err(error) => {
                        if created {
                            session
                                .execute("file.close", &json!({}))
                                .map_err(|e| format!("{error}; could not close the failed new document: {e}"))?;
                            if let Some(index) = previous_active {
                                session.execute("file.activate", &json!({"index":index})).map_err(|e| e.to_string())?;
                            }
                        } else if switched_tab && let Some(index) = previous_active {
                            session.set_active(index);
                        }
                        return Err(error);
                    }
                };
                report.actions = actions;
                report.title = title;
                if let Some(contract) = crate::print_contract::PrintContract::from_prompt(&task.prompt)? {
                    self.print_contracts.insert(session.doc().map_err(|e| e.to_string())?.uid, contract);
                }
                let preflight = ensure_preflight_at(session, min_ppi)?;
                report.issues.extend(preflight.get("issues").and_then(Value::as_array).into_iter().flatten().cloned());
                if let Some(directory) = &task.export_directory {
                    file_access::check_write(directory)?;
                    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
                    for (extension, command) in [("designcraft", "file.saveAs"), ("pdf", "file.exportPdf")] {
                        let path = Path::new(directory).join(format!("production-{}.{}", report.id, extension));
                        let args = if extension == "pdf" {
                            match crate::print_contract::PrintContract::from_prompt(&task.prompt)? {
                                Some(contract) => contract.pdf_export_params(Some(path.to_str().ok_or("PDF export path is not valid Unicode")?))?,
                                None => json!({"path":path,"spreads":false,"bleed":false,"marks":false}),
                            }
                        } else {
                            json!({"path":path})
                        };
                        session.execute(command, &args).map_err(|e| e.to_string())?;
                        report.artifacts.push(path.to_string_lossy().into_owned());
                    }
                }
                report.message = format!("Created {} editable native actions; {} preflight notices", report.actions, report.issues.len());
            }
        }
        Ok(())
    }

    fn choose_skills(&self, prompt: &str) -> Vec<String> {
        choose_compatible_skills(prompt, self.registry.catalog(""), &supported_commands())
    }

    #[cfg(test)]
    fn pinned_document_params(&self, pins: &[(String, String)], pages: u32) -> Result<Value, String> {
        self.pinned_document_params_for_brief(pins, pages, &AiSettings::default(), "")
    }
    fn pinned_document_params_for_brief(&self, pins: &[(String, String)], pages: u32, settings: &AiSettings, brief: &str) -> Result<Value, String> {
        let mut profile = None;
        for (id, revision) in pins {
            let record = self.registry.revision(id, revision).map_err(|e| e.to_string())?;
            if let Some(selected) = record.manifest.design_profile.as_ref() {
                if profile.is_some() {
                    return Err("New document has conflicting pinned design profiles".into());
                }
                selected.validate()?;
                profile = Some(selected);
            }
        }
        production_document_params(profile, pages, settings, brief)
    }

    fn prepare_automatic_skills(&mut self, session: &Session, prompt: &str) -> Result<Vec<String>, String> {
        self.prepare_skills_for_style(session, prompt, None)
    }

    fn prepare_skills_for_style(&mut self, session: &Session, prompt: &str, style_id: Option<&str>) -> Result<Vec<String>, String> {
        if prompt.trim().is_empty() || prompt.len() > MAX_BRIEF_BYTES {
            return Err(format!("Enter a nonempty brief of at most {MAX_BRIEF_BYTES} UTF-8 bytes (1 MiB)"));
        }
        self.snapshot.skill_warnings.clear();
        let supported = supported_commands();
        let mut candidates = self
            .registry
            .catalog("")
            .into_iter()
            .filter_map(|entry| {
                if !entry.enabled {
                    return None;
                }
                let revision = entry.revisions.last()?;
                if revision.manifest.design_profile.is_some() {
                    return None;
                }
                if !skill_is_compatible(revision, &supported) {
                    return None;
                }
                let score = skill_match_score(prompt, &entry);
                (score > 0).then(|| (score, entry.skill_id.clone(), revision.revision.clone(), revision.status.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, id, revision, status) in candidates.into_iter().take(6) {
            if status == RevisionStatus::Approved {
                continue;
            }
            let validation =
                validate_skill_templates(&self.registry, session, &id, &revision).and_then(|()| self.run_sample(session, &id, &revision));
            if let Err(error) = validation {
                self.snapshot.skill_warnings.push(format!(
                    "Skipped invalid skill draft {id}: {}. A compatible active revision is retained when available.",
                    error.chars().take(1024).collect::<String>()
                ));
                continue;
            }
            let evidence = self.sample.as_ref().ok_or("Automatic sample evidence is unavailable")?;
            self.registry
                .activate_automatically(
                    &id,
                    &revision,
                    ApprovalEvidence {
                        reviewer: String::new(),
                        sample_artifact: evidence.path.clone(),
                        executed_actions: evidence.actions.clone(),
                        supported_commands: supported.clone(),
                        notes: String::new(),
                    },
                )
                .map_err(|error| error.to_string())?;
        }
        self.refresh_catalog();
        let mut ids = self.choose_skills(prompt);
        let style = match style_id {
            Some(id) => Some(id.to_owned()),
            None => self
                .registry
                .catalog("")
                .into_iter()
                .filter(|entry| entry.enabled)
                .filter_map(|entry| {
                    let revision = design_revision(&entry)
                        .filter(|r| skill_is_compatible(r, &supported))
                        .or_else(|| active_design_revision(&entry).filter(|r| skill_is_compatible(r, &supported)))?;
                    let profile = revision.manifest.design_profile.as_ref()?;
                    let lower = normalize_skill_search(prompt);
                    let hits = profile.keywords.iter().filter(|word| lower.contains(&normalize_skill_search(word))).count();
                    let bonus = usize::from(hits > 0 && entry.skill_id == "kg-oxford-cambridge-style") * 16;
                    (hits > 0).then_some((hits.saturating_add(bonus), entry.skill_id))
                })
                .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
                .map(|(_, id)| id),
        };
        if let Some(id) = style {
            self.prepare_style(session, &id)?;
            ids.push(id);
        }
        Ok(ids)
    }

    fn prepare_pinned_skills(&mut self, session: &Session, pinned: &[String]) -> Result<Vec<String>, String> {
        if pinned.is_empty() {
            return Ok(Vec::new());
        }
        let supported = supported_commands();
        let mut active_pinned = Vec::new();
        for id in pinned {
            let Some(entry) = self.registry.catalog("").into_iter().find(|e| &e.skill_id == id) else {
                continue;
            };
            if !entry.enabled {
                continue;
            }
            if entry.active_revision.is_some() {
                active_pinned.push(id.clone());
                continue;
            }
            let Some(revision) = entry.revisions.last() else { continue };
            if revision.manifest.design_profile.is_some() || !skill_is_compatible(revision, &supported) {
                continue;
            }
            let rev_id = revision.revision.clone();
            if validate_skill_templates(&self.registry, session, id, &rev_id).and_then(|()| self.run_sample(session, id, &rev_id)).is_ok()
                && let Some(evidence) = self.sample.as_ref()
            {
                self.registry
                    .activate_automatically(
                        id,
                        &rev_id,
                        ApprovalEvidence {
                            reviewer: String::new(),
                            sample_artifact: evidence.path.clone(),
                            executed_actions: evidence.actions.clone(),
                            supported_commands: supported.clone(),
                            notes: String::new(),
                        },
                    )
                    .map_err(|e| e.to_string())?;
                active_pinned.push(id.clone());
            }
        }
        if !active_pinned.is_empty() {
            self.refresh_catalog();
        }
        Ok(active_pinned)
    }

    fn prepare_style(&mut self, session: &Session, id: &str) -> Result<(), String> {
        let entry = self.registry.catalog("").into_iter().find(|e| e.skill_id == id).ok_or_else(|| format!("Unknown design style: {id}"))?;
        if !entry.enabled {
            return Err(format!("Design style {id} is disabled"));
        }
        let revision = design_revision(&entry).ok_or("Design style has no revision")?;
        let version = revision.revision.clone();
        let result = (|| {
            if revision.manifest.design_profile.is_none() || !skill_is_compatible(revision, &supported_commands()) {
                return Err(format!("Design style {id} is incompatible or has no design profile"));
            }
            if revision.status == RevisionStatus::Approved {
                return Ok(());
            }
            validate_skill_templates(&self.registry, session, id, &version)?;
            self.run_sample(session, id, &version)?;
            let sample = self.sample.as_ref().ok_or("Design style sample evidence is unavailable")?;
            self.registry.activate_automatically(id, &version, ApprovalEvidence {
                reviewer: String::new(), sample_artifact: sample.path.clone(), executed_actions: sample.actions.clone(),
                supported_commands: supported_commands(), notes: "All page-family templates passed native preflight and print geometry checks. Educational and aesthetic review remains human.".into(),
            }).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(error) = result {
            if active_design_revision(&entry).is_some_and(|active| active.revision != version && skill_is_compatible(active, &supported_commands())) {
                self.snapshot.message = format!("New design-style draft failed validation; retained the active revision: {error}");
            } else {
                return Err(error);
            }
        }
        self.refresh_catalog();
        Ok(())
    }

    fn style_use(&self, pins: &[(String, String)], target: Option<&TaskTarget>) -> Option<designcraft_ai_types::DesignStyleUse> {
        pins.iter().find_map(|(id, revision)| {
            let record = self.registry.revision(id, revision).ok()?;
            let profile = record.manifest.design_profile.as_ref()?;
            Some(designcraft_ai_types::DesignStyleUse {
                skill_id: id.clone(),
                revision: revision.clone(),
                name: profile.name.clone(),
                execution_mode: if matches!(target, Some(TaskTarget::NewA4 { .. } | TaskTarget::ContinueBook))
                    && profile.page_families.iter().all(|family| !family.text_slots.is_empty())
                {
                    "native_templates".into()
                } else {
                    "command_planner".into()
                },
            })
        })
    }
    fn refresh_catalog(&mut self) {
        let supported = supported_commands();
        self.snapshot.skills = self
            .registry
            .catalog("")
            .into_iter()
            .flat_map(|entry| {
                let supported = &supported;
                entry.revisions.into_iter().map(move |revision| {
                    let compatible = skill_is_compatible(&revision, supported);
                    let trusted_source = is_trusted_catalog_provenance(&revision.provenance);
                    SkillCard {
                        design_profile: revision.manifest.design_profile.clone(),
                        id: entry.skill_id.clone(),
                        name: revision.name,
                        description: revision.description,
                        active: entry.active_revision.as_ref() == Some(&revision.revision),
                        revision: revision.revision,
                        approved: revision.status == RevisionStatus::Approved,
                        automatically_validated: revision.approval.as_ref().is_some_and(|approval| approval.automatic),
                        trusted_source,
                        enabled: entry.enabled,
                        compatible,
                    }
                })
            })
            .collect();
    }
    fn inspect_skill(&mut self, id: &str, revision: &str) -> Result<(), String> {
        let files = self.registry.review_files(id, revision).map_err(|e| e.to_string())?;
        let text = |name: &str| -> Result<String, String> {
            String::from_utf8(files.get(name).ok_or_else(|| format!("Missing {name}"))?.clone()).map_err(|e| e.to_string())
        };
        let sample = self.sample.as_ref().filter(|s| s.id == id && s.revision == revision);
        self.snapshot.review = Some(SkillReview {
            id: id.into(),
            revision: revision.into(),
            skill_md: text("SKILL.md")?,
            manifest_json: text("designcraft.json")?,
            sample_path: sample.map(|s| s.path.to_string_lossy().into_owned()),
            sample_valid: sample.is_some(),
            files: files.keys().cloned().collect(),
        });
        Ok(())
    }
    fn run_sample(&mut self, session: &Session, id: &str, revision: &str) -> Result<(), String> {
        self.run_design_template(session, id, revision, None)
    }

    fn run_design_template(&mut self, session: &Session, id: &str, revision: &str, template: Option<&str>) -> Result<(), String> {
        let dir = self.root.join("samples");
        let manifest = self.registry.revision(id, revision).map_err(|e| e.to_string())?.manifest.clone();
        let template_actions = if let Some(template) = template {
            if !manifest.design_profile.as_ref().is_some_and(|profile| profile.page_families.iter().any(|family| family.template == template)) {
                return Err("Template is not a declared design page family".into());
            }
            Some(self.registry.review_templates(id, revision).map_err(|e| e.to_string())?.remove(template).ok_or("Missing native page template")?)
        } else {
            None
        };
        let suffix = template.and_then(|path| Path::new(path).file_stem()).and_then(|name| name.to_str()).unwrap_or("sample");
        let path = dir.join(format!("{id}-{revision}-{suffix}.png"));
        self.profile_access.check(&dir, file_access::Access::Write)?;
        self.profile_access.check(&path, file_access::Access::Write)?;
        self.sample = None;
        let actions = match template_actions {
            Some(actions) => actions,
            None => self.registry.sample_actions(id, revision).map_err(|e| e.to_string())?,
        };
        let mut scratch = Session::new();
        scratch.prefs = session.prefs.clone();
        let profile = self.registry.revision(id, revision).map_err(|e| e.to_string())?.manifest.design_profile.clone();
        scratch.execute("file.new", &crate::design::document_params(profile.as_ref(), 1)).map_err(|e| e.to_string())?;
        scratch
            .execute(
                "production.apply",
                &json!({"commands":actions,"outputMode":"native_only","label":"Skill review sample","requirePreflight":true}),
            )
            .map_err(|e| e.to_string())?;
        ensure_preflight(&mut scratch)?;
        if let Some(profile) = &profile {
            crate::design::check_geometry(None, scratch.doc().map_err(|e| e.to_string())?, profile)?;
        }
        let mut renderer = designcraft_render::Renderer::new();
        let doc = scratch.doc().map_err(|e| e.to_string())?;
        let raster = renderer
            .render_page(&doc.doc, &scratch.cache, 0, 1.2, false, &designcraft_render::RenderOptions::default())
            .ok_or("Could not render skill sample")?;
        let bytes = raster.to_png();
        if bytes.is_empty() {
            return Err("Skill sample image is empty".into());
        }
        self.profile_access.check(&dir, file_access::Access::Write)?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        write_profile(&self.profile_access, &path, &bytes)?;
        let is_main = template.is_none() || template == manifest.sample_template.as_deref();
        if is_main {
            self.sample = Some(SampleEvidence { id: id.into(), revision: revision.into(), actions, path: path.clone() });
        }
        self.inspect_skill(id, revision)?;
        if let Some(review) = &mut self.snapshot.review {
            review.sample_path = Some(path.to_string_lossy().into_owned());
        }
        self.snapshot.message = "Sample executed with native tools and passed preflight; review its preview before approving".into();
        Ok(())
    }

    fn prepare_selected_skills(&mut self, session: &Session, skill_ids: &[String], style_id: Option<&str>) -> Result<Vec<String>, String> {
        let mut ids = skill_ids.to_vec();
        if let Some(style) = style_id {
            let entry =
                self.registry.catalog("").into_iter().find(|e| e.skill_id == style).ok_or_else(|| format!("Unknown design style: {style}"))?;
            if !entry.enabled {
                return Err(format!("Design style {style} is disabled"));
            }
            let latest = design_revision(&entry).ok_or("Design style has no revision")?;
            let untrusted_draft = latest.status == RevisionStatus::Draft && !is_trusted_catalog_provenance(&latest.provenance);
            if untrusted_draft {
                if entry.active_revision.is_none() {
                    return Err(format!("Skill {style} requires approval"));
                }
                let supported = supported_commands();
                if !active_design_revision(&entry).is_some_and(|active| skill_is_compatible(active, &supported)) {
                    return Err(format!("Design style {style} is incompatible or has no design profile"));
                }
            } else {
                self.prepare_style(session, style)?;
            }
            if !ids.iter().any(|selected| selected == style) {
                ids.push(style.to_owned());
            }
        }
        let mut non_style_trusted = Vec::new();
        for id in &ids {
            let Some(entry) = self.registry.catalog("").into_iter().find(|e| &e.skill_id == id) else {
                continue;
            };
            if entry.active_revision.is_some() || !entry.enabled {
                continue;
            }
            let Some(revision) = entry.revisions.last() else {
                continue;
            };
            if !is_trusted_catalog_provenance(&revision.provenance) {
                continue;
            }
            if revision.manifest.design_profile.is_some() {
                self.prepare_style(session, id)?;
            } else {
                non_style_trusted.push(id.clone());
            }
        }
        if !non_style_trusted.is_empty() {
            self.prepare_pinned_skills(session, &non_style_trusted)?;
        }
        Ok(ids)
    }

    fn default_preview_style_id(&self) -> Option<String> {
        let supported = supported_commands();
        let entries = self.registry.catalog("");
        let has_compatible_profile = |entry: &designcraft_ai_skills::CatalogEntry| {
            entry.enabled
                && (design_revision(entry).is_some_and(|r| skill_is_compatible(r, &supported))
                    || active_design_revision(entry).is_some_and(|r| skill_is_compatible(r, &supported)))
        };
        for preferred in ["kg-oxford-cambridge-style", "early-years-clear-style"] {
            if entries.iter().any(|entry| entry.skill_id == preferred && has_compatible_profile(entry)) {
                return Some(preferred.to_owned());
            }
        }
        entries.into_iter().find(has_compatible_profile).map(|entry| entry.skill_id)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableCatalogManifest {
    schema_version: u32,
    packages: Vec<PortableCatalogPackage>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableCatalogPackage {
    id: String,
    files: BTreeMap<String, String>,
}

const EMBEDDED_PORTABLE_CATALOG: &[u8] = include_bytes!("../../../skills/catalog.json");

fn is_trusted_catalog_provenance(provenance: &Provenance) -> bool {
    provenance.source.starts_with("DesignCraft native builtin")
        || (provenance.source == "Smart Design portable catalog"
            && provenance.source_sha256.as_deref() == Some(hex_sha256(EMBEDDED_PORTABLE_CATALOG).as_str()))
}

fn validate_portable_package_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')) {
        return Err("invalid portable package identifier".into());
    }
    let mut comps = Path::new(id).components();
    if !matches!(comps.next(), Some(std::path::Component::Normal(_))) || comps.next().is_some() {
        return Err("portable package identifier must be a single path segment".into());
    }
    Ok(())
}

fn resolve_portable_package_file(pkg_dir: &Path, rel_path: &str) -> Result<PathBuf, String> {
    if rel_path.is_empty() || rel_path.len() > 256 || rel_path.contains('\\') {
        return Err(format!("invalid portable package file path: {rel_path}"));
    }
    let mut current = pkg_dir.to_path_buf();
    let segments: Vec<_> = Path::new(rel_path).components().collect();
    if segments.is_empty() || segments.len() > 8 {
        return Err(format!("portable package file depth out of bounds: {rel_path}"));
    }
    for (idx, comp) in segments.iter().enumerate() {
        let std::path::Component::Normal(seg) = comp else {
            return Err(format!("non-normal portable package file path: {rel_path}"));
        };
        current.push(seg);
        let meta = std::fs::symlink_metadata(&current).map_err(|e| format!("{rel_path}: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("symlink forbidden in portable package path: {rel_path}"));
        }
        let is_last = idx + 1 == segments.len();
        if is_last && !meta.is_file() {
            return Err(format!("portable package entry is not a regular file: {rel_path}"));
        }
        if !is_last && !meta.is_dir() {
            return Err(format!("portable package intermediate segment is not a directory: {rel_path}"));
        }
    }
    Ok(current)
}

fn read_catalog_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("catalog entry must be a regular non-symlink file".into());
    }
    if meta.len() > max_bytes {
        return Err("catalog entry exceeds byte limit".into());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let limit = max_bytes.saturating_add(1);
    let mut buf = Vec::with_capacity(usize::try_from(meta.len().min(64 * 1024)).unwrap_or(0));
    file.take(limit).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if u64::try_from(buf.len()).map_or(true, |len| len > max_bytes) {
        return Err("catalog entry grew past byte limit".into());
    }
    Ok(buf)
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let hi = char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0');
        let lo = char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0');
        out.push(hi);
        out.push(lo);
    }
    out
}

/// Preserve immutable guidance and executable design profiles within the task
/// budget. Command planners can omit optional sample templates, largest first;
/// the content compiler never needs non-style command examples.
fn bounded_task_skills(mut skills: Vec<SkillSnapshot>, target: &TaskTarget) -> Result<Vec<SkillSnapshot>, String> {
    if matches!(target, TaskTarget::CurrentDocument) {
        // The opened document supplies its actual geometry. Fixed page-template
        // commands are examples, while instructions, references and profile
        // identity remain pinned guidance for its command planner.
        for skill in &mut skills {
            if skill.design_profile.is_some() {
                skill.templates.clear();
            }
        }
    }
    let multi_limit = (64 * 1024usize).saturating_add(skills.len().saturating_sub(2).saturating_mul(16 * 1024)).min(160 * 1024);
    if matches!(target, TaskTarget::NewA4 { .. } | TaskTarget::ContinueBook) && skills.iter().any(crate::template_plan::eligible) {
        for skill in &mut skills {
            if skill.design_profile.is_none() {
                skill.templates.clear();
            }
        }
        // Compiler commands remain immutable local inputs. The model receives
        // only deduplicated guidance and the serialized design-profile rules.
        let mut bytes = worker::compact_skill_guidance(&skills)?.len();
        for profile in skills.iter().filter_map(|skill| skill.design_profile.as_ref()) {
            let rules = serde_json::to_vec(profile).map_err(|e| e.to_string())?.len();
            if rules > 32 * 1024 {
                return Err("Template style rules exceed the bounded context limit".into());
            }
            bytes = bytes.checked_add(rules).ok_or("Selected skill context size overflow")?;
            if bytes > multi_limit {
                return Err("Selected skills exceed the 64-KiB task context limit; reduce references or split the skill before retrying".into());
            }
        }
        return Ok(skills);
    }
    if task_skill_context_bytes(&skills)? <= 64 * 1024 {
        return Ok(skills);
    }
    let mut optional_examples = Vec::new();
    for (index, skill) in skills.iter().enumerate().filter(|(_, skill)| skill.design_profile.is_none()) {
        for (path, actions) in &skill.templates {
            let size = serde_json::to_vec(&(path, actions)).map_err(|e| e.to_string())?.len();
            optional_examples.push((size, index, path.clone()));
        }
    }
    optional_examples.sort_by_key(|example| std::cmp::Reverse(example.0));
    for (_, index, path) in optional_examples {
        if let Some(skill) = skills.get_mut(index) {
            skill.templates.remove(&path);
        }
        if task_skill_context_bytes(&skills)? <= 64 * 1024 {
            return Ok(skills);
        }
    }
    if task_skill_context_bytes(&skills)? <= multi_limit {
        return Ok(skills);
    }
    if skills.len() > 1
        && skills.iter().all(|s| s.instructions.len() <= 32 * 1024 && s.references.values().all(|r| r.len() <= 32 * 1024))
        && let Ok(guidance) = worker::compact_skill_guidance_limit(&skills, multi_limit)
    {
        let mut dedup_bytes = guidance.len();
        let mut fits = true;
        for profile in skills.iter().filter_map(|skill| skill.design_profile.as_ref()) {
            let rules = serde_json::to_vec(profile).map_err(|e| e.to_string())?.len();
            if rules > 32 * 1024 {
                fits = false;
                break;
            }
            dedup_bytes = dedup_bytes.saturating_add(rules);
        }
        if fits && dedup_bytes <= multi_limit {
            return Ok(skills);
        }
    }
    Err("Selected skills exceed the 64-KiB task context limit; reduce references or split the skill before retrying".into())
}

fn merge_pinned_and_automatic_skills(pinned: &[String], auto_ids: Vec<String>, style_id: Option<&str>) -> Vec<String> {
    if pinned.is_empty() {
        return auto_ids;
    }
    let mut merged = Vec::new();
    for id in pinned {
        if !merged.contains(id) {
            merged.push(id.clone());
        }
    }
    let reserve_style = style_id.filter(|style| !merged.iter().any(|existing| existing == *style) && auto_ids.iter().any(|id| id == *style));
    let cap = if reserve_style.is_some() { 7 } else { 8 };
    for id in auto_ids {
        if reserve_style.is_some_and(|style| style == id) {
            continue;
        }
        if merged.len() >= cap {
            break;
        }
        if !merged.contains(&id) {
            merged.push(id);
        }
    }
    if let Some(style) = reserve_style {
        merged.push(style.to_owned());
    }
    merged
}

fn task_skill_context_bytes(skills: &[SkillSnapshot]) -> Result<usize, String> {
    let mut bytes = 2usize;
    for (index, skill) in skills.iter().enumerate() {
        let size = serde_json::to_vec(skill).map_err(|e| e.to_string())?.len();
        bytes = bytes.checked_add(size).and_then(|size| size.checked_add(usize::from(index > 0))).ok_or("Selected skill context size overflow")?;
    }
    Ok(bytes)
}

fn design_revision(entry: &designcraft_ai_skills::CatalogEntry) -> Option<&designcraft_ai_skills::SkillRevision> {
    let latest = entry.revisions.last()?;
    if latest.status == RevisionStatus::Draft {
        Some(latest)
    } else {
        entry.active_revision.as_ref().and_then(|id| entry.revisions.iter().find(|r| &r.revision == id)).or(Some(latest))
    }
}

fn active_design_revision(entry: &designcraft_ai_skills::CatalogEntry) -> Option<&designcraft_ai_skills::SkillRevision> {
    entry.active_revision.as_ref().and_then(|id| {
        entry.revisions.iter().find(|r| &r.revision == id && r.status == RevisionStatus::Approved && r.manifest.design_profile.is_some())
    })
}

fn selected_models(settings: &AiSettings) -> Vec<designcraft_ai_types::ModelSelection> {
    let text = settings.text_model.clone();
    let image = settings.image_model.clone();
    if text.provider == image.provider && validate_endpoint(&text).ok() == validate_endpoint(&image).ok() { vec![text] } else { vec![text, image] }
}

/// Revalidate every exact revision template immediately before approval. Each
/// variant gets a fresh scratch document; no live document, history or selection
/// is exposed to template execution, including failed result references.
fn validate_skill_templates(registry: &SkillRegistry, session: &Session, id: &str, revision: &str) -> Result<(), String> {
    let record = registry.revision(id, revision).map_err(|e| e.to_string())?;
    if !record.manifest.external_tools.is_empty() {
        return Err("Unsupported skill: adapt external tools before native template review".into());
    }
    let templates = registry.review_templates(id, revision).map_err(|e| e.to_string())?;
    if templates.is_empty() {
        return Err("Unsupported skill: no native templates are available for review".into());
    }
    let allowed: BTreeSet<_> = supported_commands().into_iter().filter(|command| command != "file.place").collect();
    let mut reviewed_pages = 0u64;
    for (name, actions) in templates {
        if actions.len() > 512 {
            return Err(format!("Unsupported skill template {name}: native review supports at most 512 actions per variant"));
        }
        let mut pages = 1u64;
        let mut inserted = 0u64;
        for action in &actions {
            if !allowed.contains(&action.command) || !record.manifest.required_commands.contains(&action.command) {
                return Err(format!("Unsupported skill template {name}: command {} is not declared and permitted for native review", action.command));
            }
            if let Some(spread) = action.params.get("spread").and_then(Value::as_u64) {
                pages = pages.max(spread.checked_add(1).ok_or("Skill template spread overflow")?);
            }
            if action.command == "layout.documentSetup"
                && let Some(value) = action.params.get("pages")
            {
                pages = pages
                    .max(value.as_u64().ok_or_else(|| format!("Unsupported skill template {name}: review page counts must be literal integers"))?);
            }
            if action.command == "layout.pages.insert" {
                let count = action
                    .params
                    .get("count")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("Unsupported skill template {name}: review insertion counts must be literal integers"))?;
                inserted = inserted.checked_add(count).ok_or("Skill template page insertion overflow")?;
            }
        }
        let page_bound = pages.checked_add(inserted).ok_or("Skill template page bound overflow")?;
        reviewed_pages = reviewed_pages.checked_add(page_bound).ok_or("Skill template review page total overflow")?;
        if page_bound > 16 || reviewed_pages > 64 {
            return Err(format!("Unsupported skill template {name}: review is limited to 16 pages per variant and 64 pages total"));
        }
        let pages = u32::try_from(pages).map_err(|_| "Skill template review page count overflow")?;
        let mut scratch = Session::new();
        scratch.prefs = session.prefs.clone();
        scratch
            .execute("file.new", &crate::design::document_params(record.manifest.design_profile.as_ref(), pages))
            .map_err(|e| format!("Skill template {name}: {e}"))?;
        scratch
            .execute(
                "production.apply",
                &json!({
                    "commands":actions,"outputMode":"native_only","allowedCommands":allowed,
                    "label":"Skill template compatibility review","requirePreflight":true
                }),
            )
            .map_err(|e| format!("Skill template {name} is incompatible with the native engine: {e}"))?;
        if let Some(profile) = &record.manifest.design_profile {
            crate::design::check_geometry(None, scratch.doc().map_err(|e| e.to_string())?, profile)?;
        }
    }
    Ok(())
}

/// Stored credentials keep precedence over environment fallbacks. A storage
/// error may be ignored only when the caller's endpoint-aware resolver confirms
/// a usable transient key. This helper never serializes or returns that key.
fn credential_from_store_or_transient(
    selection: &designcraft_ai_types::ModelSelection,
    load: impl FnOnce() -> Result<Option<SecretString>, String>,
    transient_available: impl FnOnce() -> bool,
) -> Result<Option<SecretString>, String> {
    validate_endpoint(selection)?;
    match load() {
        Ok(stored) => Ok(stored),
        Err(error) => {
            if transient_available() {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn credential_status(selection: designcraft_ai_types::ModelSelection, presence: Result<bool, String>, transient_available: bool) -> CredentialStatus {
    let protected_storage_available = presence.is_ok();
    let persisted = presence.as_ref().copied().unwrap_or(false);
    let available = persisted || transient_available;
    let message = match presence {
        Ok(true) => "OS-protected key stored for this endpoint".into(),
        Ok(false) if transient_available => {
            "Credential available from an authorized session or canonical provider environment; no protected key stored".into()
        }
        Ok(false) => "No credential available for this endpoint; no protected key stored".into(),
        Err(error) if transient_available => format!("Credential available from an authorized session or canonical provider environment; {error}"),
        Err(error) => format!("No credential available for this endpoint; {error}"),
    };
    CredentialStatus { selection, persisted, available, protected_storage_available, message }
}

fn skill_is_compatible(revision: &designcraft_ai_skills::SkillRevision, supported: &BTreeSet<String>) -> bool {
    revision.manifest.external_tools.is_empty()
        && !revision.manifest.required_commands.is_empty()
        && revision.manifest.required_commands.iter().all(|command| supported.contains(command))
}

fn selected_skill_references(revision: &designcraft_ai_skills::SkillRevision) -> Vec<String> {
    revision.manifest.references.iter().filter(|path| designcraft_ai_skills::is_supported_text_reference(path)).cloned().collect()
}

fn choose_compatible_skills(prompt: &str, entries: Vec<designcraft_ai_skills::CatalogEntry>, supported: &BTreeSet<String>) -> Vec<String> {
    let mut ranked = entries
        .into_iter()
        .filter(|entry| {
            entry.enabled
                && entry.active_revision.as_ref().is_some_and(|active| {
                    entry.revisions.iter().find(|revision| &revision.revision == active).is_some_and(|revision| {
                        revision.status == RevisionStatus::Approved
                            && revision.manifest.design_profile.is_none()
                            && skill_is_compatible(revision, supported)
                    })
                })
        })
        .map(|entry| (skill_match_score(prompt, &entry), entry.skill_id))
        .filter(|(score, _)| *score > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.into_iter().take(6).map(|(_, id)| id).collect()
}

fn prompt_names_skill_id(prompt: &str, skill_id: &str) -> bool {
    if skill_id.len() < 3 {
        return false;
    }
    let lower_prompt = prompt.to_lowercase();
    let lower_id = skill_id.to_lowercase();
    lower_prompt.match_indices(&lower_id).any(|(idx, matched)| {
        let before_ok = lower_prompt.get(..idx).and_then(|s| s.chars().next_back()).is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-');
        let after_idx = idx.saturating_add(matched.len());
        let after_ok = lower_prompt.get(after_idx..).and_then(|s| s.chars().next()).is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-');
        before_ok && after_ok
    })
}

fn skill_match_score(prompt: &str, entry: &designcraft_ai_skills::CatalogEntry) -> usize {
    let exact_id_boost = if prompt_names_skill_id(prompt, &entry.skill_id) { 80 } else { 0 };
    let prompt = normalize_skill_search(prompt);
    let haystack = normalize_skill_search(&format!("{} {} {}", entry.skill_id, entry.name, entry.description));
    let words = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 3 || *word == "a4")
        .filter(|word| {
            !matches!(
                *word,
                "the"
                    | "and"
                    | "with"
                    | "for"
                    | "from"
                    | "that"
                    | "this"
                    | "into"
                    | "without"
                    | "are"
                    | "you"
                    | "your"
                    | "not"
                    | "all"
                    | "use"
                    | "على"
                    | "الى"
                    | "إلى"
                    | "هذا"
                    | "هذه"
                    | "ذلك"
                    | "التي"
                    | "الذي"
                    | "لكل"
                    | "لكي"
                    | "كما"
                    | "بعد"
                    | "قبل"
                    | "بين"
                    | "فقط"
                    | "جدا"
            )
        })
        .collect::<BTreeSet<_>>();
    // Repetition cannot multiply relevance, and a long brief cannot displace
    // core native/typography/print guidance merely by matching more words.
    let lexical = words.into_iter().filter(|word| haystack.contains(*word)).count().min(8);
    let mentions = |terms: &[&str]| terms.iter().any(|term| prompt.contains(term));
    let arabic = prompt.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c) && c.is_alphabetic());
    let intl = mentions(&[
        "دولي",
        "دولية",
        "عالمي",
        "عالمية",
        "أكسفورد",
        "اكسفورد",
        "كامبريدج",
        "كامبردج",
        "فاملي اند فريندز",
        "فرست فريندز",
        "كارنيت",
        "جارنيت",
        "international",
        "oxford",
        "cambridge",
        "family and friends",
        "first friends",
        "garnet",
    ]);
    let computing = mentions(&[
        "حاسوب",
        "الحاسوب",
        "كمبيوتر",
        "برمجة",
        "تقنية",
        "رقمي",
        "رقمية",
        "ذكاء اصطناعي",
        "خوارزميات",
        "بيانات",
        "شبكات",
        "روبوت",
        "computer",
        "computing",
        "ict",
        "coding",
        "programming",
        "software",
        "hardware",
        "algorithm",
        "digital",
        "robotics",
    ]);
    let book = mentions(&[
        "كتاب",
        "كتيب",
        "كراس",
        "منهج",
        "مناهج",
        "مقرر",
        "مادة",
        "ماده",
        "وحدة",
        "درس",
        "رياضيات",
        "علوم",
        "حاسوب",
        "الحاسوب",
        "كمبيوتر",
        "برمجة",
        "تقنية",
        "إسلامية",
        "اسلامية",
        "اسلاميه",
        "الاسلاميه",
        "قرآن",
        "قران",
        "عبادات",
        "سيرة",
        "سيره",
        "لغتي",
        "العربية",
        "اجتماعيات",
        "curriculum",
        "book",
        "workbook",
        "coursebook",
        "textbook",
        "math",
        "science",
        "computer",
        "computing",
        "ict",
        "islamic",
        "arabic",
        "social studies",
    ]);
    let upper_stage = mentions(&[
        "ثانوي",
        "ثانوية",
        "ثانويه",
        "إعدادي",
        "اعدادي",
        "متوسط",
        "متوسطة",
        "متوسطه",
        "جامعي",
        "جامعة",
        "جامعه",
        "secondary",
        "high school",
        "middle school",
        "university",
        "college",
    ]);
    let children = mentions(&[
        "أطفال",
        "اطفال",
        "طفل",
        "روضة",
        "روضه",
        "تمهيدي",
        "للصغار",
        "ابتدائي",
        "تلاميذ",
        "kg",
        "kg1",
        "kg2",
        "fs1",
        "fs2",
        "kindergarten",
        "preschool",
        "children",
        "early years",
        "early-years",
        "primary",
    ]);
    let visual = (!upper_stage && (children || intl || computing) && book)
        || mentions(&[
            "تصميم",
            "صمم",
            "توزيع",
            "بصري",
            "مصور",
            "صور",
            "رسوم",
            "بطاقات",
            "illustrat",
            "visual",
            "layout",
            "design",
            "picture",
            "flashcard",
        ]);
    let print = book || mentions(&["طباعة", "للطباعة", "مطبعة", "مطبوع", "print", "a4"]);
    let kg = !upper_stage && (children || intl || computing) && (book || visual);
    let architecture = mentions(&[
        "تسلسل",
        "وتسلسله",
        "وتسلسة",
        "وحدات",
        "ووحداته",
        "ووحداتة",
        "فصول",
        "وفصوله",
        "وفصولة",
        "ربط سلسلي",
        "ربط ذهني",
        "السلسلي الذهني",
        "خريطة منهج",
        "هيكلة المنهج",
        "بناء المنهج",
        "scope and sequence",
        "syllabus",
        "curriculum map",
        "spiral curriculum",
    ]);
    let math_or_cpa =
        mentions(&["رياضيات", "حساب", "العد", "كميات", "منطق", "محسوس", "قواعد", "math", "mathematics", "counting", "subitiz", "grammar", "cpa"]);
    let purpose = match entry.skill_id.as_str() {
        "native-engine-workflow" => 64,
        "arabic-english-typography" if arabic => 56,
        "school-print-preflight" if print => 48,
        "curriculum-print-design-studio" if book && print => 41,
        "kg-garnet-ict-computing" if kg && computing => 46,
        "kg-international-curriculum-architect" if (kg || book) && architecture => 45,
        "kg-cambridge-thinking-cpa" if kg && (computing || intl || math_or_cpa) => {
            if math_or_cpa {
                44
            } else {
                37
            }
        }
        "kg-international-curriculum-architect" if kg && (intl || book) => {
            if intl {
                43
            } else {
                39
            }
        }
        "kg-oxford-phonics-handwriting"
            if kg && mentions(&["حروف", "أصوات", "الأصوات", "صوتيات", "خط اليد", "تأسيس الكتابة", "phonics", "handwriting", "letter maze"]) =>
        {
            44
        }
        "kg-oxford-skills-time"
            if kg && (intl || computing || mentions(&["قراءة", "استماع", "تحدث", "مهارات", "reading", "listening", "speaking", "skills time"])) =>
        {
            if mentions(&["قراءة", "استماع", "تحدث", "مهارات", "reading", "listening", "speaking", "skills time"]) {
                44
            } else {
                38
            }
        }
        "kg-oxford-fluency-time"
            if kg && (intl || computing || mentions(&["محادثة", "حوار", "مشروع يدوي", "بمشروع يدوي", "craft", "fluency", "conversation"])) =>
        {
            if mentions(&["محادثة", "حوار", "مشروع يدوي", "بمشروع يدوي", "craft", "fluency", "conversation"]) {
                44
            } else {
                36
            }
        }
        "kg-sensory-illustration-vector"
            if kg
                && mentions(&[
                    "ألوان",
                    "الألوان",
                    "للألوان",
                    "الوان",
                    "قوس",
                    "فني",
                    "الفن",
                    "رسم",
                    "colour",
                    "color",
                    "rainbow",
                    "song",
                    "sensory art",
                ]) =>
        {
            44
        }
        "kg-school-print-prepress" if kg && mentions(&["كراس", "كلمات متقاطعة", "workbook", "crossword", "word bank"]) => 44,
        "native-editorial-variety" if visual && mentions(&["تنويع", "متنوع", "توزيع", "variety", "varied", "different layouts"]) => 42,
        "kg-cognitive-sensory-layout" if kg && visual => 40,
        "native-design-composition" if visual => 32,
        "kg-school-print-prepress" if kg && print => 24,
        "early-years-booklet-planning" | "early-years-activity-layout" if book => 12,
        _ => 0,
    };
    lexical.saturating_add(purpose).saturating_add(exact_id_boost)
}

/// Search-only normalization; task copy and pinned skill contents stay exact.
fn normalize_skill_search(text: &str) -> String {
    let stripped = text
        .chars()
        .filter(|c| !matches!(*c, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}'))
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let mut normalized = String::with_capacity(stripped.len().saturating_add(8));
    let mut prev_arabic = None;
    for ch in stripped.chars() {
        let is_arabic = ('\u{0600}'..='\u{06ff}').contains(&ch);
        let is_latin_or_digit = ch.is_ascii_alphanumeric();
        if let Some(was_arabic) = prev_arabic
            && ((was_arabic && is_latin_or_digit) || (!was_arabic && is_arabic))
            && !normalized.ends_with(' ')
        {
            normalized.push(' ');
        }
        normalized.push(ch);
        prev_arabic = if is_arabic {
            Some(true)
        } else if is_latin_or_digit {
            Some(false)
        } else {
            None
        };
    }
    normalized
}

fn supported_commands() -> BTreeSet<String> {
    designcraft_engine::cmd::production::catalog()
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("command").and_then(Value::as_str).map(str::to_string))
        .collect()
}
fn validate_settings(settings: &AiSettings) -> Result<(), String> {
    settings.resources.validate()?;
    if settings.text_model.provider == ProviderKind::Ollama && settings.budget.max_output_tokens >= settings.resources.context_tokens {
        return Err("Ollama output token limit must be smaller than the local context window".into());
    }
    if settings.pages_per_batch > 1000 {
        return Err("Human review groups must contain 1–1000 pages, or zero for a short one-shot task".into());
    }
    validate_endpoint(&settings.text_model)?;
    if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages {
        validate_endpoint(&settings.image_model)?;
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
fn validate_production_draft(draft: &ProductionDraft) -> Result<(), String> {
    validate_settings(&draft.request.settings)?;
    if draft.id.len() > 256
        || draft.request.prompt.trim().is_empty()
        || draft.request.prompt.len() > MAX_BRIEF_BYTES
        || draft.document_title.len() > 4096
        || draft.document_path.as_ref().is_some_and(|path| path.len() > 32_768)
        || draft.request.export_directory.as_ref().is_some_and(|path| path.len() > 32_768)
        || draft.request.skill_ids.len() > 8
        || draft.request.skill_ids.iter().any(|id| id.len() > 256)
        || matches!(draft.request.target, TaskTarget::NewA4 { pages } if !(1..=1000).contains(&pages))
        || draft.progress.completed_pages > 1000
        || draft.progress.target_total_pages.is_some_and(|pages| !(1..=1000).contains(&pages))
    {
        return Err("Saved production instructions contain invalid or oversized fields".into());
    }
    Ok(())
}
fn apply_execution_settings_to_draft(draft: &mut ProductionDraft, settings: &AiSettings) {
    let is_bound_book = draft.document_uid.is_some() || draft.document_path.is_some() || matches!(draft.request.target, TaskTarget::ContinueBook);
    if is_bound_book {
        draft.request.settings.budget = settings.budget.clone();
        draft.request.settings.text_model = settings.text_model.clone();
        draft.request.settings.image_model = settings.image_model.clone();
        draft.request.settings.output_mode = settings.output_mode;
        draft.request.settings.verify_generated_images = settings.verify_generated_images;
        draft.request.settings.allow_paid_repairs = settings.allow_paid_repairs;
        draft.request.settings.image_rates = settings.image_rates.clone();
        draft.request.settings.comfy_workflow = settings.comfy_workflow.clone();
        draft.request.settings.resources = settings.resources.clone();
    } else {
        draft.request.settings = settings.clone();
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
    if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages {
        if settings.image_model.provider != ProviderKind::ComfyUi && settings.image_model.model.trim().is_empty() {
            return Err("Choose an image model before starting illustrated generation".into());
        }
        if settings.image_model.provider == ProviderKind::Ollama {
            return Err("Ollama image generation is unsupported; choose an explicit image provider or switch Output Mode to Native Only".into());
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
fn task_document_context(
    session: &Session,
    target: &TaskTarget,
    profile: Option<&designcraft_ai_types::DesignProfile>,
    brief: &str,
    settings: &AiSettings,
) -> Result<(designcraft_engine::DocState, Value), String> {
    match target {
        TaskTarget::CurrentDocument => Ok((session.doc().map_err(|e| e.to_string())?.clone(), compact_context(session)?)),
        TaskTarget::ContinueBook => {
            let current = session.doc().map_err(|e| e.to_string())?;
            let desired_rtl = book_binding(settings, brief);
            let projected = if settings.facing_pages && !current.doc.settings.facing_pages {
                Some(crate::facing_pages::formatted_proof_state(current, &session.prefs, desired_rtl)?)
            } else if !settings.facing_pages
                && !current.doc.settings.facing_pages
                && current.doc.spreads.iter().all(|s| s.items.is_empty())
                && current.doc.settings.right_to_left_binding != desired_rtl
            {
                Some(crate::facing_pages::single_page_proof_state(current, &session.prefs, desired_rtl)?)
            } else {
                None
            };
            let state = crate::facing_pages::canonicalize(projected.as_ref().unwrap_or(current), &session.prefs)?;
            let mut scratch = Session::new();
            scratch.prefs = session.prefs.clone();
            scratch.add_document(state.clone());
            Ok((state, compact_context(&scratch)?))
        }
        TaskTarget::NewA4 { pages } => {
            if let Some(profile) = profile {
                profile.validate()?;
            }
            let mut scratch = Session::new();
            scratch.execute("file.new", &production_document_params(profile, *pages, settings, brief)?).map_err(|e| e.to_string())?;
            initialize_print_document(&mut scratch, brief)?;
            if settings.facing_pages {
                initialize_facing_document(&mut scratch, settings, brief)?;
                let state = crate::facing_pages::canonicalize(scratch.doc().map_err(|e| e.to_string())?, &scratch.prefs)?;
                scratch.add_document(state);
            }
            let context = compact_context(&scratch)?;
            Ok((scratch.doc().map_err(|e| e.to_string())?.clone(), context))
        }
    }
}
fn production_document_params(
    profile: Option<&designcraft_ai_types::DesignProfile>,
    pages: u32,
    settings: &AiSettings,
    brief: &str,
) -> Result<Value, String> {
    let rtl = book_binding(settings, brief);
    match crate::print_contract::PrintContract::from_prompt(brief)? {
        Some(contract) => contract.document_params_with_binding(profile, pages, rtl),
        None => Ok(crate::design::document_params_with_binding(profile, pages, rtl)),
    }
}
fn initialize_print_document(session: &mut Session, brief: &str) -> Result<(), String> {
    if let Some(contract) = crate::print_contract::PrintContract::from_prompt(brief)? {
        for action in contract.initial_actions() {
            session.execute(&action.command, &action.params).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub(crate) fn book_binding(settings: &AiSettings, brief: &str) -> bool {
    settings.right_to_left_binding.unwrap_or_else(|| crate::facing_pages::auto_direction(brief) == "rightToLeft")
}
fn initialize_facing_document(session: &mut Session, settings: &AiSettings, brief: &str) -> Result<(), String> {
    if settings.facing_pages {
        session
            .execute("layout.documentSetup", &json!({"facingPages":true,"binding":if book_binding(settings,brief){"rightToLeft"}else{"leftToRight"}}))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[cfg(test)]
fn new_a4(pages: u32) -> Value {
    json!({"preset":"A4","pages":pages,"facingPages":false,"margins":15.0*72.0/25.4,"bleed":0})
}
fn print_params(printer: Option<String>, grayscale: bool, duplex: String) -> Value {
    let mut params = json!({"grayscale":grayscale,"duplex":duplex,"scaling":"exact"});
    if let Some(printer) = printer {
        params["printer"] = json!(printer);
    }
    params
}
fn ensure_preflight(session: &mut Session) -> Result<Value, String> {
    ensure_preflight_at(session, 150.0)
}
fn ensure_preflight_at(session: &mut Session, min_ppi: f64) -> Result<Value, String> {
    let report = session.execute("preflight.run", &json!({"minPpi":min_ppi})).map_err(|e| e.to_string())?;
    if report.get("errors").and_then(Value::as_u64).is_none_or(|n| n > 0) {
        return Err(format!("Preflight errors require correction before output: {report}"));
    }
    Ok(report)
}
/// Check before file.new or evidence writes: normal command prologues can commit IME
/// composition and restore previews, so the later production.apply guard is insufficient.
fn ensure_publication_idle(session: &Session) -> Result<(), String> {
    if session.tool_composing() || session.active().is_some_and(|state| state.interaction.is_some() || state.preview_stash.is_some()) {
        return Err("Finish the current interaction, IME composition or preview before publishing AI production".into());
    }
    Ok(())
}
/// Task environment is rebuilt for every explicit start, retry and approved
/// group. It is host data, never authority recovered from a model response.
fn enrich_production_context(context: &mut Value, root: &Path, base: &DocState, task: &TaskRequest) -> Result<(), String> {
    let context = context.as_object_mut().ok_or("Invalid native production context")?;
    context.insert("_production_cache_root".into(), json!(root.join("production-cache-v1")));
    if task.settings.facing_pages {
        context.insert("facing_editorial_direction".into(), json!({
            "enabled":true,"rightToLeft":book_binding(&task.settings,&task.prompt),
            "instruction":"Plan occasional visually connected facing pairs, like an educational magazine: shared thematic scene, aligned sections, related activities and a continuous restrained identity. Use chapter openers and selected learning sequences, not one repeated spread everywhere. Preserve each page's supplied lesson and printable copy. Keep critical text/answer areas clear of the binding gutter; headers/folios follow left/right positions. Coordinate with the adjacent supplied PAGE block; both pages retain independently editable native elements. Cover page 1 stands alone; reader pairs are pages 2–3, 4–5 and so on."
        }));
    }
    if let Some(contract) = crate::print_contract::PrintContract::from_prompt(&task.prompt)? {
        context.insert("production_print_contract".into(), serde_json::to_value(&contract).map_err(|e| e.to_string())?);
        if !matches!(task.target, TaskTarget::NewA4 { .. })
            && let Err(message) = contract.validate_existing(base)
        {
            context.insert(
                "existing_document_print_notice".into(),
                json!(format!("Preserve the opened document's actual geometry; requested final print settings differ: {message}")),
            );
        }
    }
    Ok(())
}

fn compact_context(session: &Session) -> Result<Value, String> {
    let state = session.doc().map_err(|e| e.to_string())?;
    let doc = &state.doc;
    // A small, measured font inventory prevents planning from assuming installed glyphs.
    let fonts = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
    let mut families: Vec<String> = ["Tahoma", "Arial", "Noto Sans", "Noto Naskh Arabic", "DejaVu Sans"]
        .into_iter()
        .filter(|family| fonts.has_family(family))
        .take(3)
        .map(str::to_owned)
        .collect();
    if families.is_empty() {
        families.extend(fonts.families().into_iter().take(3));
    }
    let mut font_faces = Vec::new();
    for family in families {
        for style in fonts.styles(&family).into_iter().take(3) {
            let face = fonts.face(&family, &style);
            let missing: String =
                "ABCabc0123456789أابتثجحخدذرزسشصضطظعغفقكلمنهويىة★☆●✓".chars().filter(|character| face.glyph_for(*character) == 0).collect();
            font_faces.push(json!({"family":family,"style":style,"missingProbeCharacters":missing}));
        }
    }
    let mut objects = Vec::new();
    let mut objects_bytes = 0usize;
    let mut objects_truncated = false;
    for (spread_index, spread) in doc.spreads.iter().enumerate() {
        let first_page = doc.first_page_of_spread(spread_index);
        let mut stack: Vec<(&designcraft_engine::doc::Item, designcraft_geom::Affine, Option<u64>, usize)> =
            spread.items.iter().rev().map(|item| (item.as_ref(), designcraft_geom::Affine::IDENTITY, None, 0usize)).collect();
        while let Some((item, parent_xf, parent_group, depth)) = stack.pop() {
            if objects.len() >= 200 || objects_bytes >= 24 * 1024 {
                objects_truncated = true;
                break;
            }
            let bounds = parent_xf.transform_rect_bbox(item.bounds());
            let page_offset = spread.page_at_x(bounds.center().x).unwrap_or(0);
            let page = first_page.saturating_add(page_offset).saturating_add(1);
            let content = match &item.content {
                designcraft_engine::doc::Content::Text(_) => "text",
                designcraft_engine::doc::Content::Graphic(_) => "graphic",
                designcraft_engine::doc::Content::Group { .. } => "group",
                designcraft_engine::doc::Content::Unassigned if item.object_style == doc.styles.default_graphic_frame => "graphic",
                designcraft_engine::doc::Content::Unassigned => "unassigned",
            };
            let shape = match item.shape {
                designcraft_engine::doc::Shape::Rectangle => "rectangle",
                designcraft_engine::doc::Shape::Oval => "ellipse",
                designcraft_engine::doc::Shape::Polygon => "polygon",
                designcraft_engine::doc::Shape::GraphicLine => "line",
                designcraft_engine::doc::Shape::Path => "path",
                designcraft_engine::doc::Shape::Group => "group",
            };
            let story_id = item.text_frame().map(|tf| tf.story.0);
            let story_ref = item.text_frame().and_then(|tf| doc.stories.get(&tf.story));
            let text = story_ref.map(|s| s.text.chars().take(500).collect::<String>());
            let typography = story_ref.and_then(|story| {
                let para = story.paras.first()?;
                let (paragraph, base) = doc.styles.resolve_para(para);
                let chars = doc.styles.resolve_char(&base, story.char_format_at(0));
                Some(json!({
                    "fontFamily": chars.font_family.chars().take(160).collect::<String>(),
                    "fontStyle": chars.font_style.chars().take(80).collect::<String>(),
                    "size": chars.size,
                    "fill": chars.fill.chars().take(160).collect::<String>(),
                    "leading": chars.leading,
                    "direction": paragraph.direction,
                    "align": paragraph.align,
                }))
            });
            let tables = story_ref.filter(|story| !story.tables.is_empty()).map(|story| {
                story
                    .tables
                    .values()
                    .take(8)
                    .map(|table| {
                        let cols = table.columns.len();
                        let cells: Vec<Value> = table
                            .cells
                            .iter()
                            .enumerate()
                            .filter(|(_, cell)| !cell.text.text.trim().is_empty())
                            .take(32)
                            .map(|(idx, cell)| {
                                let row = idx.checked_div(cols).unwrap_or(0);
                                let col = idx.checked_rem(cols).unwrap_or(0);
                                json!({
                                    "row": row,
                                    "col": col,
                                    "text": cell.text.text.chars().take(120).collect::<String>(),
                                    "fill": cell.fill.chars().take(80).collect::<String>(),
                                })
                            })
                            .collect();
                        json!({
                            "table": table.id,
                            "rows": table.rows.len(),
                            "cols": cols,
                            "cells": cells,
                        })
                    })
                    .collect::<Vec<_>>()
            });
            let entry = json!({
                "id": item.id.0,
                "label": item.label,
                "spread": spread_index,
                "page": page,
                "bounds": [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
                "content": content,
                "shape": shape,
                "story": story_id,
                "text": text,
                "fillSwatch": item.fill.swatch.chars().take(160).collect::<String>(),
                "fillTint": item.fill.tint,
                "strokeSwatch": item.stroke.swatch.chars().take(160).collect::<String>(),
                "strokeWeight": item.stroke.weight,
                "locked": item.locked,
                "parentGroup": parent_group,
                "typography": typography,
                "tables": tables,
            });
            let entry_len = serde_json::to_vec(&entry).map(|v| v.len()).unwrap_or(512);
            if !objects.is_empty() && objects_bytes.saturating_add(entry_len) > 24 * 1024 {
                objects_truncated = true;
                break;
            }
            objects_bytes = objects_bytes.saturating_add(entry_len);
            objects.push(entry);
            if depth < 8 && !item.children().is_empty() {
                let child_xf = parent_xf * item.xf;
                for child in item.children().iter().rev() {
                    stack.push((child.as_ref(), child_xf, Some(item.id.0), depth + 1));
                }
            }
        }
        if objects_truncated || objects.len() >= 200 {
            objects_truncated = true;
            break;
        }
    }
    Ok(
        json!({"documentUid":state.uid,"revision":state.revision,"title":doc.title,"pageCount":doc.page_count(),"pageWidth":doc.settings.page_width,"pageHeight":doc.settings.page_height,"facingPages":doc.settings.facing_pages,"selection":state.selection,"objects":objects,"objectsTruncated":objects_truncated,"fontFaces":font_faces,"swatchNames":doc.swatches.iter().take(64).map(|s|s.name.chars().take(160).collect::<String>()).collect::<Vec<_>>(),"units":"points, 72 per inch; rect=[left,top,right,bottom]"}),
    )
}

fn corrected_design_context(session: &Session) -> Result<Value, String> {
    let mut context = compact_context(session)?;
    let doc = &session.doc().map_err(|e| e.to_string())?.doc;
    let mut observations = Vec::new();
    for (spread_index, spread) in doc.spreads.iter().enumerate() {
        for item in &spread.items {
            if observations.len() >= 48 {
                break;
            }
            let typography=item.text_frame().and_then(|frame|doc.stories.get(&frame.story)).and_then(|story| {
                let para=story.paras.first()?;
                let (paragraph,base)=doc.styles.resolve_para(para);
                let chars=doc.styles.resolve_char(&base,story.char_format_at(0));
                Some(json!({"fontFamily":chars.font_family.chars().take(160).collect::<String>(),"fontStyle":chars.font_style.chars().take(80).collect::<String>(),"size":chars.size,"leading":chars.leading,"direction":paragraph.direction,"align":paragraph.align,"spaceBefore":paragraph.space_before,"spaceAfter":paragraph.space_after}))
            });
            let bounds = item.visible_bounds();
            observations.push(json!({"spread":spread_index,"visibleBounds":[bounds.x0,bounds.y0,bounds.x1,bounds.y1],"fillSwatch":item.fill.swatch.chars().take(160).collect::<String>(),"fillTint":item.fill.tint,"strokeSwatch":item.stroke.swatch.chars().take(160).collect::<String>(),"strokeWeight":item.stroke.weight,"typography":typography}));
        }
        if observations.len() >= 48 {
            break;
        }
    }
    context["measuredDesignObservations"] = json!(observations);
    context["designObservationScope"] = json!(
        "At most 48 top-level items; first text paragraph/run only; values resolved from actual native styles. Not full-document coverage or inferred before-state."
    );
    Ok(context)
}
fn read_json(access: &std::sync::Arc<file_access::AutomationRoots>, path: &Path, max: u64) -> Result<Option<Value>, String> {
    access.check(path, file_access::Access::Read)?;
    match file_access::confine(Some(access), || designcraft_engine::io::read_capped(path, max)) {
        Ok(bytes) if bytes.len() as u64 > max => Err(format!("Configuration file exceeds limit: {}", path.display())),
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn write_json(access: &file_access::AutomationRoots, path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    write_profile(access, path, &bytes)
}
fn write_profile(access: &file_access::AutomationRoots, path: &Path, bytes: &[u8]) -> Result<(), String> {
    access.check(path, file_access::Access::Write)?;
    designcraft_engine::atomic::write_atomic(path, bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai23_native_only_ignores_unused_invalid_image_endpoint_in_settings_and_batch_admission() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let settings = AiSettings {
            output_mode: designcraft_ai_types::OutputMode::NativeOnly,
            text_model: designcraft_ai_types::ModelSelection {
                provider: ProviderKind::Ollama,
                model: "ai23-no-network".into(),
                endpoint: Some("http://127.0.0.1:9/".into()),
            },
            image_model: designcraft_ai_types::ModelSelection {
                provider: ProviderKind::OpenAi,
                model: "unused-image".into(),
                endpoint: Some("http://untrusted-image.invalid/v1/".into()),
            },
            // Every request reserves at least 1024 input tokens. Admission fails
            // before provider traffic even if the worker starts immediately.
            budget: BudgetSpec { token_limit: 256, max_output_tokens: 1, max_requests: 1, repair_attempts: 0, ..Default::default() },
            pages_per_batch: 1,
            ..Default::default()
        };
        assert!(validate_settings(&settings).is_ok(), "Native-only settings must not validate an unused image endpoint");
        let mut illustrated = settings.clone();
        illustrated.output_mode = designcraft_ai_types::OutputMode::NativeWithImages;
        assert!(validate_settings(&illustrated).is_err(), "An image-capable task must still validate that endpoint");
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        // Match the synthetic text endpoint in memory, avoiding protected-store
        // access during admission. It cannot authorize the unrelated image URL.
        runner.keys.push(SessionKey::new(&settings.text_model, SecretString::new("synthetic-ai23-local-key".into())).unwrap());
        let mut session = Session::new();
        runner
            .start(
                &mut session,
                TaskRequest {
                    prompt: "Create one editable native page.".into(),
                    settings,
                    skill_ids: vec![],
                    auto_select_skills: false,
                    target: TaskTarget::NewA4 { pages: 1 },
                    export_directory: None,
                },
            )
            .unwrap();
        assert!(runner.pending.is_some(), "The native batch must reach worker admission without touching image credentials");
        assert!(runner.snapshot.busy);
        assert_eq!(runner.snapshot.batch.as_ref().unwrap().completed_pages, 0);
        assert!(session.documents().is_empty(), "Admission must not publish an unreviewed document");
    }

    #[test]
    fn continuation_saved_draft_survives_model_validation_and_restart_without_authority() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(4)).unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let request = TaskRequest {
            prompt: "Keep the exact bilingual book instructions العربية without losing this draft".into(),
            target: TaskTarget::ContinueBook,
            settings: AiSettings::default(),
            skill_ids: vec![],
            auto_select_skills: false,
            export_directory: None,
        };
        let state = runner.handle(&mut session, AiRequest::Start(request.clone()));
        assert!(!state.busy, "Missing model must not launch a provider");
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["production_draft"]["request"]["prompt"], request.prompt);
        assert!(dir.path().join("production-draft.json").is_file());
        drop(runner);
        let mut reopened = ProductionRunner::open(dir.path()).unwrap();
        let state = reopened.handle(&mut session, AiRequest::Poll);
        let value = serde_json::to_value(state).unwrap();
        assert_eq!(value["production_draft"]["request"]["prompt"], request.prompt);
        assert!(value["production_draft"]["document_uid"].is_null(), "A process-local UID is never resumed from disk");
        assert!(reopened.pending.is_none() && reopened.batch.is_none());
    }

    // Context bounding is a pure transformation of a pinned snapshot. Load the
    // verified draft data without activating/rendering a bilingual sample: font
    // availability and system-font initialization order belong to sample tests.
    fn native_style_context_fixture() -> SkillSnapshot {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let id = "early-years-clear-style";
        let card = runner.snapshot.skills.iter().find(|card| card.id == id).unwrap();
        let record = runner.registry.revision(id, &card.revision).unwrap();
        let files = runner.registry.review_files(id, &card.revision).unwrap();
        SkillSnapshot {
            skill_id: id.into(),
            revision: card.revision.clone(),
            instructions: String::from_utf8(files.get("SKILL.md").unwrap().clone()).unwrap(),
            references: Default::default(),
            reference_hashes: Default::default(),
            templates: runner.registry.review_templates(id, &card.revision).unwrap(),
            design_profile: record.manifest.design_profile.clone(),
        }
    }

    fn automatic_template_batch_fixture() -> (tempfile::TempDir, ProductionRunner) {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/kg-oxford-cambridge-style");
        let revision = runner.registry.import_folder(source, Provenance::default()).unwrap();
        // Isolated test evidence avoids font-dependent sample rendering. This
        // approval exists only in this temporary fixture registry.
        let artifact = directory.path().join("synthetic-capacity-review.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255])).save(&artifact).unwrap();
        runner
            .registry
            .approve(
                &revision.skill_id,
                &revision.revision,
                ApprovalEvidence {
                    reviewer: "Synthetic capacity test".into(),
                    sample_artifact: artifact,
                    executed_actions: runner.registry.sample_actions(&revision.skill_id, &revision.revision).unwrap(),
                    supported_commands: supported_commands(),
                    notes: "Synthetic admission fixture; no human proof approval or provider request.".into(),
                },
            )
            .unwrap();
        let pinned = runner.registry.snapshot(&revision.skill_id, &[]).unwrap();
        for mode in [designcraft_ai_types::OutputMode::NativeOnly, designcraft_ai_types::OutputMode::NativeWithImages] {
            assert_eq!(crate::template_plan::capacity(&pinned, mode).unwrap(), 2);
        }
        (directory, runner)
    }

    #[test]
    fn ai23_local_curriculum_preview_never_launches_generation_or_edits_documents() {
        let (_directory, mut runner) = automatic_template_batch_fixture();
        let mut session = Session::new();
        let mut request = TaskRequest {
            prompt: "Create a Science book".into(),
            settings: AiSettings::default(),
            skill_ids: vec!["kg-oxford-cambridge-style".into()],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 48 },
            export_directory: None,
        };
        runner.preview_production(&session, request.clone()).unwrap();
        let preview = runner.snapshot.production_preview.as_ref().unwrap();
        assert_eq!(preview["total_pages"], 48);
        assert!(preview["blueprint"]["subject_domain"].as_str().unwrap().contains("Science"));
        assert!(runner.pending.is_none() && runner.batch.is_none());
        assert!(session.documents().is_empty());
        assert_eq!(runner.snapshot.current_usage.requests, 0);
        session.execute("file.new", &json!({"pages":6,"facingPages":false})).unwrap();
        for page in 0..2 {
            session.execute("frame.create", &json!({"spread":page,"rect":[60,60,100,100]})).unwrap();
        }
        let before = session.doc().unwrap().doc.clone();
        request.target = TaskTarget::ContinueBook;
        request.settings.right_to_left_binding = Some(true);
        runner.preview_production(&session, request.clone()).unwrap();
        let preview = runner.snapshot.production_preview.as_ref().unwrap();
        assert_eq!(preview["binding"], "LTR");
        assert_eq!(preview["blueprint"]["scoped_page_plan"][0]["page"], 2);
        assert!(std::sync::Arc::ptr_eq(&before, &session.doc().unwrap().doc));
        request.target = TaskTarget::NewA4 { pages: 1001 };
        assert!(runner.preview_production(&session, request).is_err());
        assert!(runner.snapshot.production_preview.is_none(), "Failed previews must not display stale plans");
    }

    fn no_provider_template_settings(requested: u32) -> AiSettings {
        AiSettings {
            text_model: designcraft_ai_types::ModelSelection {
                provider: ProviderKind::Ollama,
                model: "capacity-test-no-provider".into(),
                endpoint: Some("http://127.0.0.1:9/".into()),
            },
            output_mode: designcraft_ai_types::OutputMode::NativeWithImages,
            pages_per_batch: requested,
            // Every reservation includes at least 1024 input tokens. This
            // valid smaller budget prevents all provider traffic, even if
            // the worker runs before the test inspects its pinned scope.
            budget: BudgetSpec { token_limit: 256, max_output_tokens: 1, max_requests: 1, repair_attempts: 0, ..Default::default() },
            ..Default::default()
        }
    }

    fn assert_automatic_template_batch(total_pages: u32, requested: u32, total_batches: u32) {
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let (_directory, mut runner) = automatic_template_batch_fixture();
        let mut session = Session::new();
        let request = TaskRequest {
            prompt: "Create a complete editable kindergarten science book, preserving all requested pages.".into(),
            settings: no_provider_template_settings(requested),
            skill_ids: vec!["kg-oxford-cambridge-style".into()],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: total_pages },
            export_directory: None,
        };
        runner.start(&mut session, request).unwrap();
        let batch = runner.snapshot.batch.as_ref().expect("Positive review groups require proof approval even for a single group");
        assert_eq!(batch.total_pages, total_pages, "Admission must preserve the complete book");
        assert_eq!(batch.pages_per_batch, requested, "Internal capacity must never change the requested human review count");
        assert_eq!(batch.total_batches, total_batches);
        assert_eq!(batch.completed_pages, 0);
        assert_eq!(batch.current_batch, 1);
        let pending = runner.pending.as_ref().unwrap();
        assert_eq!(pending.task.as_ref().unwrap().settings.pages_per_batch, requested, "Keep the user's requested preference in task evidence");
        assert!(matches!(pending.task.as_ref().unwrap().target, TaskTarget::NewA4 { pages } if pages == total_pages));
        assert!(session.documents().is_empty(), "Admission must not publish an unreviewed document");
        pending.worker.cancel.store(true, Ordering::Relaxed);
    }

    #[test]
    fn template_admission_48_pages_keeps_four_page_review_groups() {
        assert_automatic_template_batch(48, 4, 12);
    }

    #[test]
    fn template_admission_48_pages_keeps_twelve_page_review_groups() {
        assert_automatic_template_batch(48, 12, 4);
    }

    #[test]
    fn template_admission_48_pages_keeps_thirty_two_page_review_groups() {
        assert_automatic_template_batch(48, 32, 2);
    }

    #[test]
    fn template_admission_short_book_keeps_its_single_review_gate() {
        assert_automatic_template_batch(4, 12, 1);
    }

    #[test]
    fn template_admission_49_pages_keeps_the_review_remainder() {
        assert_automatic_template_batch(49, 12, 5);
    }

    #[test]
    fn template_admission_allows_requested_review_counts_above_internal_limits() {
        assert_automatic_template_batch(48, 64, 1);
        assert_automatic_template_batch(1000, 1000, 1);
    }

    #[test]
    fn template_admission_preserves_long_arabic_briefs_verbatim() {
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let (_directory, mut runner) = automatic_template_batch_fixture();
        let mut session = Session::new();
        let prompt = "Create all 48 requested pages.\nصَمِّمْ مَنْهَجَ رِيَاضِيَّاتٍ مُتَكَامِلًا وَاحْفَظْ هَذَا النَّصَّ كَامِلًا.\n".repeat(512);
        assert!(prompt.len() > 32_768 && prompt.len() < MAX_BRIEF_BYTES);
        let request = TaskRequest {
            prompt: prompt.clone(),
            settings: no_provider_template_settings(12),
            skill_ids: vec!["kg-oxford-cambridge-style".into()],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 48 },
            export_directory: None,
        };
        runner.start(&mut session, request).unwrap();
        let pending = runner.pending.as_ref().unwrap();
        assert_eq!(pending.task.as_ref().unwrap().prompt, prompt);
        assert!(session.documents().is_empty());
        pending.worker.cancel.store(true, Ordering::Relaxed);
    }

    #[test]
    fn template_admission_rejects_briefs_above_the_shared_byte_limit_before_launch() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        let request = TaskRequest {
            prompt: "x".repeat(MAX_BRIEF_BYTES + 1),
            settings: no_provider_template_settings(12),
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 48 },
            export_directory: None,
        };
        let error = runner.start(&mut session, request).unwrap_err();
        assert!(error.contains(&MAX_BRIEF_BYTES.to_string()), "{error}");
        assert!(runner.pending.is_none());
        assert!(session.documents().is_empty());
    }

    fn assert_current_document_template_guidance(has_content: bool) {
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let (_directory, mut runner) = automatic_template_batch_fixture();
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":2,"facingPages":false})).unwrap();
        if has_content {
            session.execute("frame.create", &json!({"spread":0,"rect":[20,30,100,120],"content":"unassigned"})).unwrap();
        }
        let original = session.doc().unwrap().clone();
        let request = TaskRequest {
            prompt: "Design one editable sample on page 2 of this opened document, preserving its actual geometry.".into(),
            settings: no_provider_template_settings(12),
            skill_ids: vec!["kg-oxford-cambridge-style".into()],
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: None,
        };
        runner.start(&mut session, request).unwrap();
        let pending = runner.pending.as_ref().unwrap();
        assert_eq!(pending.expected, Some((original.uid, original.revision)));
        assert!(matches!(pending.task.as_ref().unwrap().target, TaskTarget::CurrentDocument));
        assert_eq!(pending.skills.len(), 1, "The selected style remains pinned guidance");
        assert!(runner.snapshot.batch.is_none());
        assert_eq!(session.documents().len(), 1);
        let current = session.doc().unwrap();
        assert!(std::sync::Arc::ptr_eq(&original.doc, &current.doc), "Admission must not resize or edit the opened document");
        assert_eq!((current.uid, current.revision), (original.uid, original.revision));
        assert_eq!(current.history.undo.len(), original.history.undo.len());
        pending.worker.cancel.store(true, Ordering::Relaxed);
    }

    #[test]
    fn template_admission_current_blank_document_retains_its_actual_geometry() {
        assert_current_document_template_guidance(false);
    }

    #[test]
    fn template_admission_current_existing_document_retains_its_actual_geometry() {
        assert_current_document_template_guidance(true);
    }

    #[test]
    fn new_document_context_and_publication_use_the_same_pinned_profile_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let card = runner.snapshot.skills.iter().find(|card| card.id == "early-years-clear-style").unwrap().clone();
        let mut manifest = runner.registry.revision(&card.id, &card.revision).unwrap().manifest.clone();
        let profile = manifest.design_profile.as_mut().unwrap();
        profile.page_mm = [240.0, 320.0];
        profile.margin_mm = 10.0;
        let profile = profile.clone();
        let mut files = runner.registry.review_files(&card.id, &card.revision).unwrap();
        files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
        let pinned = runner.registry.import_files(files.clone(), Provenance::default()).unwrap();
        // A later revision must never change the geometry of an in-flight task.
        manifest.design_profile.as_mut().unwrap().page_mm = [250.0, 350.0];
        files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
        runner.registry.import_files(files, Provenance::default()).unwrap();
        let pins = vec![(pinned.skill_id.clone(), pinned.revision.clone())];
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let original = session.doc().unwrap().clone();
        let target = TaskTarget::NewA4 { pages: 2 };
        let (base, context) = task_document_context(&session, &target, Some(&profile), "", &AiSettings::default()).unwrap();
        assert!(!context.is_null());
        let page = &base.doc.spreads[0].pages[0];
        assert!((page.width - 240.0 * 72.0 / 25.4).abs() < 0.001);
        assert!((page.height - 320.0 * 72.0 / 25.4).abs() < 0.001);
        assert!((page.margins.top - 10.0 * 72.0 / 25.4).abs() < 0.001);
        assert!(std::sync::Arc::ptr_eq(&original.doc, &session.doc().unwrap().doc));
        let task = TaskRequest {
            prompt: "Create a native document in the selected print geometry".into(),
            target,
            settings: Default::default(),
            skill_ids: vec![pinned.skill_id],
            auto_select_skills: false,
            export_directory: None,
        };
        let mut report = TaskReport {
            design_style: None,
            id: "pinned-print-geometry".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: "synthetic-local".into(),
            settings: None,
            skills: pins,
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
        };
        let plan = ProductionPlan {
            title: "Native print geometry".into(),
            commands: vec![designcraft_ai_types::EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":0,"rect":[50,50,100,100],"content":"unassigned"}),
            }],
            images: vec![],
        };
        runner.publish(&mut session, &Some(task), None, worker::Product::Plan(plan), &mut report).unwrap();
        let published = session.doc().unwrap();
        crate::design::check_geometry(None, published, &profile).unwrap();
        let page = &published.doc.spreads[0].pages[0];
        assert!((page.margins.top - 10.0 * 72.0 / 25.4).abs() < 0.001);
        assert_eq!(session.documents().len(), 2);
        session.execute("file.activate", &json!({"index":0})).unwrap();
        assert!(std::sync::Arc::ptr_eq(&original.doc, &session.doc().unwrap().doc));
        assert!(runner.pinned_document_params(&[("missing".into(), "missing".into())], 1).is_err());
    }

    #[test]
    fn template_tasks_keep_skill_guidance_without_loading_unused_command_examples() {
        let style = native_style_context_fixture();
        assert!(crate::template_plan::eligible(&style));
        let mut guidance = style.clone();
        guidance.skill_id = "large-native-guidance".into();
        guidance.design_profile = None;
        guidance.instructions = "Preserve actual educational guidance".into();
        guidance.references.insert("references/rules.md".into(), "Use short independent activities".into());
        guidance.templates = std::collections::BTreeMap::from([(
            "templates/example.json".into(),
            vec![designcraft_ai_types::EngineAction { command: "frame.create".into(), params: json!({"text":"x".repeat(65 * 1024)}) }],
        )]);
        let originals = vec![guidance.clone(), style.clone()];
        let command_plan = bounded_task_skills(originals.clone(), &TaskTarget::CurrentDocument).unwrap();
        assert!(command_plan[0].templates.is_empty(), "Oversized optional examples must not exclude actual guidance");
        let command_plan = bounded_task_skills(vec![guidance.clone()], &TaskTarget::ContinueBook).unwrap();
        assert!(command_plan[0].templates.is_empty());
        for target in [TaskTarget::ContinueBook, TaskTarget::NewA4 { pages: 48 }] {
            let bounded = bounded_task_skills(originals.clone(), &target).unwrap();
            assert!(bounded[0].templates.is_empty());
            assert_eq!(bounded[0].instructions, guidance.instructions);
            assert_eq!(bounded[0].references, guidance.references);
            assert_eq!(bounded[0].revision, guidance.revision);
            assert_eq!(bounded[0].reference_hashes, guidance.reference_hashes);
            assert_eq!(serde_json::to_value(&bounded[1]).unwrap(), serde_json::to_value(&style).unwrap());
        }
        guidance.instructions = "x".repeat(65 * 1024);
        assert!(bounded_task_skills(vec![guidance, style], &TaskTarget::ContinueBook).is_err(), "The context guard still limits actual guidance");
    }

    #[test]
    fn bounded_skill_context_keeps_guidance_pins_small_examples_and_executable_profiles() {
        let mut guidance = SkillSnapshot {
            skill_id: "kg-guidance".into(),
            revision: "immutable-revision".into(),
            instructions: "Keep Arabic copy exactly: تَصْمِيم صَفَحَات".into(),
            references: std::collections::BTreeMap::from([("references/layout.md".into(), "Exact safe-area guidance".into())]),
            reference_hashes: std::collections::BTreeMap::from([("references/layout.md".into(), "pinned-sha256".into())]),
            templates: std::collections::BTreeMap::from([
                (
                    "templates/oversized.json".into(),
                    vec![EngineAction { command: "frame.create".into(), params: json!({"text":"x".repeat(111_399)}) }],
                ),
                ("templates/small.json".into(), vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[54,54,100,100]}) }]),
            ]),
            design_profile: None,
        };
        let original = guidance.clone();
        let bounded = bounded_task_skills(vec![guidance.clone()], &TaskTarget::CurrentDocument).unwrap();
        assert!(!bounded[0].templates.contains_key("templates/oversized.json"));
        assert_eq!(
            serde_json::to_value(&bounded[0].templates["templates/small.json"]).unwrap(),
            serde_json::to_value(&original.templates["templates/small.json"]).unwrap()
        );
        guidance.templates.remove("templates/oversized.json");
        assert_eq!(serde_json::to_value(&bounded[0]).unwrap(), serde_json::to_value(&guidance).unwrap());
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= 64 * 1024);
        assert_eq!(
            serde_json::to_value(&bounded_task_skills(vec![guidance.clone()], &TaskTarget::CurrentDocument).unwrap()[0]).unwrap(),
            serde_json::to_value(&guidance).unwrap()
        );
        guidance.references.insert("references/oversized.md".into(), "x".repeat(65 * 1024));
        assert!(
            bounded_task_skills(vec![guidance], &TaskTarget::CurrentDocument).is_err(),
            "Actual reference guidance must never be silently truncated"
        );

        let mut executable = native_style_context_fixture();
        executable.templates.insert("templates/required.json".into(), original.templates["templates/oversized.json"].clone());
        let bounded = bounded_task_skills(vec![executable.clone()], &TaskTarget::CurrentDocument).unwrap();
        executable.templates.clear();
        assert_eq!(
            serde_json::to_value(&bounded[0]).unwrap(),
            serde_json::to_value(&executable).unwrap(),
            "The current document keeps pinned style guidance without fixed page command examples"
        );
    }

    #[test]
    fn compiler_context_bounds_guidance_and_profile_rules_without_sending_native_templates() {
        let mut executable = native_style_context_fixture();
        let valid_actions = executable.templates.values().max_by_key(|actions| actions.len()).unwrap().clone();
        for index in 0..12 {
            executable.templates.insert(format!("templates/valid-copy-{index}.json"), valid_actions.clone());
        }
        assert!(task_skill_context_bytes(std::slice::from_ref(&executable)).unwrap() > 64 * 1024);
        let current = bounded_task_skills(vec![executable.clone()], &TaskTarget::CurrentDocument).unwrap();
        assert!(current[0].templates.is_empty());
        assert_eq!(current[0].instructions, executable.instructions);
        assert_eq!(current[0].revision, executable.revision);
        for target in [TaskTarget::ContinueBook, TaskTarget::NewA4 { pages: 48 }] {
            let bounded = bounded_task_skills(vec![executable.clone()], &target).unwrap();
            assert_eq!(
                serde_json::to_value(&bounded[0]).unwrap(),
                serde_json::to_value(&executable).unwrap(),
                "Compiler templates remain immutable local inputs"
            );
        }
        let mut oversized = executable.clone();
        oversized.instructions = "x".repeat(33 * 1024);
        assert!(bounded_task_skills(vec![oversized], &TaskTarget::NewA4 { pages: 1 }).is_err());
        executable.design_profile.as_mut().unwrap().layout.push("x".repeat(33 * 1024));
        assert!(bounded_task_skills(vec![executable], &TaskTarget::ContinueBook).is_err());
    }

    #[test]
    fn corrected_design_context_uses_resolved_native_type_and_color_values() {
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        session.execute("frame.create", &json!({"rect":[60,60,400,120],"content":"text","text":"Native sample","caret":false})).unwrap();
        session
            .execute("type.char", &json!({"attrs":{"fontFamily":"Tahoma","fontStyle":"Regular","size":21,"leading":{"kind":"points","value":30}}}))
            .unwrap();
        session.execute("type.para", &json!({"attrs":{"align":"right","direction":"rightToLeft"}})).unwrap();
        session.execute("object.fill", &json!({"swatch":"C=100 M=0 Y=0 K=0"})).unwrap();
        let context = corrected_design_context(&session).unwrap();
        let observed = &context["measuredDesignObservations"][0];
        assert_eq!(observed["typography"]["fontFamily"], "Tahoma");
        assert_eq!(observed["typography"]["size"].as_f64(), Some(21.0));
        assert_eq!(observed["typography"]["align"], "right");
        assert_eq!(observed["fillSwatch"], "C=100 M=0 Y=0 K=0");
        assert!(context["swatchNames"].as_array().unwrap().iter().any(|v| v == "[Black]"));
    }

    #[test]
    fn design_style_corrections_pin_revisions_preserve_rollback_and_reject_failed_updates() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let original = runner.registry.catalog("").into_iter().flat_map(|e| e.revisions).find(|r| r.manifest.design_profile.is_some()).unwrap();
        for entry in runner.registry.catalog("") {
            runner.registry.set_enabled(&entry.skill_id, entry.skill_id == original.skill_id).unwrap();
        }
        let mut files = runner.registry.review_files(&original.skill_id, &original.revision).unwrap();
        let mut manifest = original.manifest;
        manifest.required_commands = vec!["frame.create".into()];
        for family in &mut manifest.design_profile.as_mut().unwrap().page_families {
            family.text_slots.clear();
        }
        files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
        for path in &manifest.templates {
            files.insert(path.clone(), serde_json::to_vec(&json!([{"command":"frame.create","params":{"rect":[60,60,120,120]}}])).unwrap());
        }
        let record = runner.registry.import_files(files.clone(), Provenance::default()).unwrap();
        let mut session = Session::new();
        let selected = runner.prepare_skills_for_style(&session, "kindergarten", None).unwrap();
        assert_eq!(selected.as_slice(), std::slice::from_ref(&record.skill_id));
        let pinned = runner.registry.snapshot(&record.skill_id, &[]).unwrap();
        runner
            .dispatch(
                &mut session,
                AiRequest::AddDesignCorrection {
                    id: record.skill_id.clone(),
                    revision: record.revision.clone(),
                    correction: designcraft_ai_types::DesignCorrection {
                        before: "Crowded layout".into(),
                        after: "Two aligned tasks with answer space".into(),
                        reason: "Pencil room".into(),
                    },
                },
            )
            .unwrap();
        assert_eq!(runner.registry.snapshot(&record.skill_id, &[]).unwrap().revision, record.revision);
        runner.prepare_style(&session, &record.skill_id).unwrap();
        let revised = runner.registry.snapshot(&record.skill_id, &[]).unwrap();
        assert_ne!(revised.revision, pinned.revision);
        assert_eq!(revised.design_profile.unwrap().corrections.len(), pinned.design_profile.as_ref().unwrap().corrections.len() + 1);
        assert_eq!(pinned.revision, record.revision);
        runner.registry.rollback(&record.skill_id, &record.revision).unwrap();
        runner.prepare_style(&session, &record.skill_id).unwrap();
        assert_eq!(runner.registry.snapshot(&record.skill_id, &[]).unwrap().revision, record.revision);
        // An invalid native template cannot replace the active style or break later tasks.
        files.insert(manifest.templates[0].clone(), serde_json::to_vec(&json!([{"command":"frame.create","params":{"rect":[0,0,10,10]}}])).unwrap());
        let broken = runner.registry.import_files(files, Provenance::default()).unwrap();
        runner.prepare_style(&session, &record.skill_id).unwrap();
        assert_eq!(runner.registry.snapshot(&record.skill_id, &[]).unwrap().revision, record.revision);
        assert_eq!(runner.registry.revision(&record.skill_id, &broken.revision).unwrap().status, RevisionStatus::Draft);
        assert!(runner.snapshot.reports.is_empty(), "Local style checks must not call a provider");
        runner.registry.set_enabled(&record.skill_id, false).unwrap();
        assert!(runner.prepare_style(&session, &record.skill_id).is_err());
    }

    #[test]
    fn planning_context_reports_measured_font_glyph_coverage() {
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let context = compact_context(&session).unwrap();
        let faces = context["fontFaces"].as_array().unwrap();
        assert!(!faces.is_empty() && faces.len() <= 9);
        let fonts = designcraft_fonts::FontDb::global().scoped(session.doc().unwrap().doc.font_scope);
        for value in faces {
            let face = fonts.face(value["family"].as_str().unwrap(), value["style"].as_str().unwrap());
            assert_eq!(value["missingProbeCharacters"].as_str().unwrap().contains('★'), face.glyph_for('★') == 0);
        }
    }

    #[test]
    fn automatic_skills_validate_real_samples_without_human_approval_or_provider_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        for entry in runner.registry.catalog("") {
            runner.registry.set_enabled(&entry.skill_id, false).unwrap();
        }
        let session = Session::new();
        let manifest = json!({"schema_version":1,"skill_id":"automatic-fixture","required_commands":["frame.create"],
            "templates":["templates/sample.json"],"sample_template":"templates/sample.json"});
        let files = designcraft_ai_skills::PackageFiles::from([
            (
                "SKILL.md".into(),
                b"---\nname: automatic-fixture\ndescription: fixture automatic layout\n---\nUse editable native elements.\n".to_vec(),
            ),
            ("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap()),
            (
                "templates/sample.json".into(),
                serde_json::to_vec(&json!([
                    {"command":"frame.create","params":{"rect":[54,54,120,120],"content":"unassigned"}}
                ]))
                .unwrap(),
            ),
        ]);
        let revision = runner.registry.import_files(files.clone(), Provenance::default()).unwrap();
        let reports = runner.snapshot.reports.len();
        let ids = runner.prepare_automatic_skills(&session, "fixture layout").unwrap();
        assert!(ids.contains(&revision.skill_id));
        let pinned = runner.registry.snapshot(&revision.skill_id, &[]).unwrap();
        let record = runner.registry.revision(&revision.skill_id, &revision.revision).unwrap();
        assert!(record.approval.as_ref().unwrap().automatic);
        runner.dispatch(&mut Session::new(), AiRequest::ApproveSkill { id: revision.skill_id.clone(), revision: revision.revision.clone() }).unwrap();
        assert!(
            !runner.registry.revision(&revision.skill_id, &revision.revision).unwrap().approval.as_ref().unwrap().automatic,
            "a subsequent explicit human review must remain possible"
        );
        assert_eq!(runner.snapshot.reports.len(), reports, "local validation must not call a provider");
        assert!(session.documents().is_empty(), "samples must not alter the user's document");
        runner.registry.set_enabled(&revision.skill_id, false).unwrap();
        assert!(!runner.prepare_automatic_skills(&session, "fixture layout").unwrap().contains(&revision.skill_id));
        runner.registry.set_enabled(&revision.skill_id, true).unwrap();
        let mut broken = files;
        broken.insert(
            "templates/sample.json".into(),
            serde_json::to_vec(&json!([
                {"command":"frame.create","params":{"rect":"bad rectangle"}}
            ]))
            .unwrap(),
        );
        let rejected = runner.registry.import_files(broken, Provenance::default()).unwrap();
        assert!(runner.prepare_automatic_skills(&session, "fixture layout").unwrap().contains(&revision.skill_id));
        assert_eq!(runner.snapshot.skill_warnings.len(), 1);
        assert_eq!(runner.registry.snapshot(&revision.skill_id, &[]).unwrap().revision, pinned.revision);
        assert_eq!(runner.registry.revision(&revision.skill_id, &rejected.revision).unwrap().status, RevisionStatus::Draft);
        // A broken imported skill with no approved revision must not block the
        // valid skill, acquire activation evidence, or alter the live document.
        let bad_manifest = json!({"schema_version":1,"skill_id":"broken-fixture","required_commands":["frame.create"],
            "templates":["templates/sample.json"],"sample_template":"templates/sample.json"});
        let bad = runner
            .registry
            .import_files(
                designcraft_ai_skills::PackageFiles::from([
                    ("SKILL.md".into(), b"---\nname: broken-fixture\ndescription: fixture layout\n---\nNative sample.\n".to_vec()),
                    ("designcraft.json".into(), serde_json::to_vec(&bad_manifest).unwrap()),
                    (
                        "templates/sample.json".into(),
                        serde_json::to_vec(&json!([
                            {"command":"frame.create","params":{"rect":"bad rectangle"}}
                        ]))
                        .unwrap(),
                    ),
                ]),
                Provenance::default(),
            )
            .unwrap();
        let selected = runner.prepare_automatic_skills(&session, "fixture layout").unwrap();
        assert!(selected.contains(&revision.skill_id));
        assert!(!selected.contains(&bad.skill_id));
        assert_eq!(runner.snapshot.skill_warnings.len(), 2);
        assert!(runner.registry.revision(&bad.skill_id, &bad.revision).unwrap().approval.is_none());
        assert!(session.documents().is_empty());
        assert_eq!(runner.snapshot.reports.len(), reports);
    }

    #[test]
    fn new_document_publication_preserves_active_ime_before_any_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        session.execute("frame.create", &json!({"rect":[50,50,300,200],"content":"text","text":"Existing text"})).unwrap();
        session.set_tool("type");
        session.tool_preedit("pending IME", Some(3..3), designcraft_engine::ViewInfo::default()).unwrap();
        assert!(session.tool_composing());
        let before = session.doc().unwrap().clone();
        let journal = session.journal.clone();
        let active = session.active_index();
        let count = session.documents().len();
        let task = TaskRequest {
            prompt: "New document".into(),
            settings: Default::default(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 1 },
            export_directory: None,
        };
        let plan = ProductionPlan {
            title: "Refused publication".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,200,200],"content":"unassigned"}) }],
            images: vec![],
        };
        let mut report = TaskReport {
            design_style: None,
            id: "ime-publication".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: "mock".into(),
            settings: None,
            skills: vec![],
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
        };
        let error = runner.publish(&mut session, &Some(task), None, worker::Product::Plan(plan), &mut report).unwrap_err();
        assert!(error.contains("interaction"), "{error}");
        assert!(session.tool_composing(), "Refusal must not commit marked text");
        let after = session.doc().unwrap();
        assert_eq!(session.active_index(), active);
        assert_eq!(session.documents().len(), count);
        assert_eq!((after.uid, after.revision), (before.uid, before.revision));
        assert!(std::sync::Arc::ptr_eq(&after.doc, &before.doc));
        assert_eq!(after.selection, before.selection);
        assert_eq!(after.interaction.as_ref().map(|i| (&i.label, i.revision)), before.interaction.as_ref().map(|i| (&i.label, i.revision)));
        assert_eq!(after.history.undo.len(), before.history.undo.len());
        assert_eq!(after.history.redo.len(), before.history.redo.len());
        assert!(
            after
                .history
                .undo
                .iter()
                .zip(&before.history.undo)
                .all(|(a, b)| a.label == b.label && a.selection == b.selection && std::sync::Arc::ptr_eq(&a.doc, &b.doc))
        );
        assert_eq!(session.journal, journal);
        assert!(report.artifacts.is_empty());
        assert!(!dir.path().join("runs").exists(), "No trace or output may precede the idle guard");
    }

    #[cfg(any(windows, unix))]
    fn profile_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let output = std::process::Command::new("cmd")
                .creation_flags(0x08000000)
                .args(["/D", "/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(output.status.success(), "junction fixture failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    #[cfg(any(windows, unix))]
    fn private_profile_links_cannot_redirect_reads_samples_or_trace_writes() {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel.json");
        std::fs::write(&sentinel, b"{\"private\":true}").unwrap();
        let mut runner = ProductionRunner::open(profile.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let before = session.doc().unwrap().clone();
        for name in ["runs", "samples"] {
            profile_link(outside.path(), &profile.path().join(name));
        }
        let redirected = profile.path().join("runs/sentinel.json");
        assert!(read_json(&runner.profile_access, &redirected, 1024).unwrap_err().contains("outside the read root"));
        assert!(write_json(&runner.profile_access, &redirected, &json!({"changed":true})).unwrap_err().contains("outside the write root"));
        let skill = runner.snapshot.skills[0].clone();
        let error = runner.run_sample(&session, &skill.id, &skill.revision).unwrap_err();
        assert!(error.contains("outside the write root"), "{error}");
        let task = TaskRequest {
            prompt: "test".into(),
            settings: Default::default(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 1 },
            export_directory: None,
        };
        let plan = ProductionPlan {
            title: "test".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,100,100]}) }],
            images: vec![],
        };
        let mut report = TaskReport {
            design_style: None,
            id: "profile-escape".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: String::new(),
            settings: None,
            skills: vec![],
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
        };
        let error = runner.publish(&mut session, &Some(task), None, worker::Product::Plan(plan), &mut report).unwrap_err();
        assert!(error.contains("outside the write root"), "{error}");
        assert_eq!(session.documents().len(), 1);
        assert!(std::sync::Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert_eq!(before.history.undo.len(), session.doc().unwrap().history.undo.len());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"{\"private\":true}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
        for name in ["runs", "samples"] {
            let link = profile.path().join(name);
            std::fs::remove_dir(&link).or_else(|_| std::fs::remove_file(&link)).unwrap();
        }
    }

    #[test]
    #[cfg(any(windows, unix))]
    fn startup_profile_source_paths_are_checked_before_reading() {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = profile.path().join("settings.json");
        profile_link(outside.path(), &link);
        let error = ProductionRunner::open(profile.path()).err().unwrap();
        assert!(error.contains("outside the read root"), "{error}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_dir(&link).or_else(|_| std::fs::remove_file(&link)).unwrap();
    }

    #[test]
    fn confined_requests_reject_external_paths_before_side_effects_or_workers() {
        let profile = tempfile::tempdir().unwrap();
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = file_access::AutomationRoots::new(Some(allowed.path()), Some(allowed.path())).unwrap().unwrap();
        let mut runner = ProductionRunner::open(profile.path()).unwrap();
        let mut session = Session::new();
        let outside_path = outside.path().join("not-created").to_string_lossy().into_owned();
        let requests = [
            AiRequest::ImportSkill { path: outside_path.clone() },
            AiRequest::ExportSkill { id: "missing".into(), revision: "missing".into(), path: outside_path.clone() },
            AiRequest::GenerateSkill {
                design_style: false,
                prompt: "Review reference".into(),
                reference_paths: vec![outside_path.clone()],
                settings: Default::default(),
            },
            AiRequest::CreateScienceSample { directory: outside_path.clone() },
            AiRequest::ExportDocument { path: outside_path.clone(), pdf: false },
            AiRequest::Start(TaskRequest {
                prompt: "Produce".into(),
                settings: Default::default(),
                skill_ids: vec![],
                auto_select_skills: false,
                target: TaskTarget::NewA4 { pages: 1 },
                export_directory: Some(outside_path.clone()),
            }),
        ];
        for request in requests {
            let snapshot = file_access::confine(Some(&roots), || runner.handle(&mut session, request));
            assert!(snapshot.message.contains("outside the"), "{}", snapshot.message);
            assert!(!snapshot.busy);
            assert!(runner.pending.is_none());
            assert!(session.documents().is_empty());
            assert!(!Path::new(&outside_path).exists());
            assert_eq!(snapshot.skills.len(), 9);
        }
    }

    #[test]
    fn launch_retains_roots_when_completion_is_polled_unconfined() {
        let profile = tempfile::tempdir().unwrap();
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = file_access::AutomationRoots::new(Some(allowed.path()), Some(allowed.path())).unwrap().unwrap();
        let mut runner = ProductionRunner::open(profile.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let before = session.doc().unwrap().clone();
        let task = TaskRequest {
            prompt: "test".into(),
            settings: Default::default(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: Some(outside.path().join("output").to_string_lossy().into_owned()),
        };
        // Exercise the launch boundary directly: completion must recheck paths even if
        // an upstream validation is missing or a destination changed while generating.
        let work = worker::Work::Skill {
            design_style: false,
            prompt: String::new(),
            reference_paths: vec![],
            context: None,
            settings: Default::default(),
            catalog: json!({}),
            registry_root: profile.path().join("skills"),
        };
        file_access::confine(Some(&roots), || runner.launch(work, Some(task), Some((before.uid, before.revision)), vec![], Default::default()))
            .unwrap();
        let pending = runner.pending.as_mut().unwrap();
        assert!(std::sync::Arc::ptr_eq(pending.roots.as_ref().unwrap(), &roots));
        pending.worker.cancel.store(true, Ordering::Relaxed);
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.worker = worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) };
        sender
            .send(worker::Message::Finished(worker::Outcome {
                result: Ok(worker::Product::Plan(ProductionPlan {
                    title: "test".into(),
                    commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,100,100]}) }],
                    images: vec![],
                })),
                usage: Default::default(),
            }))
            .unwrap();
        let snapshot = file_access::unconfined(|| runner.handle(&mut session, AiRequest::Poll));
        assert_eq!(snapshot.stage, "failed");
        assert!(snapshot.message.contains("outside the write root"), "{}", snapshot.message);
        assert!(std::sync::Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert_eq!(before.history.undo.len(), session.doc().unwrap().history.undo.len());
        assert!(!outside.path().join("output").exists());
    }

    #[test]
    fn local_ollama_worker_publishes_new_native_document_and_measured_history() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let plan = json!({
            "title":"Local native production regression",
            "commands":[
                {"command":"frame.create","params":{"rect":[50,60,200,180],"content":"unassigned"}},
                {"command":"object.label","params":{"ids":["$0.id"],"label":"integration.editable-frame"}}
            ],
            "images":[]
        });
        let reply = json!({
            "done":true,"message":{"role":"assistant","content":plan.to_string()},
            "prompt_eval_count":321,"eval_count":45,"prompt_eval_cached_count":17
        })
        .to_string();
        let server = std::thread::spawn(move || -> Result<String, String> {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if server_stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                            return Err("Local mock received no request before stopping".into());
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error.to_string()),
                }
            };
            socket.set_nonblocking(false).map_err(|error| error.to_string())?;
            socket.set_read_timeout(Some(Duration::from_secs(3))).map_err(|error| error.to_string())?;
            socket.set_write_timeout(Some(Duration::from_secs(3))).map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                if Instant::now() >= deadline || bytes.len() > 1024 * 1024 {
                    return Err("Local mock request exceeded its time or byte limit".into());
                }
                let count = socket.read(&mut chunk).map_err(|error| error.to_string())?;
                if count == 0 {
                    return Err("Local mock request ended before its body".into());
                }
                bytes.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .ok_or("Local mock request needs Content-Length")?;
                    if body.len() >= length {
                        break;
                    }
                }
            }
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .map_err(|error| error.to_string())?;
            String::from_utf8(bytes).map_err(|error| error.to_string())
        });

        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        session.execute("frame.create", &json!({"rect":[20,20,80,80],"content":"unassigned"})).unwrap();
        let previous = session.doc().unwrap().clone();
        let settings = AiSettings {
            text_model: designcraft_ai_types::ModelSelection {
                provider: ProviderKind::Ollama,
                model: "local-regression".into(),
                endpoint: Some(endpoint),
            },
            budget: BudgetSpec { token_limit: 256_000, max_output_tokens: 512, max_requests: 1, repair_attempts: 0, ..Default::default() },
            pages_per_batch: 0,
            ..Default::default()
        };
        let mut snapshot = runner.handle(
            &mut session,
            AiRequest::Start(TaskRequest {
                prompt: "Create one editable native frame and label on a new A4 page.".into(),
                settings,
                skill_ids: Vec::new(),
                auto_select_skills: false,
                target: TaskTarget::NewA4 { pages: 1 },
                export_directory: None,
            }),
        );
        assert!(snapshot.busy, "{}", snapshot.message);
        let deadline = Instant::now() + Duration::from_secs(30);
        while snapshot.busy && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            snapshot = runner.handle(&mut session, AiRequest::Poll);
        }
        stop.store(true, Ordering::Relaxed);
        let captured = server.join().unwrap();
        assert!(!snapshot.busy, "Background production did not finish before the deadline: {}", snapshot.message);
        assert_eq!(snapshot.stage, "completed", "{}", snapshot.message);
        let captured = captured.unwrap();
        assert!(captured.starts_with("POST /api/chat HTTP/1.1"));
        let (_, body) = captured.split_once("\r\n\r\n").unwrap();
        let submitted: Value = serde_json::from_str(body).unwrap();
        assert_eq!(submitted["model"], "local-regression");
        assert_eq!(submitted["stream"], false);
        assert_eq!(submitted["options"]["num_predict"], 512);
        assert_eq!(submitted["format"]["properties"]["images"]["maxItems"], 0);

        assert_eq!(session.documents().len(), 2);
        assert_eq!(session.active_index(), Some(1));
        let retained = &session.documents()[0];
        assert_eq!(retained.uid, previous.uid);
        assert_eq!(retained.revision, previous.revision);
        assert!(Arc::ptr_eq(&retained.doc, &previous.doc));
        let generated = session.doc().unwrap();
        assert_ne!(generated.uid, previous.uid);
        assert_eq!(generated.doc.page_count(), 1);
        assert!(generated.doc.assets.is_empty());
        assert_eq!(generated.doc.spreads[0].items.len(), 1);
        assert_eq!(generated.doc.spreads[0].items[0].label, "integration.editable-frame");
        assert_eq!(generated.history.undo.len(), 1);
        assert_eq!(generated.history.undo[0].label, "Local native production regression");
        let item = generated.doc.spreads[0].items[0].id.0;
        let initial_x = generated.doc.spreads[0].items[0].bounds().x0;

        let report = snapshot.reports.last().unwrap();
        assert_eq!(report.status, "completed");
        assert_eq!(report.provider, ProviderKind::Ollama);
        assert_eq!(report.model, "local-regression");
        assert_eq!(report.actions, 2);
        assert_eq!(report.usage.requests, 1);
        assert_eq!(report.usage.input_tokens, 321);
        assert_eq!(report.usage.output_tokens, 45);
        assert_eq!(report.usage.cached_input_tokens, 17);
        assert_eq!(report.usage.estimated_tokens, 0);
        assert_eq!(report.usage.estimated_cost_usd, Some(0.0));
        assert!(!report.usage.has_unreported_usage);
        assert!(report.skills.is_empty());
        let trace_path = dir.path().join("runs").join(format!("{}.json", report.id));
        assert!(report.artifacts.contains(&trace_path.to_string_lossy().into_owned()));
        let trace: Value = serde_json::from_slice(&std::fs::read(trace_path).unwrap()).unwrap();
        assert_eq!(trace["taskId"], report.id);
        assert_eq!(trace["base"]["newA4Pages"], 1);
        assert_eq!(trace["plan"], plan);
        let history: Vec<TaskReport> = serde_json::from_slice(&std::fs::read(dir.path().join("history.json")).unwrap()).unwrap();
        assert_eq!(history.last().unwrap().id, report.id);
        assert_eq!(history.last().unwrap().status, "completed");
        assert_eq!(history.last().unwrap().usage.input_tokens, 321);

        // A native command can still edit the generated item. Undoing that edit restores
        // the single production undo step, which then removes both frame and label together.
        session.execute("transform.move", &json!({"ids":[item],"dx":10,"dy":0})).unwrap();
        assert_eq!(session.doc().unwrap().doc.spreads[0].items[0].bounds().x0, initial_x + 10.0);
        session.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().history.undo.len(), 1);
        session.execute("edit.undo", &json!({})).unwrap();
        assert!(session.doc().unwrap().doc.spreads[0].items.is_empty());
        assert!(Arc::ptr_eq(&session.documents()[0].doc, &previous.doc));
    }

    #[test]
    fn builtins_remain_drafts_and_cannot_be_approved_without_sample() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let snapshot = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(snapshot.skills.len(), 9);
        assert!(snapshot.skills.iter().all(|s| !s.approved));
        let skill = &snapshot.skills[0];
        let denied = runner.handle(&mut session, AiRequest::ApproveSkill { id: skill.id.clone(), revision: skill.revision.clone() });
        assert!(denied.message.contains("Run and visually review"));
        assert!(denied.skills.iter().all(|s| !s.approved));
    }

    #[test]
    fn approval_checks_every_template_and_keeps_previous_active_revision_on_failure() {
        fn package(extra: Option<Value>) -> designcraft_ai_skills::PackageFiles {
            let mut templates = vec!["templates/sample.json".to_string()];
            if extra.is_some() {
                templates.push("templates/alternative.json".into());
            }
            let manifest = designcraft_ai_skills::NativeManifest {
                design_profile: None,
                schema_version: 1,
                skill_id: "approval-variants".into(),
                required_commands: vec!["frame.create".into(), "object.label".into()],
                references: Vec::new(),
                templates,
                sample_template: Some("templates/sample.json".into()),
                external_tools: Vec::new(),
                compatibility_notes: "Native shape-only review fixture".into(),
            };
            let sample = json!([
                {"command":"frame.create","params":{"rect":[54,54,140,140],"content":"unassigned"}},
                {"command":"object.label","params":{"ids":["$0.id"],"label":"review.sample"}}
            ]);
            let mut files = designcraft_ai_skills::PackageFiles::from([
                (
                    "SKILL.md".into(),
                    b"---\nname: approval-variants\ndescription: A4 print review and Arabic typography \n---\nKeep native editable sample content.\n"
                        .to_vec(),
                ),
                ("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap()),
                ("templates/sample.json".into(), serde_json::to_vec(&sample).unwrap()),
            ]);
            if let Some(extra) = extra {
                files.insert("templates/alternative.json".into(), serde_json::to_vec(&extra).unwrap());
            }
            files
        }
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let original = session.doc().unwrap().clone();
        let first = runner.registry.import_files(package(None), Provenance::default()).unwrap();
        assert!(runner.choose_skills("A4").is_empty(), "drafts must never auto-select");
        runner.run_sample(&session, &first.skill_id, &first.revision).unwrap();
        runner.dispatch(&mut session, AiRequest::ApproveSkill { id: first.skill_id.clone(), revision: first.revision.clone() }).unwrap();
        assert_eq!(runner.choose_skills("A4"), vec![first.skill_id.clone()]);
        assert_eq!(runner.choose_skills("a4"), vec![first.skill_id.clone()]);
        for extra in [
            json!([{"command":"frame.create","params":{"rect":"malformed rectangle","content":"unassigned"}}]),
            json!([{"command":"object.label","params":{"ids":["$99.id"],"label":"invalid forward reference"}}]),
        ] {
            let draft = runner.registry.import_files(package(Some(extra)), Provenance::default()).unwrap();
            runner.run_sample(&session, &draft.skill_id, &draft.revision).unwrap();
            let error =
                runner.dispatch(&mut session, AiRequest::ApproveSkill { id: draft.skill_id.clone(), revision: draft.revision.clone() }).unwrap_err();
            assert!(error.contains("templates/alternative.json"), "{error}");
            let entry = runner.registry.catalog(&draft.skill_id).remove(0);
            assert_eq!(entry.active_revision.as_deref(), Some(first.revision.as_str()));
            assert_eq!(runner.registry.revision(&draft.skill_id, &draft.revision).unwrap().status, RevisionStatus::Draft);
            assert_eq!(session.documents().len(), 1);
            assert_eq!(session.doc().unwrap().uid, original.uid);
            assert_eq!(session.doc().unwrap().revision, original.revision);
            assert!(std::sync::Arc::ptr_eq(&session.doc().unwrap().doc, &original.doc));
        }
    }
    #[test]
    fn credentials_never_enter_settings_or_task_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        runner.snapshot.settings.text_model =
            designcraft_ai_types::ModelSelection { provider: ProviderKind::OpenAi, model: "test".into(), endpoint: None };
        runner.handle(&mut session, AiRequest::SetApiKey { provider: ProviderKind::OpenAi, key: "test-private-value".into() });
        assert_eq!(runner.keys.len(), 1);
        runner.handle(&mut session, AiRequest::UpdateSettings(AiSettings::default()));
        let saved = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert!(!saved.contains("test-private-value"));
        assert!(!serde_json::to_string(&runner.snapshot).unwrap().contains("test-private-value"));
    }

    #[test]
    fn credential_readiness_is_independent_from_protected_storage_health() {
        let selection = designcraft_ai_types::ModelSelection {
            provider: ProviderKind::OpenAi,
            model: "synthetic-model".into(),
            endpoint: Some("https://credential-review.invalid/v1/".into()),
        };
        let empty = credential_status(selection.clone(), Ok(false), false);
        assert!(!empty.available);
        assert!(!empty.persisted);
        assert!(empty.protected_storage_available);
        let stored = credential_status(selection.clone(), Ok(true), false);
        assert!(stored.available && stored.persisted && stored.protected_storage_available);
        let missing = credential_status(selection.clone(), Err("Protected storage denied".into()), false);
        assert!(!missing.available && !missing.persisted && !missing.protected_storage_available);
        let keys = vec![SessionKey::new(&selection, SecretString::new("synthetic-runtime-credential".into())).unwrap()];
        let in_memory = crate::vault::selected_key(&keys, &selection).is_some();
        let usable = credential_status(selection.clone(), Err("Protected storage denied".into()), in_memory);
        assert!(usable.available);
        assert!(!usable.protected_storage_available);
        assert!(!usable.persisted);
        let other_endpoint =
            designcraft_ai_types::ModelSelection { endpoint: Some("https://other-credential-review.invalid/v1/".into()), ..selection };
        // Custom endpoints inherit neither another endpoint's session key nor
        // canonical-provider environment keys, even when those exist locally.
        let inherited = crate::vault::selected_key(&keys, &other_endpoint).is_some();
        assert!(!inherited);
        let other = credential_status(other_endpoint, Ok(false), inherited);
        assert!(!other.available);
        let serialized = serde_json::to_string(&usable).unwrap();
        assert!(!serialized.contains("synthetic-runtime-credential"));
        assert!(!serialized.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn credential_preparation_allows_authorized_transient_keys_when_storage_is_denied() {
        let canonical = designcraft_ai_types::ModelSelection { provider: ProviderKind::OpenAi, model: "test-model".into(), endpoint: None };
        // Inject already-authorized canonical environment presence; do not
        // mutate process environment or access the user's credential store.
        let prepared = credential_from_store_or_transient(&canonical, || Err("Synthetic storage denied".into()), || true).unwrap();
        assert!(prepared.is_none(), "the worker retains its canonical environment fallback");
        let status = credential_status(canonical.clone(), Err("Synthetic storage denied".into()), true);
        assert!(status.available && !status.protected_storage_available);
        let custom =
            designcraft_ai_types::ModelSelection { endpoint: Some("https://authorized-session-review.invalid/v1/".into()), ..canonical.clone() };
        let session = vec![SessionKey::new(&custom, SecretString::new("synthetic-authorized-session-key".into())).unwrap()];
        assert!(
            credential_from_store_or_transient(
                &custom,
                || Err("Synthetic storage denied".into()),
                || crate::vault::selected_key(&session, &custom).is_some(),
            )
            .unwrap()
            .is_none()
        );
        let other_endpoint =
            designcraft_ai_types::ModelSelection { endpoint: Some("https://unauthorized-session-review.invalid/v1/".into()), ..canonical };
        let denied = credential_from_store_or_transient(
            &other_endpoint,
            || Err("Synthetic storage denied".into()),
            || crate::vault::selected_key(&session, &other_endpoint).is_some(),
        )
        .unwrap_err();
        assert_eq!(denied, "Synthetic storage denied", "custom endpoints must inherit no session or canonical environment key");
    }

    #[test]
    fn credential_preparation_keeps_stored_key_precedence_and_requires_a_real_fallback() {
        let selection = designcraft_ai_types::ModelSelection { provider: ProviderKind::OpenAi, model: "test-model".into(), endpoint: None };
        let stored = credential_from_store_or_transient(
            &selection,
            || Ok(Some(SecretString::new("synthetic-stored-key".into()))),
            || panic!("a successful stored key must take precedence without checking transient credentials"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(stored.expose(), "synthetic-stored-key");
        assert!(
            credential_from_store_or_transient(&selection, || Ok(None), || panic!("a healthy empty store is not a storage error")).unwrap().is_none()
        );
        let missing = credential_from_store_or_transient(&selection, || Err("Synthetic storage denied".into()), || false).unwrap_err();
        assert_eq!(missing, "Synthetic storage denied");
        let invalid = designcraft_ai_types::ModelSelection { endpoint: Some("http://remote-credential-review.invalid/v1/".into()), ..selection };
        assert!(
            credential_from_store_or_transient(
                &invalid,
                || panic!("invalid endpoints must fail before accessing storage"),
                || panic!("invalid endpoints cannot use transient fallback"),
            )
            .is_err()
        );
    }

    #[test]
    fn automatic_skill_selection_uses_the_manual_current_compatibility_predicate() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entry = runner.registry.catalog("school-print-preflight").remove(0);
        let record = entry.revisions.first_mut().unwrap();
        record.status = RevisionStatus::Approved;
        entry.active_revision = Some(record.revision.clone());
        let supported = supported_commands();
        assert_eq!(choose_compatible_skills("A4 طباعة", vec![entry.clone()], &supported), vec![entry.skill_id.clone()]);
        let mut removed_command = entry.clone();
        removed_command.revisions[0].manifest.required_commands.push("removed.engine.command".into());
        assert!(!skill_is_compatible(&removed_command.revisions[0], &supported));
        assert!(choose_compatible_skills("A4 طباعة", vec![removed_command], &supported).is_empty());
        let mut external_tool = entry.clone();
        external_tool.revisions[0].manifest.external_tools.push("unsupported-external-tool".into());
        assert!(!skill_is_compatible(&external_tool.revisions[0], &supported));
        assert!(choose_compatible_skills("A4 طباعة", vec![external_tool], &supported).is_empty());
        let mut disabled = entry.clone();
        disabled.enabled = false;
        assert!(choose_compatible_skills("A4 طباعة", vec![disabled], &supported).is_empty());
        let mut draft = entry;
        draft.revisions[0].status = RevisionStatus::Draft;
        assert!(choose_compatible_skills("A4 طباعة", vec![draft], &supported).is_empty());
    }

    #[test]
    fn skill_relevance_counts_distinct_meaningful_terms_and_normalizes_only_search_text() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entry = runner.registry.catalog("school-print-preflight").remove(0);
        entry.skill_id = "search-fixture".into();
        entry.name = "fixture".into();
        entry.description = "layout picture typography تصميم صفحات and the with".into();
        assert_eq!(skill_match_score("layout picture", &entry), skill_match_score(&"layout picture ".repeat(100), &entry));
        assert_eq!(skill_match_score("and the with", &entry), 0, "Common instruction words are not evidence of relevance");
        assert_eq!(skill_match_score("تَصْمِيم صَفَحَات", &entry), skill_match_score("تصميم صفحات", &entry));
        assert!(skill_match_score("تَصْمِيم صَفَحَات", &entry) > 0);
        entry.description = "تَصْمِيم صَفَحَات".into();
        assert!(skill_match_score("تصميم صفحات", &entry) > 0, "Search normalization must cover catalog text too");
    }

    #[test]
    fn curriculum_studio_routes_to_books_without_claiming_unrelated_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entry = runner.registry.catalog("school-print-preflight").remove(0);
        entry.skill_id = "curriculum-print-design-studio".into();
        entry.name = "Curriculum studio".into();
        entry.description = "Subject progression and page production".into();
        entry.revisions[0].status = RevisionStatus::Approved;
        entry.active_revision = Some(entry.revisions[0].revision.clone());
        for prompt in ["كتيب علوم 48 صفحة للأطفال", "Create a 48 page science curriculum workbook"] {
            assert_eq!(choose_compatible_skills(prompt, vec![entry.clone()], &supported_commands()), vec![entry.skill_id.clone()]);
        }
        assert!(choose_compatible_skills("Create a business invoice", vec![entry], &supported_commands()).is_empty());

        // Explicitly naming a skill_id in the prompt elevates it into the top-6 selection even alongside 8+ high-priority skills.
        let mut catalog_entries =
            runner.registry.catalog("").into_iter().filter(|e| e.revisions[0].manifest.design_profile.is_none()).collect::<Vec<_>>();
        let seed_entry = catalog_entries[0].clone();
        for extra_id in [
            "kg-cognitive-sensory-layout",
            "kg-international-curriculum-architect",
            "kg-oxford-skills-time",
            "kg-oxford-fluency-time",
            "kg-school-print-prepress",
        ] {
            let mut extra = seed_entry.clone();
            extra.skill_id = extra_id.into();
            extra.name = extra_id.into();
            extra.description = "منهج علوم دولي للأطفال للطباعة".into();
            extra.revisions[0].status = RevisionStatus::Approved;
            extra.active_revision = Some(extra.revisions[0].revision.clone());
            catalog_entries.push(extra);
        }
        let mut custom = seed_entry;
        custom.skill_id = "custom-editorial-craft".into();
        custom.name = "Editorial Craft".into();
        custom.description = "Bespoke grid polish".into();
        custom.revisions[0].status = RevisionStatus::Approved;
        custom.active_revision = Some(custom.revisions[0].revision.clone());
        catalog_entries.push(custom);
        let chosen = choose_compatible_skills(
            "صمم منهج علوم دولي للأطفال لمرحلة KG2 باستخدام مهارة custom-editorial-craft للطباعة",
            catalog_entries,
            &supported_commands(),
        );
        assert!(chosen.contains(&"custom-editorial-craft".to_owned()), "explicitly named skill_id must be selected, got: {chosen:?}");
    }

    #[test]
    fn automatic_curriculum_routing_retains_native_arabic_print_and_visual_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entries =
            runner.registry.catalog("").into_iter().filter(|entry| entry.revisions[0].manifest.design_profile.is_none()).collect::<Vec<_>>();
        let seed = entries[0].clone();
        for (id, description) in [
            (
                "kg-cognitive-sensory-layout",
                "Unit opener, vocabulary flashcards and 4-panel story grid. تخطيط صفحات الوحدات والبطاقات المصورة والشبكة القصصية",
            ),
            ("kg-school-print-prepress", "Workbook architecture, word banks and school print preflight. هندسة كراس الأنشطة المدرسي"),
        ] {
            let mut entry = seed.clone();
            entry.skill_id = id.into();
            entry.name = id.into();
            entry.description = description.into();
            entries.push(entry);
        }
        for index in 0..8 {
            let mut distractor = seed.clone();
            distractor.skill_id = format!("distractor-{index}");
            distractor.name = distractor.skill_id.clone();
            distractor.description = "واقعي كامل مستقل توليدي متنوع دقيق واضح جذاب مناسب منظم متكامل أصلي".into();
            entries.push(distractor);
        }
        for entry in &mut entries {
            entry.enabled = true;
            let record = entry.revisions.first_mut().unwrap();
            record.status = RevisionStatus::Approved;
            entry.active_revision = Some(record.revision.clone());
        }
        let brief = "صمم منهج علوم مصور للأطفال بعمر 5-6 سنوات مع صور ورسوم متنوعة للطباعة A4. ";
        let repeated = "واقعي كامل مستقل توليدي متنوع دقيق واضح جذاب مناسب منظم متكامل أصلي ".repeat(60);
        let selected = choose_compatible_skills(&format!("{brief}{repeated}"), entries.clone(), &supported_commands());
        for id in [
            "native-engine-workflow",
            "arabic-english-typography",
            "school-print-preflight",
            "kg-cognitive-sensory-layout",
            "native-design-composition",
            "kg-school-print-prepress",
        ] {
            assert!(selected.iter().any(|selected| selected == id), "Missing {id} in {selected:?}");
        }
        assert_eq!(selected.len(), 6);
        let unrelated = choose_compatible_skills("Create a business invoice", entries, &supported_commands());
        assert!(!unrelated.iter().any(|id| id.starts_with("kg-")), "KG skills require a matching educational brief");
    }

    #[test]
    fn automatic_kg_routing_follows_the_educational_purpose() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entries =
            runner.registry.catalog("").into_iter().filter(|entry| entry.revisions[0].manifest.design_profile.is_none()).collect::<Vec<_>>();
        let seed = entries[0].clone();
        for (id, description) in [
            ("kg-cognitive-sensory-layout", "Unit opener, vocabulary cards and story grids"),
            ("kg-cambridge-thinking-cpa", "Grammar puzzle pieces and sensory mathematics"),
            ("kg-oxford-phonics-handwriting", "Phonics letter maze and handwriting rows"),
            ("kg-oxford-skills-time", "Integrated reading listening speaking writing"),
            ("kg-oxford-fluency-time", "Everyday conversation and hands-on craft project"),
            ("kg-sensory-illustration-vector", "Colour song, sensory art and rainbow illustration"),
            ("kg-school-print-prepress", "Workbook picture crosswords and school print preflight"),
            ("kg-garnet-ict-computing", "Garnet, Oxford and Cambridge international ICT and computing systems lab"),
            (
                "kg-international-curriculum-architect",
                "International curriculum authoring, units, chapters, scope and sequence, and spiral mental chain",
            ),
        ] {
            let mut entry = seed.clone();
            entry.skill_id = id.into();
            entry.name = id.into();
            entry.description = description.into();
            entries.push(entry);
        }
        for entry in &mut entries {
            let record = entry.revisions.first_mut().unwrap();
            record.status = RevisionStatus::Approved;
            entry.active_revision = Some(record.revision.clone());
        }
        for (prompt, expected) in [
            ("منهج رياضيات للأطفال لتعليم العد ومقارنة الكميات والتفكير المحسوس", "kg-cambridge-thinking-cpa"),
            ("اريد منهج دولي لمادةmath", "kg-cambridge-thinking-cpa"),
            ("منهج للأطفال عن الحروف والأصوات وتدريب خط اليد", "kg-oxford-phonics-handwriting"),
            ("منهج أطفال لدمج القراءة والاستماع والتحدث والكتابة", "kg-oxford-skills-time"),
            ("منهج للأطفال بمشروع يدوي ومحادثة يومية", "kg-oxford-fluency-time"),
            ("منهج أطفال للألوان والفن والرسم وقوس قزح", "kg-sensory-illustration-vector"),
            ("كراس أنشطة للأطفال مع شبكة كلمات متقاطعة وتجهيز المطبعة", "kg-school-print-prepress"),
            ("صمم منهج علوم مصور للأطفال مع بطاقات وقصة مصورة", "kg-cognitive-sensory-layout"),
            ("إنشاء منهج دولي متكامل في مادة الحاسوب بأسلوب أكسفورد وكامبريدج وكارنيت", "kg-garnet-ict-computing"),
            ("خوارزمية ومهارة كتابة المنهج وإنشائه وتسلسله ووحداته وفصوله والربط السلسلي الذهني", "kg-international-curriculum-architect"),
        ] {
            let selected = choose_compatible_skills(prompt, entries.clone(), &supported_commands());
            assert_eq!(selected.iter().find(|id| id.starts_with("kg-")).map(String::as_str), Some(expected), "{prompt}: {selected:?}");
        }
        let math_selected = choose_compatible_skills("اريد منهج دولي لمادةmath", entries, &supported_commands());
        assert!(math_selected.iter().any(|id| id == "kg-international-curriculum-architect"), "{math_selected:?}");
    }

    #[test]
    fn automatic_arabic_book_brief_selects_kg_layout_and_editorial_variety() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut entries =
            runner.registry.catalog("").into_iter().filter(|entry| entry.revisions[0].manifest.design_profile.is_none()).collect::<Vec<_>>();
        let seed = entries[0].clone();
        for (id, description) in [
            ("kg-cognitive-sensory-layout", "Unit opener, vocabulary cards and story grids"),
            ("native-editorial-variety", "Choose different editable page compositions"),
        ] {
            let mut entry = seed.clone();
            entry.skill_id = id.into();
            entry.name = id.into();
            entry.description = description.into();
            entries.push(entry);
        }
        for entry in &mut entries {
            let record = entry.revisions.first_mut().unwrap();
            record.status = RevisionStatus::Approved;
            entry.active_revision = Some(record.revision.clone());
        }
        let selected = choose_compatible_skills("كتيب علوم متنوع للأطفال48صفحةA4", entries.clone(), &supported_commands());
        for expected in ["kg-cognitive-sensory-layout", "native-editorial-variety", "native-design-composition"] {
            assert!(selected.iter().any(|id| id == expected), "Missing {expected}: {selected:?}");
        }
        let unrelated = choose_compatible_skills("Create a business invoice", entries, &supported_commands());
        assert!(!unrelated.iter().any(|id| id == "native-editorial-variety" || id.starts_with("kg-")));
    }

    #[test]
    fn runtime_selects_every_supported_text_reference_including_third_and_fourth_files() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(dir.path()).unwrap();
        let mut record = runner.registry.catalog("").remove(0).revisions.remove(0);
        record.manifest.references =
            ["references/a.md", "references/b.txt", "references/c.json", "references/d.csv", "references/source.pdf", "references/icon.png"]
                .into_iter()
                .map(str::to_owned)
                .collect();
        assert_eq!(selected_skill_references(&record), vec!["references/a.md", "references/b.txt", "references/c.json", "references/d.csv"]);
    }

    #[test]
    fn forwarded_session_key_cannot_follow_caller_endpoint_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let selected = designcraft_ai_types::ModelSelection { provider: ProviderKind::OpenAi, model: "test".into(), endpoint: None };
        runner.snapshot.settings.text_model = selected.clone();
        runner.handle(&mut session, AiRequest::SetApiKey { provider: ProviderKind::OpenAi, key: "synthetic-private-key".into() });
        let keys = runner.worker_keys();
        assert_eq!(crate::vault::selected_key(&keys, &selected).unwrap().expose(), "synthetic-private-key");
        let attacker = designcraft_ai_types::ModelSelection { endpoint: Some("https://attacker.example/v1/".into()), ..selected };
        assert!(crate::vault::selected_key(&keys, &attacker).is_none());
    }
    #[test]
    #[cfg(windows)]
    fn builtin_samples_really_execute_render_and_can_be_reviewed() {
        designcraft_fonts::FontDb::global().load_system_fonts();
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let session = Session::new();
        let cards = runner.snapshot.skills.clone();
        for skill in cards {
            runner.run_sample(&session, &skill.id, &skill.revision).unwrap_or_else(|e| panic!("{}: {e}", skill.id));
            let review = runner.snapshot.review.as_ref().unwrap();
            assert!(review.sample_valid);
            assert!(std::fs::metadata(review.sample_path.as_ref().unwrap()).unwrap().len() > 100);
        }
        assert!(session.documents().is_empty(), "review samples must not alter open documents");
    }
    #[test]
    fn running_usage_and_full_diagnostics_are_visible_before_worker_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
            id: "live-progress".into(),
            started: Instant::now(),
            expected: None,
            expected_prefs: None,
            task: None,
            skills: vec![],
            settings: Default::default(),
        });
        runner.snapshot.busy = true;
        sender
            .send(worker::Message::Usage(UsageSummary {
                requests: 2,
                input_tokens: 123,
                output_tokens: 45,
                estimated_cost_usd: Some(0.01),
                ..Default::default()
            }))
            .unwrap();
        sender.send(worker::Message::Diagnostic(json!({"kind":"printRequirement","message":"full detailed cause\npossible remedy"}))).unwrap();
        for attempt in 1..=3 {
            sender
                .send(worker::Message::Diagnostic(json!({"kind":"nativePlanningValidation","attempt":attempt,"message":"bounded evidence"})))
                .unwrap();
        }
        let snapshot = runner.handle(&mut session, AiRequest::Poll);
        assert!(snapshot.busy);
        assert_eq!(snapshot.current_usage.requests, 2);
        assert_eq!(snapshot.current_usage.input_tokens, 123);
        assert_eq!(snapshot.current_issues[0]["message"], "full detailed cause\npossible remedy");
        assert_eq!(snapshot.current_issues.len(), 2, "Repeated planning attempts must not multiply large history/poll payloads");
        assert_eq!(snapshot.current_issues[1]["attempt"], 3);
        assert!(snapshot.reports.is_empty());
        assert!(session.documents().is_empty());
        assert!(runner.pending.is_some());
    }

    #[test]
    fn facing_nonbatch_mapping_failure_restores_original_active_document() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        session.execute("frame.create", &json!({"rect":[40,40,100,100]})).unwrap();
        let original = session.doc().unwrap().clone();
        let task = TaskRequest {
            prompt: "Create a facing-page booklet".into(),
            settings: AiSettings { facing_pages: true, pages_per_batch: 0, ..Default::default() },
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 2 },
            export_directory: None,
        };
        // A missing absolute spread is valid in ordinary nonbatch engine actions,
        // but cannot be mapped safely into a facing-page book.
        let plan = ProductionPlan {
            title: "Invalid mapping".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,100,100]}) }],
            images: vec![],
        };
        let mut report = TaskReport {
            id: "facing-rollback".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: String::new(),
            settings: None,
            skills: vec![],
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
            design_style: None,
        };
        let error = runner.publish(&mut session, &Some(task.clone()), None, worker::Product::Plan(plan), &mut report).unwrap_err();
        assert!(error.contains("spread"), "{error}");
        assert_eq!(session.documents().len(), 1);
        assert_eq!(session.active_index(), Some(0));
        assert_eq!(session.doc().unwrap().uid, original.uid);
        assert!(std::sync::Arc::ptr_eq(&original.doc, &session.doc().unwrap().doc));
        assert_eq!(session.doc().unwrap().history.undo.len(), original.history.undo.len());
        let valid = ProductionPlan {
            title: "Valid facing publication".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[50,50,100,100]}) }],
            images: vec![],
        };
        runner.publish(&mut session, &Some(task), None, worker::Product::Plan(valid), &mut report).unwrap();
        assert_eq!(session.documents().len(), 2);
        assert!(session.doc().unwrap().doc.settings.facing_pages);
        assert_eq!(session.doc().unwrap().history.undo.len(), 1);
        session.execute("edit.undo", &json!({})).unwrap();
        assert!(session.doc().unwrap().doc.spreads.iter().all(|spread| spread.items.is_empty()));
        assert!(session.doc().unwrap().doc.settings.facing_pages);
        assert!(std::sync::Arc::ptr_eq(&original.doc, &session.documents()[0].doc));
    }

    #[test]
    fn stale_result_cannot_touch_another_document() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let uid = session.doc().unwrap().uid;
        let revision = session.doc().unwrap().revision;
        session.execute("frame.create", &json!({"rect":[40,40,100,100]})).unwrap();
        let task = TaskRequest {
            prompt: "test".into(),
            settings: AiSettings::default(),
            skill_ids: Vec::new(),
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: None,
        };
        let plan = ProductionPlan {
            title: "test".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[110,110,200,200]}) }],
            images: Vec::new(),
        };
        let mut report = TaskReport {
            design_style: None,
            id: "test".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: String::new(),
            settings: None,
            skills: Vec::new(),
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: Vec::new(),
            artifacts: Vec::new(),
            message: String::new(),
        };
        assert!(runner.publish(&mut session, &Some(task), Some((uid, revision)), worker::Product::Plan(plan), &mut report).is_err());
        assert_eq!(session.doc().unwrap().doc.spreads[0].items.len(), 1);
    }
    #[test]
    fn nonbatch_production_rejects_changed_preferences_before_any_publication() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        session.prefs.typographers_quotes = false;
        session.execute("file.new", &new_a4(1)).unwrap();
        let before = session.doc().unwrap().clone();
        let original_prefs = session.prefs.clone();
        let supplied = "\"Preserve this exact copy\"";
        let plan = ProductionPlan {
            title: "Pinned nonbatch preferences".into(),
            commands: vec![
                EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,300,180],"content":"text"}) },
                EngineAction { command: "text.insert".into(), params: json!({"text":supplied}) },
            ],
            images: vec![],
        };
        let mut private = Session::new();
        private.prefs = original_prefs.clone();
        private.add_document(before.clone());
        private.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
        assert!(private.doc().unwrap().doc.stories.values().any(|story| story.text == supplied));
        let (sender, receiver) = std::sync::mpsc::channel();
        let settings = AiSettings { pages_per_batch: 0, ..Default::default() };
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
            id: "nonbatch-prefs".into(),
            started: Instant::now(),
            task: Some(TaskRequest {
                prompt: supplied.into(),
                settings: settings.clone(),
                skill_ids: vec![],
                auto_select_skills: false,
                target: TaskTarget::CurrentDocument,
                export_directory: None,
            }),
            expected: Some((before.uid, before.revision)),
            expected_prefs: Some(original_prefs),
            skills: vec![],
            settings,
        });
        runner.snapshot.busy = true;
        session.prefs.typographers_quotes = true;
        assert_eq!(session.doc().unwrap().revision, before.revision, "Preference changes do not invalidate the document revision pin");
        sender.send(worker::Message::Finished(worker::Outcome { result: Ok(worker::Product::Plan(plan)), usage: Default::default() })).unwrap();
        let rejected = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(rejected.stage, "failed");
        assert!(rejected.message.contains("preferences changed"), "{}", rejected.message);
        assert!(runner.pending.is_none());
        assert!(std::sync::Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert_eq!(before.history.undo.len(), session.doc().unwrap().history.undo.len());
        assert_eq!(before.is_dirty(), session.doc().unwrap().is_dirty());
        assert_eq!(before.revision, session.doc().unwrap().revision);
        assert!(rejected.reports.last().unwrap().artifacts.is_empty());
        assert!(!runner.root.join("runs").exists(), "Preference drift must be refused before any trace or live publication");
    }

    #[test]
    fn cancel_wins_over_already_queued_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = TaskRequest {
            prompt: "test".into(),
            settings: AiSettings::default(),
            skill_ids: Vec::new(),
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: None,
        };
        let plan = ProductionPlan {
            title: "test".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[50,50,100,100]}) }],
            images: Vec::new(),
        };
        sender.send(worker::Message::Finished(worker::Outcome { result: Ok(worker::Product::Plan(plan)), usage: Default::default() })).unwrap();
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
            id: "cancel-test".into(),
            started: Instant::now(),
            expected: Some((session.doc().unwrap().uid, session.doc().unwrap().revision)),
            expected_prefs: Some(session.prefs.clone()),
            task: Some(task),
            skills: Vec::new(),
            settings: Default::default(),
        });
        runner.snapshot.busy = true;
        let snapshot = runner.handle(&mut session, AiRequest::Cancel);
        assert_eq!(snapshot.reports.last().unwrap().status, "cancelled");
        assert!(session.doc().unwrap().doc.spreads[0].items.is_empty());
        assert!(session.doc().unwrap().history.undo.is_empty());
    }
    #[test]
    fn default_printer_is_an_omitted_parameter_and_print_ids_are_durable() {
        let params = print_params(None, true, "off".into());
        assert!(params.get("printer").is_none());
        assert_eq!(params["scaling"], "exact");
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        runner.print_attempts.insert("already-sent".into());
        write_json(&runner.profile_access, &dir.path().join("print-attempts.json"), &runner.print_attempts).unwrap();
        drop(runner);
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        assert!(runner.print_attempts.contains("already-sent"));
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let snapshot = runner.handle(
            &mut session,
            AiRequest::PrintDocument { printer: None, grayscale: true, duplex: "off".into(), request_id: "already-sent".into() },
        );
        assert!(snapshot.message.contains("already attempted"), "{}", snapshot.message);
        assert!(session.doc().unwrap().history.undo.is_empty());
    }

    #[test]
    fn runtime_rejects_media_models_in_text_slot_and_allows_component_safe_text_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();

        for bad_model in [
            "gemini-3.1-flash-lite-image",
            "models/imagen-3.0-generate-002",
            "gemini-3.8-flash-tts",
            "gemini-embedding-001",
            "dall-e-3",
            "gpt-image-1",
            "veo-2.0-generate-001",
        ] {
            let settings = AiSettings {
                text_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::Gemini, model: bad_model.into(), endpoint: None },
                ..Default::default()
            };
            let update = runner.handle(&mut session, AiRequest::UpdateSettings(settings.clone()));
            assert_eq!(update.stage, "failed", "{bad_model}");
            assert!(update.message.contains("image/media model"), "{bad_model}: {}", update.message);

            let start = runner.handle(
                &mut session,
                AiRequest::Start(TaskRequest {
                    prompt: "Page 1 test".into(),
                    settings,
                    skill_ids: Vec::new(),
                    auto_select_skills: false,
                    target: TaskTarget::CurrentDocument,
                    export_directory: None,
                }),
            );
            assert_eq!(start.stage, "failed", "{bad_model}");
            assert!(start.message.contains("image/media model"), "{bad_model}: {}", start.message);
            assert!(!dir.path().join("production-draft.json").exists(), "rejected media model must not persist a production draft");
        }

        let alias_settings = AiSettings {
            text_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::Ollama, model: "silver-text".into(), endpoint: None },
            ..Default::default()
        };
        let saved = runner.handle(&mut session, AiRequest::UpdateSettings(alias_settings));
        assert_eq!(saved.message, "AI settings saved; API keys remain outside settings and reports");
        assert_eq!(saved.settings.text_model.model, "silver-text");
    }

    #[test]
    fn kg2_islamic_curriculum_combines_seven_skills_and_pinned_skills_without_context_overflow() {
        let prompt = "ابي انتج منهج دولي لماده الاسلاميه لمرحله kg2";
        let supported = supported_commands();

        let live_skills_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join("outputs/ai-preview-profile/AI/skills"));
        if let Some(skills_dir) = live_skills_dir.filter(|path| path.exists()) {
            let live_registry = SkillRegistry::open(&skills_dir).unwrap();
            let catalog = live_registry.catalog("");
            let auto_ids = choose_compatible_skills(prompt, catalog.clone(), &supported);
            assert!(auto_ids.contains(&"kg-international-curriculum-architect".to_string()), "auto_ids={auto_ids:?}");
            assert!(auto_ids.contains(&"kg-cognitive-sensory-layout".to_string()), "auto_ids={auto_ids:?}");
            assert!(
                !auto_ids.contains(&"kg-cambridge-thinking-cpa".to_string()),
                "math CPA skill should not displace Islamic/KG curriculum skills: {auto_ids:?}"
            );
            let mut full_ids = auto_ids.clone();
            full_ids.push("kg-oxford-cambridge-style".into());
            let merged = merge_pinned_and_automatic_skills(&["arabic-editorial-book-design".into()], full_ids, Some("kg-oxford-cambridge-style"));
            assert!(merged.contains(&"arabic-editorial-book-design".to_string()));
            assert!(merged.contains(&"kg-oxford-cambridge-style".to_string()));
            assert!(merged.len() >= 7 && merged.len() <= 8, "merged={merged:?}");

            let mut snapshots = Vec::new();
            for id in &merged {
                if let Some(card) = catalog.iter().find(|e| &e.skill_id == id)
                    && let Some(rev) = &card.active_revision
                    && let Ok(rec) = live_registry.revision(id, rev)
                {
                    let refs = selected_skill_references(rec);
                    if let Ok(snap) = live_registry.snapshot(id, &refs) {
                        snapshots.push(snap);
                    }
                }
            }
            assert!(snapshots.len() >= 7);
            let bounded = bounded_task_skills(snapshots, &TaskTarget::NewA4 { pages: 8 }).unwrap();
            let compacted = worker::compact_skill_guidance(&bounded).unwrap();
            assert!(!compacted.is_empty());
        }

        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();

        let preview = runner.handle(
            &mut session,
            AiRequest::PreviewProduction(TaskRequest {
                prompt: prompt.into(),
                target: TaskTarget::NewA4 { pages: 8 },
                settings: AiSettings::default(),
                skill_ids: vec!["early-years-booklet-planning".into()],
                auto_select_skills: true,
                export_directory: None,
            }),
        );
        assert_ne!(preview.stage, "failed", "preview failed: {}", preview.message);
        let blueprint = preview.production_preview.as_ref().and_then(|value| value.get("blueprint")).expect("curriculum preview blueprint");
        assert!(blueprint.get("subject_domain").and_then(Value::as_str).is_some_and(|s| s.contains("Islamic Studies")), "{blueprint}");
        assert!(
            blueprint
                .get("selected_profile_authority")
                .and_then(|v| v.get("inferred_learner_stage"))
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("KG2")),
            "{blueprint}"
        );
        assert_eq!(preview.production_preview.as_ref().and_then(|v| v.get("binding")).and_then(Value::as_str), Some("RTL"));
    }

    #[test]
    fn compact_context_exposes_existing_document_stories_tables_and_group_children() {
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        let text_frame =
            session.execute("frame.create", &json!({"spread":0,"rect":[36.0,36.0,280.0,180.0],"content":"text","text":"Existing Heading"})).unwrap();
        let text_id = text_frame["id"].as_u64().unwrap();
        let story_id = text_frame["story"].as_u64().unwrap();
        let table = session.execute("table.insert", &json!({"story":story_id,"rows":2,"cols":2})).unwrap()["table"].as_u64().unwrap();
        session.execute("table.setCell", &json!({"table":table,"row":0,"col":0,"text":"Cell A1"})).unwrap();
        let g1 = session.execute("frame.create", &json!({"spread":0,"rect":[300.0,36.0,420.0,140.0],"content":"graphic"})).unwrap()["id"]
            .as_u64()
            .unwrap();
        let g2 = session.execute("frame.create", &json!({"spread":0,"rect":[430.0,36.0,520.0,140.0],"content":"unassigned"})).unwrap()["id"]
            .as_u64()
            .unwrap();
        let group_id = session.execute("object.group", &json!({"ids":[g1,g2]})).unwrap()["id"].as_u64().unwrap();

        let ctx = compact_context(&session).unwrap();
        let objects = ctx["objects"].as_array().expect("objects array");
        let text_entry = objects.iter().find(|entry| entry["id"].as_u64() == Some(text_id)).expect("text frame entry");
        assert_eq!(text_entry["content"].as_str(), Some("text"));
        assert_eq!(text_entry["shape"].as_str(), Some("rectangle"));
        assert_eq!(text_entry["story"].as_u64(), Some(story_id));
        assert!(text_entry["typography"]["size"].as_f64().is_some_and(|s| s > 0.0));
        let tables = text_entry["tables"].as_array().expect("tables array");
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0]["table"].as_u64(), Some(table));
        assert_eq!(tables[0]["cells"][0]["text"].as_str(), Some("Cell A1"));

        let group_entry = objects.iter().find(|entry| entry["id"].as_u64() == Some(group_id)).expect("group entry");
        assert_eq!(group_entry["content"].as_str(), Some("group"));
        let child_entry = objects.iter().find(|entry| entry["id"].as_u64() == Some(g1)).expect("grouped child entry");
        assert_eq!(child_entry["parentGroup"].as_u64(), Some(group_id));
        assert_eq!(child_entry["content"].as_str(), Some("graphic"));
    }

    #[test]
    fn portable_catalog_verifies_sha256_digests_and_enables_manual_and_default_previews() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let imported = runner.install_default_portable_catalog().unwrap();
        assert!(imported >= 20, "expected workspace portable skills catalog to import, got {imported}");
        assert!(runner.snapshot.skill_warnings.is_empty(), "warnings={:?}", runner.snapshot.skill_warnings);
        assert!(runner.snapshot.skills.iter().any(|card| card.id == "kg-oxford-cambridge-style"));

        // Repeat installation must be completely idempotent.
        let count_before = runner.snapshot.skills.len();
        let imported_again = runner.install_default_portable_catalog().unwrap();
        assert_eq!(imported_again, imported);
        assert_eq!(runner.snapshot.skills.len(), count_before);

        // Tampered portable catalog file must be rejected by SHA-256 verification without crashing.
        let tampered_dir = tempfile::tempdir().unwrap();
        let pkg_dir = tampered_dir.path().join("tampered-skill");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("SKILL.md"), b"---\nname: tampered-skill\ndescription: tampered\n---\ntampered\n").unwrap();
        std::fs::write(
            tampered_dir.path().join("catalog.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "packages": [{
                    "id": "tampered-skill",
                    "files": {
                        "SKILL.md": "0000000000000000000000000000000000000000000000000000000000000000"
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let tampered_imported = runner.install_portable_catalog(tampered_dir.path()).unwrap();
        assert_eq!(tampered_imported, 0);
        assert!(runner.snapshot.skill_warnings.iter().any(|w| w.contains("tampered-skill") && w.contains("SHA-256")));
        assert!(!runner.snapshot.skills.iter().any(|card| card.id == "tampered-skill"));

        // Automatic preview without style keywords falls back to a compatible design style cleanly.
        let session = Session::new();
        runner
            .preview_production(
                &session,
                TaskRequest {
                    prompt: "Create a 12-page classroom workbook".into(),
                    settings: AiSettings::default(),
                    skill_ids: Vec::new(),
                    auto_select_skills: true,
                    target: TaskTarget::NewA4 { pages: 12 },
                    export_directory: None,
                },
            )
            .unwrap();
        assert!(runner.snapshot.production_preview.is_some());

        // Manual skill selection on a fresh profile prepares trusted builtin/portable skills automatically,
        // while still requiring explicit human approval for untrusted third-party drafts.
        let fresh_dir = tempfile::tempdir().unwrap();
        let mut fresh_runner = ProductionRunner::open(fresh_dir.path()).unwrap();
        fresh_runner.install_default_portable_catalog().unwrap();
        fresh_runner
            .preview_production(
                &session,
                TaskRequest {
                    prompt: "Create an 8-page workbook".into(),
                    settings: AiSettings::default(),
                    skill_ids: vec!["early-years-clear-style".into(), "early-years-booklet-planning".into()],
                    auto_select_skills: false,
                    target: TaskTarget::NewA4 { pages: 8 },
                    export_directory: None,
                },
            )
            .unwrap();
        assert!(fresh_runner.snapshot.production_preview.is_some());

        let untrusted_manifest = designcraft_ai_skills::NativeManifest {
            design_profile: None,
            schema_version: 1,
            skill_id: "untrusted-third-party".into(),
            required_commands: vec!["frame.create".into()],
            references: Vec::new(),
            templates: vec!["templates/sample.json".into()],
            sample_template: Some("templates/sample.json".into()),
            external_tools: Vec::new(),
            compatibility_notes: "Untrusted fixture".into(),
        };
        let skill_md = b"---\nname: untrusted-third-party\ndescription: third party\n---\nInstructions.\n".to_vec();
        let manifest_bytes = serde_json::to_vec(&untrusted_manifest).unwrap();
        let sample_bytes =
            serde_json::to_vec(&json!([{"command": "frame.create", "params": {"rect": [54, 54, 140, 140], "content": "unassigned"}}])).unwrap();

        // A self-consistent external portable catalog directory (whose catalog.json SHA-256 differs from
        // the shipped Smart Design catalog) imports cleanly as a draft, but must NOT gain trusted auto-approval!
        let external_catalog_dir = tempfile::tempdir().unwrap();
        let external_pkg_dir = external_catalog_dir.path().join("untrusted-third-party");
        std::fs::create_dir_all(external_pkg_dir.join("templates")).unwrap();
        std::fs::write(external_pkg_dir.join("SKILL.md"), &skill_md).unwrap();
        std::fs::write(external_pkg_dir.join("designcraft.json"), &manifest_bytes).unwrap();
        std::fs::write(external_pkg_dir.join("templates/sample.json"), &sample_bytes).unwrap();
        std::fs::write(
            external_catalog_dir.path().join("catalog.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "packages": [{
                    "id": "untrusted-third-party",
                    "files": {
                        "SKILL.md": hex_sha256(&skill_md),
                        "designcraft.json": hex_sha256(&manifest_bytes),
                        "templates/sample.json": hex_sha256(&sample_bytes),
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(fresh_runner.install_portable_catalog(external_catalog_dir.path()).unwrap(), 1);
        assert!(fresh_runner.snapshot.skills.iter().any(|c| c.id == "early-years-clear-style" && c.trusted_source));
        assert!(fresh_runner.snapshot.skills.iter().any(|c| c.id == "untrusted-third-party" && !c.trusted_source));
        let untrusted_err = fresh_runner
            .preview_production(
                &session,
                TaskRequest {
                    prompt: "Create an 8-page workbook".into(),
                    settings: AiSettings::default(),
                    skill_ids: vec!["early-years-clear-style".into(), "untrusted-third-party".into()],
                    auto_select_skills: false,
                    target: TaskTarget::NewA4 { pages: 8 },
                    export_directory: None,
                },
            )
            .unwrap_err();
        assert!(untrusted_err.contains("active skill revision") || untrusted_err.contains("approval"), "err={untrusted_err}");

        // An untrusted external design-style draft selected via settings.design_style in manual mode
        // must also require explicit human approval rather than auto-activating via prepare_style.
        let untrusted_style_manifest = designcraft_ai_skills::NativeManifest {
            design_profile: Some(designcraft_ai_types::DesignProfile {
                name: "Untrusted External Style".into(),
                keywords: vec!["workbook".into()],
                age_range: [5, 7],
                page_mm: [210.0, 297.0],
                margin_mm: 15.0,
                palette: vec!["[Black]".into()],
                typography: vec!["Readable native type".into()],
                layout: vec!["Aligned tasks".into()],
                pedagogy: vec!["Guided practice".into()],
                illustration: vec!["Clean vector shapes".into()],
                page_families: vec![designcraft_ai_types::PageFamily {
                    id: "activity".into(),
                    purpose: "Independent practice".into(),
                    template: "templates/sample.json".into(),
                    text_slots: Vec::new(),
                    image_slots: Vec::new(),
                }],
                corrections: Vec::new(),
            }),
            schema_version: 1,
            skill_id: "untrusted-external-style".into(),
            required_commands: vec!["frame.create".into()],
            references: Vec::new(),
            templates: vec!["templates/sample.json".into()],
            sample_template: Some("templates/sample.json".into()),
            external_tools: Vec::new(),
            compatibility_notes: "Untrusted external design style".into(),
        };
        let style_md = b"---\nname: untrusted-external-style\ndescription: third party style\n---\nInstructions.\n".to_vec();
        let style_manifest_bytes = serde_json::to_vec(&untrusted_style_manifest).unwrap();
        let style_pkg_dir = external_catalog_dir.path().join("untrusted-external-style");
        std::fs::create_dir_all(style_pkg_dir.join("templates")).unwrap();
        std::fs::write(style_pkg_dir.join("SKILL.md"), &style_md).unwrap();
        std::fs::write(style_pkg_dir.join("designcraft.json"), &style_manifest_bytes).unwrap();
        std::fs::write(style_pkg_dir.join("templates/sample.json"), &sample_bytes).unwrap();
        std::fs::write(
            external_catalog_dir.path().join("catalog.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "packages": [{
                    "id": "untrusted-external-style",
                    "files": {
                        "SKILL.md": hex_sha256(&style_md),
                        "designcraft.json": hex_sha256(&style_manifest_bytes),
                        "templates/sample.json": hex_sha256(&sample_bytes),
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(fresh_runner.install_portable_catalog(external_catalog_dir.path()).unwrap(), 1);
        assert!(fresh_runner.snapshot.skills.iter().any(|c| c.id == "untrusted-external-style" && !c.trusted_source && !c.active));
        let manual_style_settings = AiSettings { design_style: Some("untrusted-external-style".into()), ..Default::default() };
        let style_err = fresh_runner
            .preview_production(
                &session,
                TaskRequest {
                    prompt: "Create an 8-page workbook".into(),
                    settings: manual_style_settings,
                    skill_ids: Vec::new(),
                    auto_select_skills: false,
                    target: TaskTarget::NewA4 { pages: 8 },
                    export_directory: None,
                },
            )
            .unwrap_err();
        assert!(style_err.contains("requires approval"), "style_err={style_err}");
        assert!(fresh_runner.snapshot.skills.iter().any(|c| c.id == "untrusted-external-style" && !c.active));
    }

    #[test]
    fn start_rejects_mismatched_image_and_inspection_models_before_spending_requests_and_syncs_draft_and_rtl_binding() {
        use designcraft_ai_types::{ModelSelection, OutputMode};

        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 4, "facingPages": true})).unwrap();
        session.execute("layout.documentSetup", &json!({"binding": "rightToLeft"})).unwrap();

        // 1. Text-only model in image_model slot with NativeWithImages must fail before spawning worker.
        let bad_image_settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
            ..Default::default()
        };
        let snap1 = runner.handle(
            &mut session,
            AiRequest::Start(TaskRequest {
                prompt: "Create an illustrated science page".into(),
                settings: bad_image_settings,
                skill_ids: Vec::new(),
                auto_select_skills: false,
                target: TaskTarget::CurrentDocument,
                export_directory: None,
            }),
        );
        assert_eq!(snap1.stage, "failed");
        assert!(!snap1.busy && snap1.reports.is_empty());
        assert!(snap1.message.contains("text/non-image model"), "msg={}", snap1.message);

        // 2. Ollama in image_model slot with NativeWithImages must fail before spawning worker.
        let ollama_image_settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::Ollama, model: "llava:latest".into(), endpoint: None },
            ..Default::default()
        };
        let snap2 = runner.handle(
            &mut session,
            AiRequest::Start(TaskRequest {
                prompt: "Create an illustrated science page".into(),
                settings: ollama_image_settings,
                skill_ids: Vec::new(),
                auto_select_skills: false,
                target: TaskTarget::CurrentDocument,
                export_directory: None,
            }),
        );
        assert_eq!(snap2.stage, "failed");
        assert!(!snap2.busy && snap2.reports.is_empty());
        assert!(snap2.message.contains("Ollama"), "msg={}", snap2.message);

        // 3. verify_generated_images with non-Gemini (OpenAI) text_model must fail before spawning worker.
        let bad_verify_settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            verify_generated_images: true,
            text_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image-preview".into(), endpoint: None },
            ..Default::default()
        };
        let snap3 = runner.handle(
            &mut session,
            AiRequest::Start(TaskRequest {
                prompt: "Create an illustrated science page".into(),
                settings: bad_verify_settings,
                skill_ids: Vec::new(),
                auto_select_skills: false,
                target: TaskTarget::CurrentDocument,
                export_directory: None,
            }),
        );
        assert_eq!(snap3.stage, "failed");
        assert!(!snap3.busy && snap3.reports.is_empty());
        assert!(snap3.message.contains("Gemini text model"), "msg={}", snap3.message);

        // 4. bind_production_draft against an RTL facing-pages book must sync facing_pages & right_to_left_binding,
        // and UpdateSettings must propagate updated settings into production-draft.json!
        let stale_ltr = AiSettings {
            facing_pages: false,
            right_to_left_binding: Some(false),
            design_style: Some("kg-oxford-cambridge-style".into()),
            pages_per_batch: 4,
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None },
            ..Default::default()
        };
        runner
            .save_production_draft(
                &session,
                &TaskRequest {
                    prompt: "Continue Arabic workbook".into(),
                    settings: stale_ltr,
                    skill_ids: Vec::new(),
                    auto_select_skills: false,
                    target: TaskTarget::ContinueBook,
                    export_directory: None,
                },
            )
            .unwrap();
        runner.bind_production_draft(&session, "draft-rtl-bound").unwrap();
        let draft_settings = runner.snapshot.production_draft.as_ref().unwrap().request.settings.clone();
        assert!(draft_settings.facing_pages);
        assert_eq!(draft_settings.right_to_left_binding, Some(true));
        assert_eq!(draft_settings.design_style.as_deref(), Some("kg-oxford-cambridge-style"));
        assert_eq!(draft_settings.pages_per_batch, 4);

        let mut healed_settings = draft_settings;
        healed_settings.text_model.model = "gemini-3.8-flash".into();
        healed_settings.budget.token_limit = 512_000;
        // Attempt to change bound-book structural fields in global settings: global settings change,
        // while the bound book's `production_draft` preserves its pinned design_style, binding, and pagination!
        healed_settings.design_style = Some("other-style".into());
        healed_settings.facing_pages = false;
        healed_settings.right_to_left_binding = Some(false);
        healed_settings.pages_per_batch = 8;
        let after_update = runner.handle(&mut session, AiRequest::UpdateSettings(healed_settings.clone()));
        let bound_draft_settings = &after_update.production_draft.as_ref().unwrap().request.settings;
        assert_eq!(bound_draft_settings.text_model.model, "gemini-3.8-flash");
        assert_eq!(bound_draft_settings.budget.token_limit, 512_000);
        assert_eq!(bound_draft_settings.design_style.as_deref(), Some("kg-oxford-cambridge-style"));
        assert!(bound_draft_settings.facing_pages);
        assert_eq!(bound_draft_settings.right_to_left_binding, Some(true));
        assert_eq!(bound_draft_settings.pages_per_batch, 4);
        assert_eq!(after_update.settings.pages_per_batch, 8);

        // 5. Even if an older production-draft.json on disk has stale model settings with an older mtime than settings.json,
        // reopening ProductionRunner heals draft.request.settings from the newer settings.json.
        let mut stale_draft = after_update.production_draft.unwrap();
        stale_draft.request.settings.text_model.model = "gemini-2.5-flash".into();
        write_json(&runner.profile_access, &dir.path().join("production-draft.json"), &stale_draft).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        write_json(&runner.profile_access, &dir.path().join("settings.json"), &healed_settings).unwrap();
        let reopened = ProductionRunner::open(dir.path()).unwrap();
        assert_eq!(reopened.snapshot.production_draft.as_ref().unwrap().request.settings.text_model.model, "gemini-3.8-flash");

        // 6. Legacy saved media model in settings.json (Runs [084]-[087] gemini-3.1-flash-lite-image) must NOT brick
        // ProductionRunner::open() on startup; it clears the invalid text slot with a warning and heals stale draft image_model.
        let legacy_bad_settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-lite-image".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
            ..Default::default()
        };
        let mut legacy_draft = reopened.snapshot.production_draft.clone().unwrap();
        legacy_draft.request.settings.text_model.model = "gemini-3.1-flash-lite-image".into();
        legacy_draft.request.settings.image_model =
            ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        write_json(&reopened.profile_access, &dir.path().join("settings.json"), &legacy_bad_settings).unwrap();
        write_json(&reopened.profile_access, &dir.path().join("production-draft.json"), &legacy_draft).unwrap();

        let healed_open = ProductionRunner::open(dir.path()).unwrap();
        assert!(healed_open.snapshot.available);
        assert!(healed_open.snapshot.settings.text_model.model.is_empty());
        assert!(healed_open.snapshot.message.contains("Cleared legacy non-text model"), "msg={}", healed_open.snapshot.message);
        let healed_draft = healed_open.snapshot.production_draft.as_ref().unwrap();
        assert!(healed_draft.request.settings.text_model.model.is_empty());
        assert_eq!(healed_draft.request.settings.image_model.model, "gemini-3.1-flash-image");
    }

    #[test]
    fn ai28_bound_production_draft_preserves_target_total_pages_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let task = TaskRequest {
            prompt: "Create a 32-page illustrated science workbook.".into(),
            settings: AiSettings {
                text_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-pro".into(), endpoint: None },
                pages_per_batch: 2,
                facing_pages: true,
                ..Default::default()
            },
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 32 },
            export_directory: None,
        };
        runner.save_production_draft(&session, &task).unwrap();
        assert_eq!(runner.snapshot.production_draft.as_ref().unwrap().progress.target_total_pages, Some(32));
        assert_eq!(runner.snapshot.production_draft.as_ref().unwrap().progress.completed_pages, 0);

        session.execute("file.new", &json!({"pages": 32, "width": 595.2756, "height": 841.8898, "facingPages": true})).unwrap();
        session.execute("frame.create", &json!({"spread": 0, "rect": [72, 72, 240, 140], "content": "text", "text": "Cover"})).unwrap();
        runner.bind_production_draft(&session, "book-32-test").unwrap();

        let reopened = ProductionRunner::open(dir.path()).unwrap();
        let draft = reopened.snapshot.production_draft.as_ref().expect("bound draft persisted");
        assert!(matches!(draft.request.target, TaskTarget::ContinueBook));
        assert_eq!(draft.progress.completed_pages, 1);
        assert_eq!(draft.progress.target_total_pages, Some(32));
    }

    #[test]
    fn ai28_single_page_arabic_one_shot_publish_sets_rtl_binding_and_r2l_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let export_dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let request = TaskRequest {
            prompt: "صمّم صفحة غلاف عربية لكتاب علوم للصف الأول بعنوان عالم النباتات.".into(),
            settings: AiSettings {
                facing_pages: false,
                right_to_left_binding: None,
                pages_per_batch: 0,
                output_mode: designcraft_ai_types::OutputMode::NativeOnly,
                ..Default::default()
            },
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 1 },
            export_directory: Some(export_dir.path().to_string_lossy().into_owned()),
        };
        let plan = designcraft_ai_types::protocol::ProductionPlan {
            title: "غلاف كتاب العلوم".into(),
            commands: vec![designcraft_ai_types::EngineAction {
                command: "frame.create".into(),
                params: json!({"spread": 0, "rect": [72, 72, 300, 160]}),
            }],
            images: vec![],
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(worker::Message::Finished(worker::Outcome { result: Ok(worker::Product::Plan(plan)), usage: Default::default() })).unwrap();
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
            id: "oneshot-ar-single".into(),
            started: std::time::Instant::now(),
            task: Some(request.clone()),
            expected: None,
            expected_prefs: None,
            skills: vec![],
            settings: request.settings,
        });
        let snap = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(snap.stage, "completed", "msg={}", snap.message);
        let doc = &session.doc().unwrap().doc;
        assert!(!doc.settings.facing_pages);
        assert!(doc.settings.right_to_left_binding, "single-page Arabic one-shot publish must set right_to_left_binding");
        let pdf_bytes = std::fs::read(export_dir.path().join("production-oneshot-ar-single.pdf")).unwrap();
        let pdf_raw = String::from_utf8_lossy(&pdf_bytes);
        assert!(pdf_raw.contains("/Direction") && pdf_raw.contains("/R2L"), "expected /Direction /R2L in exported single-page Arabic PDF");
    }
}
