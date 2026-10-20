use crate::vault::{SessionKey, Vault};
use crate::{ledger::UsageLedger, science, worker};
use designcraft_ai_providers::{SecretString, validate_endpoint};
use designcraft_ai_skills::{ApprovalEvidence, Provenance, RevisionStatus, SkillRegistry, SkillSnapshot};
use designcraft_ai_types::protocol::*;
use designcraft_ai_types::{EngineAction, ProviderKind};
use designcraft_engine::{Session, file_access};
use serde_json::{Value, json};
use std::collections::BTreeSet;
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
        let settings = read_json(&profile_access, &root.join("settings.json"), 1024 * 1024)?
            .map(serde_json::from_value::<AiSettings>)
            .transpose()
            .map_err(|e| format!("AI settings: {e}"))?
            .unwrap_or_default();
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
        let mut host = Self {
            root,
            profile_access,
            registry,
            snapshot: AiSnapshot { available: true, settings, reports, batch: recovered_batch, ..Default::default() },
            keys: Vec::new(),
            vault,
            pending: None,
            batch: None,
            sample: None,
            sequence: 0,
            print_attempts,
            local_server: Default::default(),
        };
        host.refresh_catalog();
        host.refresh_credentials();
        Ok(host)
    }

    pub fn handle(&mut self, session: &mut Session, request: AiRequest) -> AiSnapshot {
        // Cancellation must win even when a completed provider response is queued.
        if matches!(request, AiRequest::Cancel)
            && let Some(job) = &self.pending
        {
            job.worker.cancel.store(true, Ordering::Relaxed);
        }
        self.poll(session);
        self.refresh_local_server();
        if self.batch.is_some()
            && !matches!(request, AiRequest::Poll | AiRequest::Cancel | AiRequest::ApproveBatch { .. } | AiRequest::InspectSkill { .. })
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
                self.snapshot.stage = "failed".into();
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
            AiRequest::UpdateSettings(settings) => {
                validate_settings(&settings)?;
                write_json(&self.profile_access, &self.root.join("settings.json"), &settings)?;
                self.snapshot.settings = settings;
                self.refresh_credentials();
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
                if pdf {
                    ensure_preflight(session)?;
                }
                let params = if pdf { json!({"path":path,"spreads":false,"bleed":false,"marks":false}) } else { json!({"path":path}) };
                session.execute(if pdf { "file.exportPdf" } else { "file.saveAs" }, &params).map_err(|e| e.to_string())?;
                self.snapshot.message = format!("Exported {path}");
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

    fn start(&mut self, session: &mut Session, task: TaskRequest) -> Result<(), String> {
        if let Some(directory) = &task.export_directory {
            file_access::check_write(directory)?;
        }
        validate_settings(&task.settings)?;
        require_text_model(&task.settings)?;
        if task.prompt.trim().is_empty() || task.prompt.len() > 32_768 {
            return Err("Enter a prompt of at most 32768 UTF-8 bytes".into());
        }
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
            self.prepare_skills_for_style(session, &task.prompt, task.settings.design_style.as_deref())?
        } else {
            let mut ids = task.skill_ids.clone();
            if let Some(id) = task.settings.design_style.as_deref() {
                self.prepare_style(session, id)?;
                if !ids.iter().any(|selected| selected == id) {
                    ids.push(id.into());
                }
            }
            ids
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
        let (base, context) = task_document_context(session, &task.target, profile)?;
        if let TaskTarget::ContinueBook = task.target {
            batch::continuation_progress(&base, profile)?;
        }
        if let Some(profile) = profile {
            crate::design::check_geometry(Some(&base), &base, profile)?;
        }
        let settings = task.settings.clone();
        if matches!(task.target, TaskTarget::ContinueBook)
            || matches!(task.target, TaskTarget::NewA4 { pages } if settings.pages_per_batch > 0 && pages > settings.pages_per_batch)
        {
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
        if prompt.trim().is_empty() || prompt.len() > 32_768 {
            return Err("Enter a bounded description for the new skill".into());
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
        let worker = worker::spawn(work, keys, roots.clone())?;
        self.pending = Some(Pending { roots, worker, id, started: Instant::now(), task, expected, skills, settings });
        self.snapshot.busy = true;
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
                Ok(worker::Message::Finished(outcome)) => {
                    completed = Some(outcome);
                    break;
                }
                Ok(worker::Message::FinishedWithLedger(outcome, ledger)) => {
                    self.finish_batch(session, pending, outcome, ledger);
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
            issues: Vec::new(),
            artifacts: Vec::new(),
            message: String::new(),
        };
        let result = if cancelled {
            Err("AI task cancelled; document changes were not published".into())
        } else {
            file_access::confine(pending.roots.as_ref(), || {
                outcome.result.and_then(|product| self.publish(session, &pending.task, pending.expected, product, &mut report))
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
            worker::Product::BatchPlan { .. } => return Err("Batch proof result requires its gated batch job".into()),
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
                if let Some((uid, revision)) = expected {
                    let current = session.doc().map_err(|e| e.to_string())?;
                    if current.uid != uid || current.revision != revision {
                        return Err(
                            "The document changed during generation; result was not applied. Run the task again against the current document".into(),
                        );
                    }
                }
                let runs = self.root.join("runs");
                self.profile_access.check(&runs, file_access::Access::Write)?;
                std::fs::create_dir_all(&runs).map_err(|e| e.to_string())?;
                let trace = runs.join(format!("{}.json", report.id));
                let page_params = match task.target {
                    TaskTarget::NewA4 { pages } => Some(self.pinned_document_params(&report.skills, pages)?),
                    _ => None,
                };
                let context = match task.target {
                    TaskTarget::CurrentDocument | TaskTarget::ContinueBook => compact_context(session)?,
                    TaskTarget::NewA4 { pages } => json!({"newA4Pages":pages,"documentParams":page_params}),
                };
                write_json(
                    &self.profile_access,
                    &trace,
                    &json!({"schemaVersion":1,"taskId":report.id,"settings":task.settings,"skills":report.skills,"designStyle":report.design_style,"base":context,"plan":plan}),
                )?;
                report.artifacts.push(trace.to_string_lossy().into_owned());
                let previous_active = session.active_index();
                let new_document = matches!(task.target, TaskTarget::NewA4 { .. });
                if let Some(params) = page_params {
                    session.execute("file.new", &params).map_err(|e| e.to_string())?;
                }
                let current = session.doc().map_err(|e| e.to_string())?;
                let params = json!({"commands":plan.commands,"outputMode":task.settings.output_mode,"label":plan.title,"expectedDocumentUid":current.uid,"expectedRevision":current.revision,"requirePreflight":true});
                if let Err(error) = session.execute("production.apply", &params) {
                    if new_document {
                        session.execute("file.close", &json!({})).map_err(|e| format!("{error}; could not close the failed new document: {e}"))?;
                        if let Some(index) = previous_active {
                            session.execute("file.activate", &json!({"index":index})).map_err(|e| e.to_string())?;
                        }
                    }
                    return Err(error.to_string());
                }
                report.actions = plan.commands.len();
                report.title = plan.title;
                let preflight = ensure_preflight(session)?;
                report.issues = preflight.get("issues").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(directory) = &task.export_directory {
                    file_access::check_write(directory)?;
                    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
                    for (extension, command) in [("designcraft", "file.saveAs"), ("pdf", "file.exportPdf")] {
                        let path = Path::new(directory).join(format!("production-{}.{}", report.id, extension));
                        let args =
                            if extension == "pdf" { json!({"path":path,"spreads":false,"bleed":false,"marks":false}) } else { json!({"path":path}) };
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

    fn pinned_document_params(&self, pins: &[(String, String)], pages: u32) -> Result<Value, String> {
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
        Ok(crate::design::document_params(profile, pages))
    }

    fn prepare_automatic_skills(&mut self, session: &Session, prompt: &str) -> Result<Vec<String>, String> {
        self.prepare_skills_for_style(session, prompt, None)
    }

    fn prepare_skills_for_style(&mut self, session: &Session, prompt: &str, style_id: Option<&str>) -> Result<Vec<String>, String> {
        if prompt.trim().is_empty() || prompt.len() > 32_768 {
            return Err("Enter a prompt of at most 32768 UTF-8 bytes".into());
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
                    SkillCard {
                        design_profile: revision.manifest.design_profile.clone(),
                        id: entry.skill_id.clone(),
                        name: revision.name,
                        description: revision.description,
                        active: entry.active_revision.as_ref() == Some(&revision.revision),
                        revision: revision.revision,
                        approved: revision.status == RevisionStatus::Approved,
                        automatically_validated: revision.approval.as_ref().is_some_and(|approval| approval.automatic),
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
}

/// Preserve immutable guidance and executable design profiles within the task
/// budget. Command planners can omit optional sample templates, largest first;
/// the content compiler never needs non-style command examples.
fn bounded_task_skills(mut skills: Vec<SkillSnapshot>, target: &TaskTarget) -> Result<Vec<SkillSnapshot>, String> {
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
            if bytes > 64 * 1024 {
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
    Err("Selected skills exceed the 64-KiB task context limit; reduce references or split the skill before retrying".into())
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

fn skill_match_score(prompt: &str, entry: &designcraft_ai_skills::CatalogEntry) -> usize {
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
        "وحدة",
        "درس",
        "رياضيات",
        "علوم",
        "حاسوب",
        "الحاسوب",
        "كمبيوتر",
        "برمجة",
        "تقنية",
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
    ]);
    let children = mentions(&[
        "أطفال",
        "اطفال",
        "طفل",
        "روضة",
        "للصغار",
        "ابتدائي",
        "مدرسي",
        "طلاب",
        "تلاميذ",
        "kindergarten",
        "preschool",
        "children",
        "early years",
        "early-years",
        "primary",
    ]);
    let visual = (children || intl || computing) && book
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
    let kg = (children || intl || computing) && (book || visual);
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
    let purpose = match entry.skill_id.as_str() {
        "native-engine-workflow" => 64,
        "arabic-english-typography" if arabic => 56,
        "school-print-preflight" if print => 48,
        "curriculum-print-design-studio" if book && print => 41,
        "kg-garnet-ict-computing" if kg && computing => 46,
        "kg-international-curriculum-architect" if (kg || book) && architecture => 45,
        "kg-cambridge-thinking-cpa"
            if kg
                && (computing
                    || intl
                    || mentions(&[
                        "رياضيات",
                        "حساب",
                        "العد",
                        "كميات",
                        "منطق",
                        "محسوس",
                        "قواعد",
                        "math",
                        "mathematics",
                        "counting",
                        "subitiz",
                        "grammar",
                        "cpa",
                    ])) =>
        {
            44
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
    lexical.saturating_add(purpose)
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
    if settings.pages_per_batch > 32 {
        return Err("Batch size must be 1–32 pages, or zero for a short one-shot task".into());
    }
    validate_endpoint(&settings.text_model)?;
    validate_endpoint(&settings.image_model)?;
    if settings.text_model.model.len() > 256 || settings.image_model.model.len() > 256 {
        return Err("Model name is too long".into());
    }
    UsageLedger::new(settings.budget.clone())?;
    if let Some(rates) = &settings.image_rates {
        crate::ledger::validate_rates(rates)?;
    }
    Ok(())
}
fn require_text_model(settings: &AiSettings) -> Result<(), String> {
    if settings.text_model.model.trim().is_empty() {
        return Err("Choose a text model before starting generation".into());
    }
    if matches!(settings.text_model.provider, ProviderKind::ComfyUi) {
        return Err("ComfyUI is an image provider; choose a text provider for planning".into());
    }
    Ok(())
}
fn task_document_context(
    session: &Session,
    target: &TaskTarget,
    profile: Option<&designcraft_ai_types::DesignProfile>,
) -> Result<(designcraft_engine::DocState, Value), String> {
    match target {
        TaskTarget::CurrentDocument | TaskTarget::ContinueBook => Ok((session.doc().map_err(|e| e.to_string())?.clone(), compact_context(session)?)),
        TaskTarget::NewA4 { pages } => {
            if let Some(profile) = profile {
                profile.validate()?;
            }
            let mut scratch = Session::new();
            scratch.execute("file.new", &crate::design::document_params(profile, *pages)).map_err(|e| e.to_string())?;
            let context = compact_context(&scratch)?;
            Ok((scratch.doc().map_err(|e| e.to_string())?.clone(), context))
        }
    }
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
    let report = session.execute("preflight.run", &json!({"minPpi":150})).map_err(|e| e.to_string())?;
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
    for (spread_index, spread) in doc.spreads.iter().enumerate() {
        for item in &spread.items {
            if objects.len() >= 200 {
                break;
            }
            let bounds = item.bounds();
            let text = item.text_frame().and_then(|t| doc.stories.get(&t.story)).map(|s| s.text.chars().take(500).collect::<String>());
            objects.push(
                json!({"id":item.id.0,"label":item.label,"spread":spread_index,"bounds":[bounds.x0,bounds.y0,bounds.x1,bounds.y1],"text":text}),
            );
        }
        if objects.len() >= 200 {
            break;
        }
    }
    Ok(
        json!({"documentUid":state.uid,"revision":state.revision,"title":doc.title,"pageCount":doc.page_count(),"pageWidth":doc.settings.page_width,"pageHeight":doc.settings.page_height,"facingPages":doc.settings.facing_pages,"selection":state.selection,"objects":objects,"objectsTruncated":objects.len()==200,"fontFaces":font_faces,"swatchNames":doc.swatches.iter().take(64).map(|s|s.name.chars().take(160).collect::<String>()).collect::<Vec<_>>(),"units":"points, 72 per inch; rect=[left,top,right,bottom]"}),
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
        let (base, context) = task_document_context(&session, &target, Some(&profile)).unwrap();
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
        assert!(
            bounded_task_skills(vec![executable], &TaskTarget::CurrentDocument).is_err(),
            "Command planners must retain executable profile templates"
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
        assert!(bounded_task_skills(vec![executable.clone()], &TaskTarget::CurrentDocument).is_err());
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
}
