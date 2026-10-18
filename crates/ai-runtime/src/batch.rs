//! Human-gated book production. Persisted snapshots are evidence, never resumable authority.
use super::*;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_engine::{DocState, Prefs};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) struct BatchJob {
    task: TaskRequest,
    skills: Vec<SkillSnapshot>,
    roots: Option<Arc<file_access::AutomationRoots>>,
    ledger: Option<UsageLedger>,
    before: UsageSummary,
    snapshot: BatchSnapshot,
    prefs: Prefs,
}
impl BatchJob {
    pub(super) fn snapshot_elapsed(&self) -> u64 {
        self.snapshot.elapsed_ms
    }
}

pub(super) fn recover_snapshot(access: &std::sync::Arc<file_access::AutomationRoots>, root: &Path) -> Result<Option<BatchSnapshot>, String> {
    let Some(value) = read_json(access, &root.join("batch-checkpoint.json"), 8 * 1024 * 1024)? else { return Ok(None) };
    let mut snapshot: BatchSnapshot = serde_json::from_value(value).map_err(|e| format!("Batch checkpoint: {e}"))?;
    if snapshot.batches.len() > 1000 {
        return Err("Batch checkpoint exceeds its batch limit".into());
    }
    if matches!(snapshot.status.as_str(), "running" | "awaiting_human_proof" | "cancelling") {
        if matches!(snapshot.status.as_str(), "running" | "cancelling") {
            snapshot.cumulative_usage.has_unreported_usage = true;
        }
        snapshot.status = "interrupted".into();
        snapshot.checkpoint = 0;
        snapshot.document_uid = None;
        snapshot.document_revision = None;
    }
    for batch in &mut snapshot.batches {
        // Recovered paths are displayable evidence only inside the pinned profile.
        if batch.proof_paths.len() != batch.proof_sha256.len()
            || batch.proof_paths.iter().any(|path| access.check(Path::new(path), file_access::Access::Read).is_err())
        {
            batch.proof_paths.clear();
            batch.proof_sha256.clear();
        }
    }
    Ok(Some(snapshot))
}

impl ProductionRunner {
    pub(super) fn start_batches(
        &mut self,
        session: &mut Session,
        task: TaskRequest,
        skills: Vec<SkillSnapshot>,
        mut base: DocState,
        context: Value,
    ) -> Result<(), String> {
        let TaskTarget::NewA4 { pages } = task.target else { return Err("Batch production requires a new A4 book".into()) };
        let size = task.settings.pages_per_batch;
        if !(1..=32).contains(&size) || !(1..=1000).contains(&pages) {
            return Err("Invalid book or batch size".into());
        }
        if self.batch.is_some() || self.pending.is_some() {
            return Err("A production job is already active".into());
        }
        self.sequence = self.sequence.saturating_add(1);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis();
        let id = format!("book-{stamp}-{}-{}", std::process::id(), self.sequence);
        let snapshot = BatchSnapshot {
            job_id: id,
            total_pages: pages,
            pages_per_batch: size,
            current_batch: 1,
            total_batches: pages.div_ceil(size),
            status: "running".into(),
            ..Default::default()
        };
        let ledger = UsageLedger::new(task.settings.budget.clone())?;
        let job = BatchJob {
            task,
            skills,
            roots: file_access::current(),
            ledger: Some(ledger),
            before: UsageSummary::default(),
            snapshot,
            prefs: session.prefs.clone(),
        };
        base.selection = Default::default();
        self.launch_batch(job, base, context)
    }

    fn launch_batch(&mut self, mut job: BatchJob, base: DocState, mut context: Value) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("A provider request is already active".into());
        }
        let scope = scope(&job.snapshot)?;
        context["batch_scope"] = serde_json::to_value(scope).map_err(|e| e.to_string())?;
        context["batch_instruction"] = json!(format!(
            "Produce ONLY native pages/spreads {} through {} inclusive of this {}-page book. These target pages are blank. Do not create/delete pages, modify earlier or future pages, change global styles/swatches, or use literal object/story IDs. Every created object must specify its spread. Formatting and object references must target results created earlier in THIS batch. The user must inspect this batch proof before any following batch request.",
            scope.start,
            scope.start.saturating_add(scope.count).saturating_sub(1),
            job.snapshot.total_pages
        ));
        let settings = job.task.settings.clone();
        // Durable intent precedes the network request; recovery still cannot resume it.
        job.snapshot.status = "running".into();
        self.persist_batch(&job.snapshot)?;
        self.prepare_credentials(&[settings.text_model.clone(), settings.image_model.clone()])?;
        let keys = self.worker_keys();
        let ledger = job.ledger.take().ok_or("Batch admission ledger is unavailable")?;
        job.before = ledger.summary();
        let task = job.task.clone();
        let pins = job.skills.iter().map(|s| (s.skill_id.clone(), s.revision.clone())).collect();
        let work = worker::Work::Production {
            request: task.clone(),
            context,
            catalog: worker::batch_catalog(designcraft_engine::cmd::production::catalog()),
            skills: job.skills.clone(),
            base,
            prefs: job.prefs.clone(),
        };
        let worker = worker::spawn_with_ledger(work, keys, job.roots.clone(), ledger)?;
        let id = format!("{}-batch-{}", job.snapshot.job_id, job.snapshot.current_batch);
        let expected = job.snapshot.document_uid.zip(job.snapshot.document_revision);
        self.pending =
            Some(Pending { roots: job.roots.clone(), worker, id, started: Instant::now(), task: Some(task), expected, skills: pins, settings });
        job.snapshot.status = "running".into();
        self.snapshot.busy = true;
        self.snapshot.stage = "generating_batch".into();
        self.snapshot.progress = job.snapshot.completed_pages as f32 / job.snapshot.total_pages as f32;
        self.snapshot.message =
            format!("Generating batch {} of {}; later requests require human proof approval", job.snapshot.current_batch, job.snapshot.total_batches);
        self.snapshot.batch = Some(job.snapshot.clone());
        self.batch = Some(job);
        Ok(())
    }

    pub(super) fn approve_batch(&mut self, session: &mut Session, id: &str, checkpoint: u32) -> Result<(), String> {
        let job = self.batch.as_ref().ok_or("No live batch job is awaiting approval; recovered checkpoints cannot resume")?;
        if job.snapshot.job_id != id
            || job.snapshot.checkpoint != checkpoint
            || job.snapshot.status != "awaiting_human_proof"
            || self.pending.is_some()
        {
            return Err("Approval does not match the current unconsumed human proof checkpoint".into());
        }
        let mut job = self.batch.take().ok_or("Batch checkpoint disappeared")?;
        let validation = (|| {
            if serde_json::to_value(&session.prefs).map_err(|e| e.to_string())? != serde_json::to_value(&job.prefs).map_err(|e| e.to_string())? {
                return Err("Application preferences changed after proof generation; approval is invalid".to_string());
            }
            let current = session.doc().map_err(|e| e.to_string())?;
            if Some(current.uid) != job.snapshot.document_uid || Some(current.revision) != job.snapshot.document_revision {
                return Err(
                    "The book changed after proof generation; approval is invalid. Start a new task against the corrected document".to_string()
                );
            }
            let proof = job.snapshot.batches.last().ok_or("Batch proof is unavailable")?;
            if proof.proof_paths.is_empty() || proof.proof_paths.len() != proof.proof_sha256.len() {
                return Err("Batch proof evidence is incomplete".into());
            }
            for (path, expected) in proof.proof_paths.iter().zip(&proof.proof_sha256) {
                self.profile_access.check(Path::new(path), file_access::Access::Read)?;
                let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
                if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
                    return Err("Batch proof file changed type or exceeds its limit".into());
                }
                let bytes = file_access::confine(Some(&self.profile_access), || designcraft_engine::io::read_capped(path, 64 * 1024 * 1024))
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 64 * 1024 * 1024 {
                    return Err("Batch proof grew beyond its byte limit".into());
                }
                if hash(&bytes) != *expected {
                    return Err("Batch proof bytes changed after generation; approval is invalid".into());
                }
            }
            Ok(())
        })();
        if let Err(error) = validation {
            job.snapshot.status = "failed".into();
            self.snapshot.batch = Some(job.snapshot.clone());
            self.snapshot.stage = "failed".into();
            self.persist_batch(&job.snapshot)?;
            return Err(error);
        }
        if let Some(last) = job.snapshot.batches.last_mut() {
            last.status = "approved".into();
        }
        let approved_id = format!("{}-batch-{}", job.snapshot.job_id, job.snapshot.current_batch);
        if let Some(report) = self.snapshot.reports.iter_mut().find(|report| report.id == approved_id) {
            report.status = "approved".into();
            report.message = "Human approved the exact native/PDF/PNG proof checkpoint".into();
        }
        // Record the human decision before any new network work can begin.
        self.persist_batch(&job.snapshot)?;
        write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports)?;
        if job.snapshot.completed_pages == job.snapshot.total_pages {
            job.snapshot.status = "completed".into();
            // Final proof approval is deliberately a local transition: no worker or provider call.
            let exported = file_access::confine(job.roots.as_ref(), || self.export_finished_batch(session, &job.task, &job.snapshot.job_id));
            self.snapshot.stage = if exported.is_ok() { "completed" } else { "failed" }.into();
            if exported.is_err() {
                job.snapshot.status = "failed".into();
            }
            self.snapshot.message = "All batches approved; no additional provider request was sent".into();
            self.snapshot.batch = Some(job.snapshot.clone());
            self.persist_batch(&job.snapshot)?;
            if let Ok(paths) = &exported {
                if let Some(report) = self.snapshot.reports.iter_mut().find(|report| report.id == approved_id) {
                    report.artifacts.extend(paths.iter().cloned());
                }
                write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports)?;
            }
            return exported.map(|_| ());
        }
        let mut base = session.doc().map_err(|e| e.to_string())?.clone();
        base.selection = Default::default();
        let context = batch_context(session, &job)?;
        job.snapshot.current_batch = job.snapshot.current_batch.checked_add(1).ok_or("Batch index overflow")?;
        // Consumes this exact approval before any next request is launched.
        job.snapshot.status = "running".into();
        let prior = job.snapshot.clone();
        if let Err(error) = self.launch_batch(job, base, context) {
            if self.batch.is_none() {
                let mut failed = prior;
                failed.status = "failed".into();
                self.snapshot.batch = Some(failed.clone());
                self.persist_batch(&failed)?;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn cancel_batch(&mut self) {
        let Some(mut job) = self.batch.take() else { return };
        if let Some(pending) = &self.pending {
            pending.worker.cancel.store(true, Ordering::Relaxed);
            job.snapshot.status = "cancelling".into();
            self.snapshot.batch = Some(job.snapshot.clone());
            let _ = self.persist_batch(&job.snapshot);
            self.batch = Some(job);
        } else {
            job.snapshot.status = "cancelled".into();
            self.snapshot.batch = Some(job.snapshot.clone());
            self.snapshot.stage = "cancelled".into();
            let _ = self.persist_batch(&job.snapshot);
        }
    }

    pub(super) fn lose_batch_worker(&mut self) {
        if let Some(mut job) = self.batch.take() {
            job.snapshot.status = "failed".into();
            self.snapshot.batch = Some(job.snapshot.clone());
            let _ = self.persist_batch(&job.snapshot);
        }
    }

    pub(super) fn finish_batch(&mut self, session: &mut Session, pending: Pending, outcome: worker::Outcome, ledger: UsageLedger) {
        let Some(mut job) = self.batch.take() else {
            self.finish(
                session,
                pending,
                worker::Outcome { result: Err("Batch job authority was lost; no document was published".into()), usage: outcome.usage },
            );
            return;
        };
        self.snapshot.busy = false;
        let elapsed = pending.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let usage = ledger.delta_since(&job.before);
        job.snapshot.cumulative_usage = ledger.summary();
        job.snapshot.elapsed_ms = job.snapshot.elapsed_ms.saturating_add(elapsed);
        job.ledger = Some(ledger);
        let mut report = TaskReport {
            id: pending.id,
            status: "failed".into(),
            title: job.task.prompt.chars().take(80).collect(),
            provider: pending.settings.text_model.provider,
            model: pending.settings.text_model.model.clone(),
            settings: Some(pending.settings.clone()),
            skills: pending.skills,
            usage: usage.clone(),
            elapsed_ms: elapsed,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
        };
        let scope = scope(&job.snapshot);
        let cancelled = pending.worker.cancel.load(Ordering::Relaxed);
        let mut batch_report = BatchReport {
            batch: job.snapshot.current_batch,
            page_start: job.snapshot.completed_pages,
            page_count: scope.as_ref().map(|s| s.count).unwrap_or(0),
            status: "failed".into(),
            elapsed_ms: elapsed,
            usage,
            ..Default::default()
        };
        let roots = job.roots.clone();
        let result = if cancelled {
            Err("Batch cancelled; this result was not published".into())
        } else {
            scope.and_then(|scope| {
                file_access::confine(roots.as_ref(), || {
                    let product = outcome.result?;
                    let worker::Product::BatchPlan { plan, proofs } = product else {
                        return Err("Batch worker did not return native proof evidence".into());
                    };
                    self.publish_batch(session, &mut job, &plan, proofs, scope, &mut batch_report, &mut report)
                })
            })
        };
        match result {
            Ok(()) => {
                job.snapshot.completed_pages = job.snapshot.completed_pages.saturating_add(batch_report.page_count);
                job.snapshot.checkpoint = job.snapshot.checkpoint.saturating_add(1);
                job.snapshot.status = "awaiting_human_proof".into();
                batch_report.status = "awaiting_human_proof".into();
                report.status = "awaiting_human_proof".into();
                report.message =
                    "Batch published with native/PDF/PNG proofs. Human approval is required before any subsequent provider request".into();
            }
            Err(error) => {
                job.snapshot.status = if cancelled { "cancelled" } else { "failed" }.into();
                batch_report.status = job.snapshot.status.clone();
                report.status = job.snapshot.status.clone();
                report.message = error;
            }
        }
        job.snapshot.batches.push(batch_report);
        self.snapshot.stage = job.snapshot.status.clone();
        self.snapshot.message = report.message.clone();
        self.snapshot.progress = job.snapshot.completed_pages as f32 / job.snapshot.total_pages as f32;
        self.snapshot.batch = Some(job.snapshot.clone());
        self.snapshot.reports.push(report);
        if self.snapshot.reports.len() > MAX_REPORTS {
            self.snapshot.reports.remove(0);
        }
        let persisted = self
            .persist_batch(&job.snapshot)
            .and_then(|()| write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports));
        if let Err(error) = persisted {
            job.snapshot.status = "failed".into();
            self.snapshot.stage = "failed".into();
            self.snapshot.message = format!("Batch evidence could not be persisted; continuation is disabled: {error}");
            self.snapshot.batch = Some(job.snapshot.clone());
        }
        if job.snapshot.status == "awaiting_human_proof" {
            self.batch = Some(job);
        }
    }

    fn publish_batch(
        &self,
        session: &mut Session,
        job: &mut BatchJob,
        plan: &ProductionPlan,
        proofs: Vec<worker::ProofArtifact>,
        scope: worker::BatchScope,
        batch_report: &mut BatchReport,
        report: &mut TaskReport,
    ) -> Result<(), String> {
        ensure_publication_idle(session)?;
        if serde_json::to_value(&session.prefs).map_err(|e| e.to_string())? != serde_json::to_value(&job.prefs).map_err(|e| e.to_string())? {
            return Err("Application preferences changed during generation; proof no longer matches publication".into());
        }
        if proofs.len() < 3 || proofs.len() > 34 || proofs.iter().map(|p| p.bytes.len()).sum::<usize>() > 64 * 1024 * 1024 {
            return Err("Batch proof count or byte limit exceeded".into());
        }
        let first = job.snapshot.document_uid.is_none();
        let previous = session.active_index();
        if first {
            session.execute("file.new", &new_a4(job.snapshot.total_pages)).map_err(|e| e.to_string())?;
        }
        let result = (|| {
            let current = session.doc().map_err(|e| e.to_string())?;
            if !first && (Some(current.uid) != job.snapshot.document_uid || Some(current.revision) != job.snapshot.document_revision) {
                return Err("The book changed while its batch was generating; publication is refused".into());
            }
            let mut base = current.clone();
            base.selection = Default::default();
            worker::simulate_scoped(plan, &base, &job.prefs, job.task.settings.output_mode, &AtomicBool::new(false), Some(scope))?;
            let uid = current.uid;
            let revision = current.revision;
            let directory = self.root.join("runs").join(&job.snapshot.job_id).join(format!("batch-{}", job.snapshot.current_batch));
            self.profile_access.check(&directory, file_access::Access::Write)?;
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            for proof in proofs {
                if proof.name.contains(['/', '\\', ':'])
                    || !matches!(Path::new(&proof.name).extension().and_then(|x| x.to_str()), Some("png" | "pdf" | "designcraft"))
                {
                    return Err("Invalid batch proof artifact name".into());
                }
                let path = directory.join(proof.name);
                write_profile(&self.profile_access, &path, &proof.bytes)?;
                batch_report.proof_sha256.push(hash(&proof.bytes));
                batch_report.proof_paths.push(path.to_string_lossy().into_owned());
            }
            let trace = directory.join("trace.json");
            write_json(
                &self.profile_access,
                &trace,
                &json!({"jobId":job.snapshot.job_id,"batch":job.snapshot.current_batch,"scope":scope,"settings":job.task.settings,"skills":report.skills,"usage":report.usage,"elapsedMs":report.elapsed_ms,"plan":plan,"proofs":batch_report.proof_paths,"sha256":batch_report.proof_sha256}),
            )?;
            session.execute("production.apply",&json!({"commands":plan.commands,"outputMode":job.task.settings.output_mode,"label":format!("{} · batch {}",plan.title,job.snapshot.current_batch),"expectedDocumentUid":uid,"expectedRevision":revision,"requirePreflight":true,"minPpi":150})).map_err(|e|e.to_string())?;
            let state = session.doc().map_err(|e| e.to_string())?;
            job.snapshot.document_uid = Some(state.uid);
            job.snapshot.document_revision = Some(state.revision);
            report.title = plan.title.clone();
            batch_report.title = plan.title.clone();
            report.actions = plan.commands.len();
            report.artifacts = batch_report.proof_paths.clone();
            report.artifacts.push(trace.to_string_lossy().into_owned());
            Ok(())
        })();
        if first && result.is_err() {
            session.execute("file.close", &json!({})).map_err(|e| e.to_string())?;
            if let Some(index) = previous {
                session.execute("file.activate", &json!({"index":index})).map_err(|e| e.to_string())?;
            }
        }
        result
    }
    fn persist_batch(&self, snapshot: &BatchSnapshot) -> Result<(), String> {
        write_json(&self.profile_access, &self.root.join("batch-checkpoint.json"), snapshot)
    }
    fn export_finished_batch(&self, session: &mut Session, task: &TaskRequest, job_id: &str) -> Result<Vec<String>, String> {
        let Some(directory) = &task.export_directory else { return Ok(vec![]) };
        file_access::check_write(Path::new(directory))?;
        std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        let source = Path::new(directory).join(format!("reviewed-{job_id}.designcraft"));
        let pdf = Path::new(directory).join(format!("reviewed-{job_id}.pdf"));
        session.execute("file.saveAs", &json!({"path":source})).map_err(|e| e.to_string())?;
        session.execute("file.exportPdf", &json!({"path":pdf,"spreads":false,"bleed":false,"marks":false})).map_err(|e| e.to_string())?;
        Ok(vec![source.to_string_lossy().into_owned(), pdf.to_string_lossy().into_owned()])
    }
}
fn scope(snapshot: &BatchSnapshot) -> Result<worker::BatchScope, String> {
    let count = snapshot.total_pages.checked_sub(snapshot.completed_pages).ok_or("Invalid completed page count")?.min(snapshot.pages_per_batch);
    if count == 0 || snapshot.total_pages == 0 {
        return Err("No unproduced batch pages remain".into());
    }
    Ok(worker::BatchScope { start: snapshot.completed_pages, count })
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Keep book-wide page coverage and recent approved content; never repeatedly resend page one.
fn batch_context(session: &Session, job: &BatchJob) -> Result<Value, String> {
    let state = session.doc().map_err(|e| e.to_string())?;
    let completed = usize::try_from(job.snapshot.completed_pages).map_err(|_| "Invalid approved page count")?;
    let last = job.snapshot.batches.last().ok_or("Approved batch context is unavailable")?;
    let start = usize::try_from(last.page_start).map_err(|_| "Invalid preceding batch page")?;
    let count = usize::try_from(last.page_count).map_err(|_| "Invalid preceding batch size")?;
    let end = start.checked_add(count).ok_or("Preceding batch range overflow")?;
    let per_page = (12_000 / completed.max(1)).clamp(8, 160);
    let mut briefs = Vec::new();
    for page in 0..completed.min(1000) {
        let spread = state.doc.spreads.get(page).ok_or("Approved book page disappeared")?;
        let heading = spread
            .items
            .iter()
            .find_map(|item| item.text_frame().and_then(|frame| state.doc.stories.get(&frame.story)).map(|story| story.text.as_str()))
            .or_else(|| spread.items.iter().find(|item| !item.label.is_empty()).map(|item| item.label.as_str()))
            .unwrap_or("");
        briefs.push(json!([page, limited_text(heading, per_page)]));
    }
    let mut objects = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for (page, spread) in state.doc.spreads.iter().enumerate().take(end).skip(start) {
        for item in &spread.items {
            let text = item.text_frame().and_then(|frame| state.doc.stories.get(&frame.story)).map(|story| limited_text(&story.text, 160));
            let bounds = item.bounds();
            let object = json!({"spread":page,"label":limited_text(&item.label,48),"bounds":[bounds.x0,bounds.y0,bounds.x1,bounds.y1],"text":text});
            let length = serde_json::to_vec(&object).map_err(|e| e.to_string())?.len();
            if bytes.saturating_add(length) > 10_000 || objects.len() >= 64 {
                truncated = true;
                break;
            }
            bytes = bytes.saturating_add(length);
            objects.push(object);
        }
        if truncated {
            break;
        }
    }
    let ranges: Vec<_> = job
        .snapshot
        .batches
        .iter()
        .rev()
        .take(64)
        .rev()
        .map(|batch| json!([batch.batch, batch.page_start, batch.page_count, limited_text(&batch.title, 80)]))
        .collect();
    let title = limited_text(&job.task.prompt, 256);
    let context = json!({"documentUid":state.uid,"revision":state.revision,"pageCount":state.doc.page_count(),"pageWidth":state.doc.settings.page_width,"pageHeight":state.doc.settings.page_height,"units":"points; each native spread is one A4 page","approvedPageCount":job.snapshot.completed_pages,"approvedPageBriefs":briefs,"recentApprovedBatchRanges":ranges,"earlierBatchRangesOmitted":job.snapshot.batches.len()>64,"recentApprovedObjects":objects,"recentObjectsTruncated":truncated,"bookRequestSummary":title,"continuityInstruction":"Continue the original book after the approved pages. Page briefs are bounded observations of existing editable content, not a new curriculum or a claim of educational certification. Do not repeat the beginning or regenerate approved pages."});
    if serde_json::to_vec(&context).map_err(|e| e.to_string())?.len() > 48 * 1024 {
        return Err("Approved book continuity context exceeds its bounded size".into());
    }
    Ok(context)
}
fn limited_text(text: &str, max: usize) -> String {
    let mut result = String::new();
    for character in text.chars() {
        if result.len().saturating_add(character.len_utf8()) > max {
            break;
        }
        result.push(character);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_ai_providers::Usage;
    use designcraft_ai_types::ModelSelection;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    fn task(endpoint: String, max_requests: u32) -> TaskRequest {
        TaskRequest {
            prompt: "Make one editable native rectangle per page, with exact 15 mm margins.".into(),
            settings: AiSettings {
                text_model: ModelSelection { provider: ProviderKind::Ollama, model: "batch-regression".into(), endpoint: Some(endpoint) },
                pages_per_batch: 1,
                budget: BudgetSpec { token_limit: 500_000, max_output_tokens: 512, max_requests, repair_attempts: 0, ..Default::default() },
                ..Default::default()
            },
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 2 },
            export_directory: None,
        }
    }
    fn wait(runner: &mut ProductionRunner, session: &mut Session) -> AiSnapshot {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = runner.handle(session, AiRequest::Poll);
            if !snapshot.busy {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "Batch worker timed out: {}", snapshot.message);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn waiting(runner: &mut ProductionRunner, session: &mut Session, endpoint: String) -> (String, u32) {
        session.execute("file.new", &new_a4(2)).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100],"content":"unassigned"})).unwrap();
        let task = task(endpoint, 1);
        let mut ledger = UsageLedger::new(task.settings.budget.clone()).unwrap();
        let reservation = ledger.reserve(10, 0, ProviderKind::Ollama, false, None).unwrap();
        ledger.record(reservation, Some(&Usage { input_tokens: Some(7), output_tokens: Some(3), ..Default::default() }));
        let path = runner.root.join("fixture-proof.png");
        let bytes = b"synthetic internal checkpoint evidence";
        write_profile(&runner.profile_access, &path, bytes).unwrap();
        let current = session.doc().unwrap();
        let snapshot = BatchSnapshot {
            job_id: "fixture-book".into(),
            total_pages: 2,
            pages_per_batch: 1,
            current_batch: 1,
            total_batches: 2,
            completed_pages: 1,
            status: "awaiting_human_proof".into(),
            checkpoint: 1,
            document_uid: Some(current.uid),
            document_revision: Some(current.revision),
            cumulative_usage: ledger.summary(),
            batches: vec![BatchReport {
                batch: 1,
                page_start: 0,
                page_count: 1,
                status: "awaiting_human_proof".into(),
                proof_paths: vec![path.to_string_lossy().into_owned()],
                proof_sha256: vec![hash(bytes)],
                ..Default::default()
            }],
            ..Default::default()
        };
        runner.persist_batch(&snapshot).unwrap();
        runner.snapshot.batch = Some(snapshot.clone());
        runner.batch = Some(BatchJob {
            task,
            skills: vec![],
            roots: None,
            ledger: Some(ledger),
            before: UsageSummary::default(),
            snapshot,
            prefs: session.prefs.clone(),
        });
        ("fixture-book".into(), 1)
    }

    #[test]
    fn explicit_approval_is_the_only_path_between_real_http_batches() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicU32::new(0));
        let server_calls = calls.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                };
                socket.set_nonblocking(false).map_err(|e| e.to_string())?;
                socket.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
                socket.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
                let mut bytes = vec![];
                let mut chunk = [0u8; 4096];
                loop {
                    if bytes.len() > 1024 * 1024 || Instant::now() >= deadline {
                        return Err("Mock request exceeded bounds".into());
                    }
                    let count = socket.read(&mut chunk).map_err(|e| e.to_string())?;
                    if count == 0 {
                        return Err("Mock request ended early".into());
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some((headers, body)) = String::from_utf8_lossy(&bytes).split_once("\r\n\r\n") {
                        let length = headers
                            .lines()
                            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|s| s.trim().parse::<usize>().ok()))
                            .ok_or("Missing mock Content-Length")?;
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                let page = server_calls.fetch_add(1, Ordering::Relaxed);
                if page > 1 {
                    return Err("Unexpected unapproved provider request".into());
                }
                let plan = json!({"title":"Reviewed native book","commands":[{"command":"frame.create","params":{"spread":page,"rect":[50,60,200,180],"content":"unassigned"}},{"command":"object.label","params":{"ids":["$0.id"],"label":format!("page-{}",page+1)}}],"images":[]});
                let reply =
                    json!({"done":true,"message":{"content":plan.to_string()},"prompt_eval_count":120,"eval_count":30,"prompt_eval_cached_count":10})
                        .to_string();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let started = runner.handle(&mut session, AiRequest::Start(task(endpoint, 3)));
        assert!(started.busy, "{}", started.message);
        let first = wait(&mut runner, &mut session);
        assert_eq!(first.stage, "awaiting_human_proof", "{}", first.message);
        let checkpoint = first.batch.as_ref().unwrap();
        assert_eq!(checkpoint.completed_pages, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(checkpoint.batches[0].proof_paths.len(), 3);
        let approved_page = session.doc().unwrap().doc.spreads[0].clone();
        for _ in 0..8 {
            runner.handle(&mut session, AiRequest::Poll);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1, "Polling must never prefetch the next batch");
        let wrong = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint + 1 });
        assert!(!wrong.busy);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let mut reopened = ProductionRunner::open(dir.path()).unwrap();
        assert_eq!(reopened.snapshot.batch.as_ref().unwrap().status, "interrupted");
        assert!(reopened.pending.is_none());
        assert!(reopened.batch.is_none());
        let denied = reopened.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint });
        assert!(!denied.busy);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(reopened);
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint });
        assert!(approved.busy, "{}", approved.message);
        let second = wait(&mut runner, &mut session);
        assert_eq!(second.stage, "awaiting_human_proof", "{}", second.message);
        let final_checkpoint = second.batch.as_ref().unwrap();
        assert_eq!(final_checkpoint.completed_pages, 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(final_checkpoint.cumulative_usage.requests, 2);
        assert_eq!(final_checkpoint.cumulative_usage.input_tokens, 240);
        assert!(final_checkpoint.batches.iter().all(|batch| batch.usage.requests == 1));
        assert_eq!(session.doc().unwrap().doc.spreads[0], approved_page);
        assert_eq!(session.doc().unwrap().doc.spreads[1].items[0].label, "page-2");
        assert_eq!(session.doc().unwrap().history.undo.len(), 2);
        let completed =
            runner.handle(&mut session, AiRequest::ApproveBatch { job_id: final_checkpoint.job_id.clone(), checkpoint: final_checkpoint.checkpoint });
        assert_eq!(completed.batch.as_ref().unwrap().status, "completed");
        assert!(runner.pending.is_none());
        assert!(runner.batch.is_none());
        runner.handle(&mut session, AiRequest::ApproveBatch { job_id: final_checkpoint.job_id.clone(), checkpoint: final_checkpoint.checkpoint });
        for _ in 0..4 {
            runner.handle(&mut session, AiRequest::Poll);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2, "Final or repeated approval cannot issue another request");
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn cumulative_request_limit_survives_a_human_checkpoint() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let (job_id, checkpoint) = waiting(&mut runner, &mut session, format!("http://{}/", listener.local_addr().unwrap()));
        let before = session.doc().unwrap().doc.clone();
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
        assert!(approved.busy, "{}", approved.message);
        let result = wait(&mut runner, &mut session);
        assert_eq!(result.batch.as_ref().unwrap().status, "failed");
        assert!(result.message.contains("request limit"), "{}", result.message);
        assert_eq!(result.batch.as_ref().unwrap().cumulative_usage.requests, 1);
        assert_eq!(result.batch.as_ref().unwrap().batches.last().unwrap().usage.requests, 0);
        assert!(listener.accept().is_err(), "Exhausted cumulative budget must stop before HTTP");
        assert!(Arc::ptr_eq(&before, &session.doc().unwrap().doc));
    }

    #[test]
    fn stale_or_modified_proof_and_cancelled_checkpoints_cannot_continue() {
        for change in ["document", "proof", "prefs", "cancel"] {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            match change {
                "document" => {
                    session.execute("frame.create", &json!({"spread":0,"rect":[200,60,300,150],"content":"unassigned"})).unwrap();
                }
                "proof" => {
                    std::fs::write(&runner.snapshot.batch.as_ref().unwrap().batches[0].proof_paths[0], b"changed proof").unwrap();
                }
                "prefs" => {
                    session.prefs.typographers_quotes = !session.prefs.typographers_quotes;
                }
                _ => {
                    runner.handle(&mut session, AiRequest::Cancel);
                }
            }
            let before = session.doc().unwrap().doc.clone();
            let rejected = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
            assert!(!rejected.busy);
            assert!(runner.pending.is_none());
            assert!(runner.batch.is_none());
            assert!(Arc::ptr_eq(&before, &session.doc().unwrap().doc));
            assert_eq!(rejected.batch.as_ref().unwrap().cumulative_usage.requests, 1);
        }
    }

    #[test]
    fn smart_quote_preferences_changed_during_generation_cannot_publish_different_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let mut job = runner.batch.take().unwrap();
        let before = session.doc().unwrap().clone();
        session.prefs.typographers_quotes = !session.prefs.typographers_quotes;
        assert_eq!(session.doc().unwrap().revision, before.revision, "Preferences are independent of document revisions");
        let plan = ProductionPlan {
            title: "Smart quotes".into(),
            commands: vec![
                EngineAction { command: "frame.create".into(), params: json!({"spread":1,"rect":[50,50,300,180],"content":"text"}) },
                EngineAction { command: "text.insert".into(), params: json!({"text":"\"Proof text\""}) },
            ],
            images: vec![],
        };
        let mut report = TaskReport {
            id: "preference-proof".into(),
            status: String::new(),
            title: String::new(),
            provider: ProviderKind::Ollama,
            model: "mock".into(),
            settings: Some(job.task.settings.clone()),
            skills: vec![],
            usage: Default::default(),
            elapsed_ms: 0,
            actions: 0,
            issues: vec![],
            artifacts: vec![],
            message: String::new(),
        };
        let proofs = vec![
            worker::ProofArtifact { name: "document.designcraft".into(), bytes: vec![1] },
            worker::ProofArtifact { name: "proof.pdf".into(), bytes: vec![2] },
            worker::ProofArtifact { name: "page-2.png".into(), bytes: vec![3] },
        ];
        let error = runner
            .publish_batch(&mut session, &mut job, &plan, proofs, worker::BatchScope { start: 1, count: 1 }, &mut BatchReport::default(), &mut report)
            .unwrap_err();
        assert!(error.contains("preferences changed"), "{error}");
        assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert!(report.artifacts.is_empty());
        assert!(!runner.root.join("runs").exists(), "A mismatched proof cannot be persisted or published");
    }

    #[test]
    fn first_batch_publication_preserves_old_document_preview_before_new_document() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let mut job = runner.batch.take().unwrap();
        job.snapshot.document_uid = None;
        job.snapshot.document_revision = None;
        job.snapshot.completed_pages = 0;
        let unfilled = session.doc().unwrap().doc.clone();
        session.execute("frame.create", &json!({"spread":0,"rect":[200,60,300,180],"content":"unassigned"})).unwrap();
        let state = session.doc_mut().unwrap();
        state.preview_stash = Some(unfilled.clone());
        state.preview_record = Some(1);
        let before = session.doc().unwrap().clone();
        let journal = session.journal.clone();
        let active = session.active_index();
        let count = session.documents().len();
        let plan = ProductionPlan {
            title: "Refused first batch".into(),
            commands: vec![EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":0,"rect":[50,50,200,180],"content":"unassigned"}),
            }],
            images: vec![],
        };
        let proofs = vec![
            worker::ProofArtifact { name: "document.designcraft".into(), bytes: vec![1] },
            worker::ProofArtifact { name: "proof.pdf".into(), bytes: vec![2] },
            worker::ProofArtifact { name: "page-1.png".into(), bytes: vec![3] },
        ];
        let mut report = TaskReport {
            id: "preview-publication".into(),
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
        let error = runner
            .publish_batch(&mut session, &mut job, &plan, proofs, worker::BatchScope { start: 0, count: 1 }, &mut BatchReport::default(), &mut report)
            .unwrap_err();
        assert!(error.contains("preview"), "{error}");
        let after = session.doc().unwrap();
        assert_eq!(session.documents().len(), count);
        assert_eq!(session.active_index(), active);
        assert_eq!((after.uid, after.revision), (before.uid, before.revision));
        assert_eq!(after.selection, before.selection);
        assert!(Arc::ptr_eq(&before.doc, &after.doc));
        assert!(Arc::ptr_eq(after.preview_stash.as_ref().unwrap(), &unfilled));
        assert_eq!(after.preview_record, Some(1));
        assert_eq!(after.history.undo.len(), before.history.undo.len());
        assert_eq!(after.history.redo.len(), before.history.redo.len());
        assert!(
            after
                .history
                .undo
                .iter()
                .zip(&before.history.undo)
                .all(|(a, b)| a.label == b.label && a.selection == b.selection && Arc::ptr_eq(&a.doc, &b.doc))
        );
        assert_eq!(session.journal, journal);
        assert!(report.artifacts.is_empty());
        assert!(!runner.root.join("runs").exists(), "First-batch refusal must not write traces or proofs");
    }

    #[test]
    fn later_batch_context_keeps_page_coverage_and_recent_approved_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        session
            .execute("frame.create", &json!({"spread":0,"rect":[50,50,300,180],"content":"text","text":"Earlier approved chapter","caret":false}))
            .unwrap();
        session
            .execute(
                "frame.create",
                &json!({"spread":1,"rect":[50,50,300,180],"content":"text","text":"Recent approved topic: roots and leaves","caret":false}),
            )
            .unwrap();
        let mut job = runner.batch.take().unwrap();
        job.snapshot.completed_pages = 2;
        job.snapshot.batches.push(BatchReport {
            batch: 2,
            title: "Plants".into(),
            page_start: 1,
            page_count: 1,
            status: "approved".into(),
            ..Default::default()
        });
        let context = batch_context(&session, &job).unwrap();
        assert_eq!(context["approvedPageCount"], 2);
        assert_eq!(context["approvedPageBriefs"].as_array().unwrap().len(), 2);
        let recent = context["recentApprovedObjects"].to_string();
        assert!(recent.contains("roots and leaves"));
        assert!(!recent.contains("Earlier approved chapter"));
        assert!(context["recentApprovedBatchRanges"].to_string().contains("Plants"));
        assert!(serde_json::to_vec(&context).unwrap().len() < 48 * 1024);
        assert!(limited_text("حروف عربية", 7).len() <= 7);
    }
}
