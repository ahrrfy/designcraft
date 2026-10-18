use crate::vault::{SessionKey, Vault};
use crate::{ledger::UsageLedger, science, worker};
use designcraft_ai_providers::{SecretString, validate_endpoint};
use designcraft_ai_skills::{ApprovalEvidence, Provenance, RevisionStatus, SkillRegistry};
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
            AiRequest::GenerateSkill { prompt, reference_paths, settings } => self.start_skill(prompt, reference_paths, None, settings)?,
            AiRequest::GenerateSkillFromDocument { prompt, settings } => {
                let context = compact_context(session)?;
                self.start_skill(prompt, Vec::new(), Some(context), settings)?;
            }
            AiRequest::RunSkillSample { id, revision } => self.run_sample(session, &id, &revision)?,
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
            TaskTarget::NewA4 { pages } => {
                if !(1..=1000).contains(&pages) {
                    return Err("AI new documents support 1–1000 A4 pages".into());
                }
                if task.settings.pages_per_batch == 0 && pages > 8 {
                    return Err("Books over eight pages require reviewed batches".into());
                }
                None
            }
        };
        let (base, context) = match task.target {
            TaskTarget::CurrentDocument => (session.doc().map_err(|e| e.to_string())?.clone(), compact_context(session)?),
            TaskTarget::NewA4 { pages } => {
                let mut scratch = Session::new();
                scratch.execute("file.new", &new_a4(pages)).map_err(|e| e.to_string())?;
                let context = compact_context(&scratch)?;
                (scratch.doc().map_err(|e| e.to_string())?.clone(), context)
            }
        };
        let ids = if task.auto_select_skills { self.choose_skills(&task.prompt) } else { task.skill_ids.clone() };
        if ids.len() > 8 {
            return Err("Select at most eight skills per task".into());
        }
        let mut snapshots = Vec::new();
        for id in ids {
            let card = self.registry.catalog("").into_iter().find(|e| e.skill_id == id).ok_or_else(|| format!("Unknown skill: {id}"))?;
            let revision = card.active_revision.as_ref().ok_or_else(|| format!("Skill {id} requires approval"))?;
            let record = self.registry.revision(&id, revision).map_err(|e| e.to_string())?;
            let refs = record.manifest.references.iter().filter(|s| s.ends_with(".md") || s.ends_with(".txt")).take(2).cloned().collect::<Vec<_>>();
            snapshots.push(self.registry.snapshot(&id, &refs).map_err(|e| e.to_string())?);
        }
        let pins = snapshots.iter().map(|s| (s.skill_id.clone(), s.revision.clone())).collect();
        let settings = task.settings.clone();
        if let TaskTarget::NewA4 { pages } = task.target
            && settings.pages_per_batch > 0
            && pages > settings.pages_per_batch
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

    fn start_skill(&mut self, prompt: String, paths: Vec<String>, context: Option<Value>, settings: AiSettings) -> Result<(), String> {
        for path in &paths {
            file_access::check_read(path)?;
        }
        validate_settings(&settings)?;
        require_text_model(&settings)?;
        if prompt.trim().is_empty() || prompt.len() > 32_768 {
            return Err("Enter a bounded description for the new skill".into());
        }
        let work = worker::Work::Skill {
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
        if !Vault::available() {
            return Ok(());
        }
        for selection in selections {
            validate_endpoint(selection)?;
            if self.keys.iter().any(|key| key.matches(selection)) {
                continue;
            }
            if let Some(secret) = self.vault.load(selection)? {
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
                match presence {
                    Ok(persisted) => CredentialStatus {
                        selection,
                        persisted,
                        available: true,
                        message: if persisted {
                            "OS-protected key stored for this endpoint".into()
                        } else {
                            "No protected key stored for this endpoint".into()
                        },
                    },
                    Err(message) => CredentialStatus { selection, persisted: false, available: false, message },
                }
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
                let context = match task.target {
                    TaskTarget::CurrentDocument => compact_context(session)?,
                    TaskTarget::NewA4 { pages } => json!({"newA4Pages":pages}),
                };
                write_json(
                    &self.profile_access,
                    &trace,
                    &json!({"schemaVersion":1,"taskId":report.id,"settings":task.settings,"skills":report.skills,"base":context,"plan":plan}),
                )?;
                report.artifacts.push(trace.to_string_lossy().into_owned());
                let previous_active = session.active_index();
                let new_document = matches!(task.target, TaskTarget::NewA4 { .. });
                if let TaskTarget::NewA4 { pages } = task.target {
                    session.execute("file.new", &new_a4(pages)).map_err(|e| e.to_string())?;
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
        let words = prompt
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.chars().count() >= 3 || s.eq_ignore_ascii_case("a4"))
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut entries = self
            .registry
            .catalog("")
            .into_iter()
            .filter(|e| e.enabled && e.active_revision.is_some())
            .map(|e| {
                let haystack = format!("{} {} {}", e.skill_id, e.name, e.description).to_lowercase();
                (words.iter().filter(|w| haystack.contains(w.as_str())).count(), e.skill_id)
            })
            .filter(|(score, _)| *score > 0)
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        entries.into_iter().take(3).map(|(_, id)| id).collect()
    }
    fn refresh_catalog(&mut self) {
        let supported = supported_commands();
        self.snapshot.skills = self
            .registry
            .catalog("")
            .into_iter()
            .flat_map(|entry| {
                let supported = &supported;
                entry.revisions.into_iter().map(move |revision| SkillCard {
                    id: entry.skill_id.clone(),
                    name: revision.name,
                    description: revision.description,
                    active: entry.active_revision.as_ref() == Some(&revision.revision),
                    revision: revision.revision,
                    approved: revision.status == RevisionStatus::Approved,
                    enabled: entry.enabled,
                    compatible: revision.manifest.external_tools.is_empty()
                        && !revision.manifest.required_commands.is_empty()
                        && revision.manifest.required_commands.iter().all(|c| supported.contains(c)),
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
        let dir = self.root.join("samples");
        let path = dir.join(format!("{id}-{revision}.png"));
        self.profile_access.check(&dir, file_access::Access::Write)?;
        self.profile_access.check(&path, file_access::Access::Write)?;
        self.sample = None;
        let actions = self.registry.sample_actions(id, revision).map_err(|e| e.to_string())?;
        let mut scratch = Session::new();
        scratch.prefs = session.prefs.clone();
        scratch.execute("file.new", &new_a4(1)).map_err(|e| e.to_string())?;
        scratch
            .execute(
                "production.apply",
                &json!({"commands":actions,"outputMode":"native_only","label":"Skill review sample","requirePreflight":true}),
            )
            .map_err(|e| e.to_string())?;
        ensure_preflight(&mut scratch)?;
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
        self.sample = Some(SampleEvidence { id: id.into(), revision: revision.into(), actions, path });
        self.inspect_skill(id, revision)?;
        self.snapshot.message = "Sample executed with native tools and passed preflight; review its preview before approving".into();
        Ok(())
    }
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
        scratch.execute("file.new", &new_a4(pages)).map_err(|e| format!("Skill template {name}: {e}"))?;
        scratch
            .execute(
                "production.validate",
                &json!({
                    "commands":actions,"outputMode":"native_only","allowedCommands":allowed,
                    "label":"Skill template compatibility review","requirePreflight":true
                }),
            )
            .map_err(|e| format!("Skill template {name} is incompatible with the native engine: {e}"))?;
    }
    Ok(())
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
        json!({"documentUid":state.uid,"revision":state.revision,"title":doc.title,"pageCount":doc.page_count(),"pageWidth":doc.settings.page_width,"pageHeight":doc.settings.page_height,"facingPages":doc.settings.facing_pages,"selection":state.selection,"objects":objects,"objectsTruncated":objects.len()==200,"units":"points, 72 per inch; rect=[left,top,right,bottom]"}),
    )
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
            AiRequest::GenerateSkill { prompt: "Review reference".into(), reference_paths: vec![outside_path.clone()], settings: Default::default() },
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
            assert_eq!(snapshot.skills.len(), 4);
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
        assert_eq!(snapshot.skills.len(), 4);
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
