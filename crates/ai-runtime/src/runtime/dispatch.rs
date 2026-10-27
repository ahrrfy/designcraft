use super::*;

impl ProductionRunner {
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

    pub(super) fn dispatch(&mut self, session: &mut Session, request: AiRequest) -> Result<(), String> {
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
            AiRequest::SetApiKey { provider, mut key } => {
                let secret = SecretString::new(key.take());
                let selection = self.configured_key_selection(provider)?;
                if !secret.expose().trim().is_empty() {
                    self.store_session_key(&selection, secret)?;
                } else {
                    self.keys.retain(|stored| !stored.matches(&selection));
                }
                self.refresh_credentials();
                self.snapshot.message = "Session credential updated".into();
            }
            AiRequest::SaveApiKey { selection, mut key } => {
                let secret = SecretString::new(key.take());
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
            AiRequest::ConfirmImageRegeneration { job_id } => self.confirm_image_regeneration(session, &job_id)?,
            AiRequest::DismissImageRegeneration { job_id } => self.dismiss_image_regeneration(&job_id)?,
        }
        Ok(())
    }

    pub(super) fn dismiss_image_regeneration(&mut self, job_id: &str) -> Result<(), String> {
        let Some(pending) = self.pending_image_regen.as_ref() else {
            self.snapshot.pending_image_approval = None;
            return Ok(());
        };
        if pending.approval.job_id != job_id {
            return Err("The pending image regeneration request changed; review the current confirmation prompt".into());
        }
        self.pending_image_regen = None;
        self.snapshot.pending_image_approval = None;
        self.snapshot.message = "Kept text and layout fixes without regenerating paid cloud illustrations".into();
        Ok(())
    }

    pub(super) fn confirm_image_regeneration(&mut self, session: &mut Session, job_id: &str) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("Only one AI task can run in this window".into());
        }
        let Some(pending_regen) = self.pending_image_regen.as_ref() else {
            return Err("There is no pending image regeneration awaiting confirmation".into());
        };
        if pending_regen.approval.job_id != job_id {
            return Err("The pending image regeneration request changed; review the current confirmation prompt".into());
        }
        let target_uid = pending_regen.approval.document_uid;
        let Some(target_index) = session.documents().iter().position(|doc| doc.uid == target_uid) else {
            self.pending_image_regen = None;
            self.snapshot.pending_image_approval = None;
            return Err("The document closed before image regeneration was confirmed".into());
        };
        let target = session.documents().get(target_index).ok_or("Target document disappeared")?;
        if target.interaction.is_some() || target.preview_stash.is_some() {
            return Err("Finish the current interaction, IME composition or preview before starting image regeneration".into());
        }
        if !std::sync::Arc::ptr_eq(&target.doc, &pending_regen.expected_doc) {
            self.pending_image_regen = None;
            self.snapshot.pending_image_approval = None;
            return Err("The document was modified after the layout fix; run the prompt again to queue updated image slots".into());
        }
        let base = target.clone();
        let target_revision = target.revision;
        let expected_doc = pending_regen.expected_doc.clone();
        let mut effective_settings = pending_regen.task.settings.clone();
        if (self.snapshot.settings.image_model.provider == ProviderKind::ComfyUi || !self.snapshot.settings.image_model.model.trim().is_empty())
            && validate_endpoint(&self.snapshot.settings.image_model).is_ok()
        {
            effective_settings.image_model = self.snapshot.settings.image_model.clone();
            effective_settings.image_rates = self.snapshot.settings.image_rates.clone();
            if self.snapshot.settings.image_model.provider == ProviderKind::ComfyUi && self.snapshot.settings.comfy_workflow.is_some() {
                effective_settings.comfy_workflow = self.snapshot.settings.comfy_workflow.clone();
            }
        }
        effective_settings = crate::agent_loop::effective_open_doc_image_settings(&effective_settings);
        let per_image_reqs = if effective_settings.verify_generated_images { 4u32 } else { 1u32 };
        let needed_reqs = u32::try_from(pending_regen.approval.count).unwrap_or(u32::MAX).saturating_mul(per_image_reqs).clamp(1, 10_000);
        effective_settings.budget.max_requests = effective_settings.budget.max_requests.max(needed_reqs);
        validate_settings(&effective_settings)?;
        require_generation_models(&effective_settings)?;
        let mut task = pending_regen.task.clone();
        task.settings = effective_settings.clone();
        let skills = pending_regen.skills.clone();
        let work = worker::Work::DeferredImages {
            title: "Regenerate Document Illustrations".into(),
            batches: pending_regen.batches.clone(),
            settings: effective_settings.clone(),
            style_profile: pending_regen.style_profile.clone(),
            cache_root: Some(self.root.join("production-cache-v1")),
            min_ppi: pending_regen.min_ppi,
            base,
            prefs: session.prefs.clone(),
        };
        self.launch_with_doc(work, Some(task), Some((target_uid, target_revision)), Some(expected_doc), skills, effective_settings)?;
        self.pending_image_regen = None;
        self.snapshot.pending_image_approval = None;
        self.snapshot.message = "Generating confirmed illustrations…".into();
        Ok(())
    }

    pub(super) fn start(&mut self, session: &mut Session, mut task: TaskRequest) -> Result<(), String> {
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
        let (expected, expected_doc) = match task.target {
            TaskTarget::CurrentDocument => {
                let current = session.doc().map_err(|e| e.to_string())?;
                (Some((current.uid, current.revision)), Some(current.doc.clone()))
            }
            TaskTarget::ContinueBook => {
                let current = session.doc().map_err(|e| e.to_string())?;
                if task.settings.pages_per_batch == 0 {
                    return Err("Continuing a book requires reviewed batches".into());
                }
                (Some((current.uid, current.revision)), Some(current.doc.clone()))
            }
            TaskTarget::NewA4 { pages } => {
                if !(1..=1000).contains(&pages) {
                    return Err("AI new documents support 1–1000 pages".into());
                }
                if task.settings.pages_per_batch == 0 && pages > 8 {
                    return Err("Books over eight pages require reviewed batches".into());
                }
                (None, None)
            }
        };
        if let Some((uid, _)) = expected
            && self.pending_image_regen.as_ref().is_some_and(|p| p.approval.document_uid == uid)
        {
            self.pending_image_regen = None;
            self.snapshot.pending_image_approval = None;
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
        self.launch_with_doc(work, Some(task), expected, expected_doc, pins, settings)
    }

    pub(super) fn preview_production(&mut self, session: &Session, task: TaskRequest) -> Result<(), String> {
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

    pub(super) fn start_skill(
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

    pub(super) fn launch(
        &mut self,
        work: worker::Work,
        task: Option<TaskRequest>,
        expected: Option<(u64, u64)>,
        skills: Vec<(String, String)>,
        settings: AiSettings,
    ) -> Result<(), String> {
        self.launch_with_doc(work, task, expected, None, skills, settings)
    }

    pub(super) fn launch_with_doc(
        &mut self,
        work: worker::Work,
        task: Option<TaskRequest>,
        expected: Option<(u64, u64)>,
        expected_doc: Option<std::sync::Arc<designcraft_engine::doc::Document>>,
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
            worker::Work::DeferredImages { settings, .. } => {
                let mut selections = vec![settings.image_model.clone()];
                if settings.verify_generated_images {
                    selections.push(settings.text_model.clone());
                }
                selections
            }
            worker::Work::Skill { settings, .. } => vec![settings.text_model.clone()],
        };
        self.prepare_credentials(&selections)?;
        let keys = self.worker_keys();
        let roots = file_access::current();
        let expected_prefs = match &work {
            worker::Work::Production { prefs, .. } | worker::Work::DeferredImages { prefs, .. } => Some(prefs.clone()),
            _ => None,
        };
        let worker = worker::spawn(work, keys, roots.clone())?;
        self.pending = Some(Pending {
            roots,
            worker,
            id,
            started: Instant::now(),
            task,
            expected,
            expected_doc,
            expected_prefs,
            skills,
            settings,
            deferred_outcome: None,
            deferred_batch: None,
        });
        self.snapshot.busy = true;
        self.snapshot.current_usage = Default::default();
        self.snapshot.current_issues.clear();
        self.snapshot.progress = 0.0;
        self.snapshot.stage = "generating".into();
        self.snapshot.message = "AI task started".into();
        Ok(())
    }

    pub(super) fn poll(&mut self, session: &mut Session) {
        let Some(mut pending) = self.pending.take() else { return };
        if let (Some(job), Some(snapshot)) = (&self.batch, &mut self.snapshot.batch) {
            snapshot.elapsed_ms = job.snapshot_elapsed().saturating_add(pending.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        }
        let target_uid = pending.expected.map(|(uid, _)| uid).or_else(|| self.batch.as_ref().and_then(|job| job.snapshot.document_uid));
        let mut completed = pending.deferred_outcome.take();
        let mut completed_batch = pending.deferred_batch.take();
        let mut disconnected = false;
        if completed.is_none() && completed_batch.is_none() {
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
                        completed_batch = Some((outcome, ledger));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        let cancelled = pending.worker.cancel.load(Ordering::Relaxed);
        if let Some((outcome, ledger)) = completed_batch {
            if !cancelled && outcome.result.is_ok() && is_publication_temporarily_busy(session, target_uid) {
                pending.deferred_batch = Some((outcome, ledger));
                self.snapshot.stage = "waiting_for_idle".into();
                self.snapshot.message =
                    "AI batch finished; waiting for your current canvas drag, IME input, or preview to finish before publishing".into();
                self.pending = Some(pending);
                return;
            }
            self.finish_batch(session, pending, outcome, *ledger);
            return;
        }
        if let Some(outcome) = completed {
            let needs_doc_publish = matches!(&outcome.result, Ok(worker::Product::Plan(_) | worker::Product::Agentic { .. }));
            if !cancelled && needs_doc_publish && is_publication_temporarily_busy(session, target_uid) {
                pending.deferred_outcome = Some(outcome);
                self.snapshot.stage = "waiting_for_idle".into();
                self.snapshot.message =
                    "AI result is ready; waiting for your current canvas drag, IME input, or preview to finish before applying".into();
                self.pending = Some(pending);
                return;
            }
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

    pub(super) fn finish(&mut self, session: &mut Session, pending: Pending, outcome: worker::Outcome) {
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
                    && batch::layout_relevant_prefs_value(expected)? != batch::layout_relevant_prefs_value(&session.prefs)?
                {
                    return Err(
                        "Application preferences changed during generation; result was not published. Run the task again with current preferences"
                            .into(),
                    );
                }
                self.publish(session, &pending.task, pending.expected, pending.expected_doc.as_ref(), product, &mut report)
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

    pub(super) fn publish(
        &mut self,
        session: &mut Session,
        task: &Option<TaskRequest>,
        expected: Option<(u64, u64)>,
        expected_doc: Option<&std::sync::Arc<designcraft_engine::doc::Document>>,
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
                    let doc_unchanged = match expected_doc {
                        Some(pinned_doc) => std::sync::Arc::ptr_eq(&target.doc, pinned_doc),
                        None => target.revision == revision,
                    };
                    if !doc_unchanged {
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
                let (page_params, pinned_profile) = match task.target {
                    TaskTarget::NewA4 { pages } => {
                        let prof = self.pinned_design_profile(&report.skills)?;
                        let params = production_document_params(prof.as_ref(), pages, &task.settings, &task.prompt)?;
                        (Some(params), prof)
                    }
                    _ => (None, None),
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
                        crate::template_plan::initialize_relational_book_master(session, pinned_profile.as_ref(), &task.settings, &task.prompt)?;
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
            worker::Product::Agentic { title, summary, chunks, deferred_images } => {
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
                    let doc_unchanged = match expected_doc {
                        Some(pinned_doc) => std::sync::Arc::ptr_eq(&target.doc, pinned_doc),
                        None => target.revision == revision,
                    };
                    if !doc_unchanged {
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
                let context = compact_context(session)?;
                let deferred_count: usize = deferred_images.iter().map(|b| b.images.len()).sum();
                if let Err(err) = write_json(
                    &self.profile_access,
                    &trace,
                    &json!({
                        "schemaVersion": 1,
                        "taskId": report.id,
                        "settings": task.settings,
                        "skills": report.skills,
                        "designStyle": report.design_style,
                        "base": context,
                        "agentic": {
                            "title": &title,
                            "summary": &summary,
                            "chunks": &chunks,
                            "deferredImageCount": deferred_count,
                        }
                    }),
                ) {
                    if switched_tab && let Some(index) = previous_active {
                        session.set_active(index);
                    }
                    return Err(err);
                }
                report.artifacts.push(trace.to_string_lossy().into_owned());
                let min_ppi =
                    crate::print_contract::PrintContract::from_prompt(&task.prompt)?.and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
                let total_actions: usize = chunks.iter().map(Vec::len).sum();
                let has_placed_images = chunks.iter().flatten().any(|a| a.command == "frame.placeImage");
                let apply_mode = if has_placed_images { designcraft_ai_types::OutputMode::NativeWithImages } else { task.settings.output_mode };
                let non_empty_chunks: Vec<&Vec<EngineAction>> = chunks.iter().filter(|c| !c.is_empty()).collect();
                let published = (|| {
                    if let [single] = non_empty_chunks.as_slice() {
                        let current = session.doc().map_err(|e| e.to_string())?;
                        let params = json!({
                            "commands": single,
                            "outputMode": apply_mode,
                            "label": &title,
                            "expectedDocumentUid": current.uid,
                            "expectedRevision": current.revision,
                            "requirePreflight": true,
                            "minPpi": min_ppi,
                        });
                        session.execute("production.apply", &params).map_err(|e| e.to_string())?;
                    } else if non_empty_chunks.len() >= 2 {
                        let current = session.doc().map_err(|e| e.to_string())?;
                        let group_chunks: Vec<Value> = non_empty_chunks
                            .iter()
                            .enumerate()
                            .map(|(idx, cmds)| json!({"commands": cmds, "label": format!("{} ({})", title, idx + 1)}))
                            .collect();
                        let params = json!({
                            "chunks": group_chunks,
                            "outputMode": apply_mode,
                            "label": &title,
                            "expectedDocumentUid": current.uid,
                            "expectedRevision": current.revision,
                            "requirePreflight": true,
                            "minPpi": min_ppi,
                        });
                        session.execute("production.applyGroup", &params).map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(())
                })();
                if let Err(error) = published {
                    if switched_tab && let Some(index) = previous_active {
                        session.set_active(index);
                    }
                    return Err(error);
                }
                report.actions = total_actions;
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
                if !deferred_images.is_empty() && deferred_count > 0 {
                    let current = session.doc().map_err(|e| e.to_string())?;
                    let img_sel = if task.settings.image_model.provider == ProviderKind::ComfyUi || !task.settings.image_model.model.trim().is_empty()
                    {
                        &task.settings.image_model
                    } else {
                        &self.snapshot.settings.image_model
                    };
                    let estimated_cost_usd = if matches!(img_sel.provider, ProviderKind::ComfyUi | ProviderKind::Ollama) {
                        Some(0.0)
                    } else {
                        task.settings
                            .image_rates
                            .as_ref()
                            .or(self.snapshot.settings.image_rates.as_ref())
                            .or(task.settings.budget.rates.as_ref())
                            .and_then(|r| r.image_per_request.or(r.image_reserve_usd))
                            .map(|unit| unit * (deferred_count as f64))
                    };
                    let style_profile = report.skills.iter().find_map(|(id, _)| self.registry.snapshot(id, &[]).ok().and_then(|s| s.design_profile));
                    let joined_summary = deferred_images.iter().map(|b| b.summary.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • ");
                    let approval_summary = if joined_summary.is_empty() {
                        format!("{deferred_count} damaged or composite illustrations ready to regenerate")
                    } else {
                        joined_summary
                    };
                    let approval = PendingImageApproval {
                        job_id: report.id.clone(),
                        document_uid: current.uid,
                        count: deferred_count,
                        provider: img_sel.provider,
                        model: img_sel.model.clone(),
                        estimated_cost_usd,
                        summary: approval_summary,
                    };
                    self.pending_image_regen = Some(PendingImageRegen {
                        approval: approval.clone(),
                        task: task.clone(),
                        skills: report.skills.clone(),
                        style_profile,
                        batches: deferred_images,
                        min_ppi,
                        expected_doc: current.doc.clone(),
                    });
                    self.snapshot.pending_image_approval = Some(approval);
                    let lead = if summary.trim().is_empty() {
                        format!("Applied {} editable native actions", report.actions)
                    } else {
                        summary.trim().to_string()
                    };
                    report.message = format!(
                        "{lead} ({} actions; {} preflight notices). Confirm below to regenerate {deferred_count} illustration(s) via {:?}.",
                        report.actions,
                        report.issues.len(),
                        img_sel.provider
                    );
                } else if summary.trim().is_empty() {
                    report.message = format!("Applied {} editable native actions; {} preflight notices", report.actions, report.issues.len());
                } else {
                    report.message = format!("{} ({} native actions; {} preflight notices)", summary.trim(), report.actions, report.issues.len());
                }
            }
        }
        Ok(())
    }
}
