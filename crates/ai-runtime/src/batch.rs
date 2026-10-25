//! Human-gated book production. Persisted snapshots are evidence, never resumable authority.
use super::*;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_engine::{DocState, Prefs};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::AtomicBool};

const MAX_PROOF_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PAGE_PROOF_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROOF_TOTAL_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct BatchJob {
    task: TaskRequest,
    skills: Vec<SkillSnapshot>,
    roots: Option<Arc<file_access::AutomationRoots>>,
    ledger: Option<UsageLedger>,
    before: UsageSummary,
    pub(super) snapshot: BatchSnapshot,
    prefs: Prefs,
    // Font reload replaces the composition cache without changing the document
    // allocation. Proof authority therefore pins it independently of doc edits.
    publication_cache: Arc<designcraft_engine::compose::Cache>,
    // Keeps the original allocation alive: selection/view revisions alone are
    // not document edits, and an address cannot be recycled while pinned here.
    publication_base: Option<DocState>,
    retry_sequence: u32,
}
impl BatchJob {
    pub(super) fn snapshot_elapsed(&self) -> u64 {
        self.snapshot.elapsed_ms
    }
}

pub(super) fn recover_snapshot(access: &std::sync::Arc<file_access::AutomationRoots>, root: &Path) -> Result<Option<BatchSnapshot>, String> {
    let Some(value) = read_json(access, &root.join("batch-checkpoint.json"), 8 * 1024 * 1024)? else { return Ok(None) };
    let mut snapshot: BatchSnapshot = serde_json::from_value(value).map_err(|e| format!("Batch checkpoint: {e}"))?;
    snapshot.retryable = false;
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

fn page_ranges_summary(spreads: &[Arc<designcraft_engine::doc::Spread>], filled: bool) -> String {
    let mut ranges: Vec<[usize; 2]> = Vec::new();
    for (index, spread) in spreads.iter().enumerate() {
        if spread.items.is_empty() != filled {
            let page = index.saturating_add(1);
            if let Some(last) = ranges.last_mut().filter(|last| last[1].saturating_add(1) == page) {
                last[1] = page;
            } else {
                ranges.push([page, page]);
            }
        }
    }
    if ranges.is_empty() {
        return "none".into();
    }
    ranges.into_iter().map(|[start, end]| if start == end { start.to_string() } else { format!("{start}–{end}") }).collect::<Vec<_>>().join(", ")
}

/// A continuation starts only from a deliberately selected current book. Never
/// infer permission or a page offset from stale, editable checkpoint metadata.
pub(super) fn continuation_progress(
    base: &DocState,
    profile: Option<&designcraft_ai_types::DesignProfile>,
    brief: &str,
) -> Result<(u32, u32), String> {
    if let Some(profile) = profile {
        profile.validate()?;
    }
    // A style supplies an identity, not permission to resize the opened book.
    // Only an explicit trim in the user's brief can require another size.
    let requested_trim = crate::print_contract::PrintContract::from_prompt(brief)?.and_then(|contract| contract.page_mm);
    let pages = u32::try_from(base.doc.spreads.len()).map_err(|_| "Invalid existing book page count")?;
    if !(1..=1000).contains(&pages)
        || base.doc.spreads.iter().any(|spread| {
            spread.pages.len() != 1
                || spread.pages.iter().any(|page| {
                    !page.width.is_finite()
                        || !page.height.is_finite()
                        || page.width <= 0.0
                        || page.height <= 0.0
                        || page.width > 15552.0
                        || page.height > 15552.0
                        || requested_trim
                            .is_some_and(|[w, h]| (page.width - w * 72.0 / 25.4).abs() > 0.1 || (page.height - h * 72.0 / 25.4).abs() > 0.1)
                })
        })
    {
        return Err(
            "Continue book requires 1–1000 valid pages matching any explicit Trim Size in your brief; select another book or correct that size"
                .into(),
        );
    }
    if let Some(contract) = crate::script_contract::parse(brief)?
        && u64::try_from(contract.pages.len()).ok() != Some(u64::from(pages))
    {
        return Err(format!(
            "Explicit page-script count ({}) must match the selected book page count ({pages}); choose a matching book or start a new document",
            contract.pages.len()
        ));
    }
    let start = base.doc.spreads.iter().position(|spread| spread.items.is_empty()).ok_or("No blank pages remain in this book")?;
    if start == 0 || base.doc.spreads.iter().skip(start).any(|spread| !spread.items.is_empty()) {
        let completed = page_ranges_summary(&base.doc.spreads, true);
        let blank = page_ranges_summary(&base.doc.spreads, false);
        return Err(format!(
            "Continue book requires completed opening pages followed only by blank pages; existing content will not be overwritten (completed pages: {completed}; blank pages: {blank})"
        ));
    }
    Ok((pages, u32::try_from(start).map_err(|_| "Invalid continuation page")?))
}

impl ProductionRunner {
    pub(super) fn refresh_unchanged_proof(&mut self, session: &Session) -> Result<(), String> {
        let Some(job) = self.batch.as_ref().filter(|job| job.snapshot.status == "awaiting_human_proof" && self.pending.is_none()) else {
            return Ok(());
        };
        let Some(target_uid) = job.snapshot.document_uid else { return Ok(()) };
        let Some(current) = session.documents().iter().find(|doc| doc.uid == target_uid) else { return Ok(()) };
        if Some(current.revision) == job.snapshot.document_revision
            || current.interaction.is_some()
            || current.preview_stash.is_some()
            || !job.publication_base.as_ref().is_some_and(|published| {
                Arc::ptr_eq(&published.doc, &current.doc)
                    && published.active_layer == current.active_layer
                    && published.editing_parents == current.editing_parents
            })
            || !Arc::ptr_eq(&job.publication_cache, &session.cache)
            || serde_json::to_value(&job.prefs).map_err(|e| e.to_string())? != serde_json::to_value(&session.prefs).map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        // Native selection/navigation may bump the session revision. Rebind only
        // the exact still-pinned document allocation; changed content never passes.
        let mut snapshot = job.snapshot.clone();
        snapshot.document_revision = Some(current.revision);
        self.persist_batch(&snapshot)?;
        if let Some(job) = &mut self.batch {
            job.snapshot = snapshot.clone();
        }
        self.snapshot.batch = Some(snapshot);
        Ok(())
    }

    pub(super) fn start_batches(
        &mut self,
        session: &mut Session,
        task: TaskRequest,
        skills: Vec<SkillSnapshot>,
        mut base: DocState,
        mut context: Value,
    ) -> Result<(), String> {
        let (pages, completed, existing) = match task.target {
            TaskTarget::NewA4 { pages } => (pages, 0, None),
            TaskTarget::ContinueBook => {
                let (pages, completed) = continuation_progress(&base, skills.iter().find_map(|skill| skill.design_profile.as_ref()), &task.prompt)?;
                (pages, completed, Some((base.uid, base.revision)))
            }
            TaskTarget::CurrentDocument => return Err("Choose new A4 book or explicitly continue remaining blank pages".into()),
        };
        let size = task.settings.pages_per_batch;
        if !(1..=1000).contains(&size) || !(1..=1000).contains(&pages) {
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
            total_batches: pages.saturating_sub(completed).div_ceil(size),
            completed_pages: completed,
            document_uid: existing.map(|(uid, _)| uid),
            document_revision: existing.map(|(_, revision)| revision),
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
            publication_cache: session.cache.clone(),
            publication_base: if existing.is_some() { Some(session.doc().map_err(|e| e.to_string())?.clone()) } else { None },
            retry_sequence: 0,
        };
        if completed > 0 {
            let mut projected = Session::new();
            projected.prefs = session.prefs.clone();
            projected.add_document(base.clone());
            context = batch_context(&projected, &job, &self.root)?;
        }
        if let Some(mut draft) = self.snapshot.production_draft.clone() {
            draft.id = job.snapshot.job_id.clone();
            self.persist_production_draft(&draft)?;
            self.snapshot.production_draft = Some(draft);
        }
        base.selection = Default::default();
        self.launch_batch(job, base, context)
    }

    fn launch_batch(&mut self, job: BatchJob, base: DocState, context: Value) -> Result<(), String> {
        let backup = job.clone();
        if let Err(error) = self.launch_batch_inner(job, base, context) {
            let mut failed = self.batch.take().unwrap_or(backup);
            failed.snapshot.status = "failed".into();
            if !failed.snapshot.batches.last().is_some_and(|batch| batch.page_start == failed.snapshot.completed_pages && batch.status == "failed") {
                failed.snapshot.batches.push(BatchReport {
                    batch: failed.snapshot.current_batch,
                    page_start: failed.snapshot.completed_pages,
                    page_count: scope(&failed.snapshot).map(|scope| scope.count).unwrap_or(0),
                    status: "failed".into(),
                    ..Default::default()
                });
            }
            failed.snapshot.retryable = retry_candidate(&failed);
            self.snapshot.batch = Some(failed.snapshot.clone());
            let persisted = self.persist_batch(&failed.snapshot);
            self.batch = Some(failed);
            return Err(match persisted {
                Ok(()) => error,
                Err(write_error) => format!("{error}; could not persist failed checkpoint: {write_error}"),
            });
        }
        Ok(())
    }

    fn launch_batch_inner(&mut self, mut job: BatchJob, base: DocState, mut context: Value) -> Result<(), String> {
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
        job.snapshot.retryable = false;
        self.persist_batch(&job.snapshot)?;
        let mut selections = vec![settings.text_model.clone()];
        if settings.output_mode == designcraft_ai_types::OutputMode::NativeWithImages {
            selections.push(settings.image_model.clone());
        }
        self.prepare_credentials(&selections)?;
        let keys = self.worker_keys();
        let ledger = job.ledger.as_ref().ok_or("Batch admission ledger is unavailable")?.clone();
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
        let worker = match worker::spawn_with_ledger(work, keys, job.roots.clone(), ledger) {
            Ok(worker) => worker,
            Err(error) => {
                job.snapshot.status = "failed".into();
                job.snapshot.retryable = retry_candidate(&job);
                self.snapshot.batch = Some(job.snapshot.clone());
                self.batch = Some(job);
                return Err(error);
            }
        };
        job.ledger = None;
        let id = batch_report_id(&job);
        let expected = job.snapshot.document_uid.zip(job.snapshot.document_revision);
        self.pending = Some(Pending {
            roots: job.roots.clone(),
            worker,
            id,
            started: Instant::now(),
            task: Some(task),
            expected,
            expected_prefs: None,
            skills: pins,
            settings,
        });
        job.snapshot.status = "running".into();
        self.snapshot.busy = true;
        self.snapshot.current_usage = job.before.clone();
        self.snapshot.current_issues.clear();
        self.snapshot.stage = "generating_batch".into();
        self.snapshot.progress = job.snapshot.completed_pages as f32 / job.snapshot.total_pages as f32;
        self.snapshot.message =
            format!("Generating batch {} of {}; later requests require human proof approval", job.snapshot.current_batch, job.snapshot.total_batches);
        self.snapshot.batch = Some(job.snapshot.clone());
        self.batch = Some(job);
        Ok(())
    }

    pub(super) fn retry_batch(&mut self, session: &mut Session, id: &str) -> Result<(), String> {
        let job = self.batch.as_ref().ok_or("This task is no longer live; select its book and explicitly Continue with a new task")?;
        if job.snapshot.job_id != id || !retry_candidate(job) || self.pending.is_some() {
            return Err("Only an unpublished failed group with its original admission ledger can be retried".into());
        }
        ensure_publication_idle(session)?;
        let profile = job.skills.iter().find_map(|skill| skill.design_profile.as_ref());
        let (base, mut context) = if let Some(uid) = job.snapshot.document_uid {
            let current = session.doc().map_err(|e| e.to_string())?;
            if current.uid != uid {
                return Err("Select this failed task's book before Retry; another open document will not be modified".into());
            }
            let (base, context) = task_document_context(session, &TaskTarget::ContinueBook, profile, &job.task.prompt, &job.task.settings)?;
            let (total, completed) = continuation_progress(&base, profile, &job.task.prompt)?;
            if total != job.snapshot.total_pages || completed != job.snapshot.completed_pages {
                return Err("The target pages or book page count changed; prepare a fresh continuation instead of overwriting content".into());
            }
            (base, context)
        } else {
            if !matches!(job.task.target, TaskTarget::NewA4 { .. }) || job.snapshot.completed_pages != 0 {
                return Err("The failed task has no matching book authority".into());
            }
            task_document_context(session, &job.task.target, profile, &job.task.prompt, &job.task.settings)?
        };
        if job.snapshot.completed_pages > 0 {
            let mut projected = Session::new();
            projected.prefs = session.prefs.clone();
            projected.add_document(base.clone());
            context = batch_context(&projected, job, &self.root)?;
        } else {
            enrich_production_context(&mut context, &self.root, &base, &job.task)?;
        }
        let mut job = self.batch.take().ok_or("The failed task disappeared")?;
        job.retry_sequence = job.retry_sequence.checked_add(1).ok_or("Too many retries for this task")?;
        if job.snapshot.document_uid.is_some() {
            let current = session.doc().map_err(|e| e.to_string())?;
            job.snapshot.document_revision = Some(current.revision);
            job.publication_base = Some(current.clone());
        }
        job.publication_cache = session.cache.clone();
        job.prefs = session.prefs.clone();
        // Failed attempts stay in history; proof lookup must see only the new attempt.
        if job.snapshot.batches.last().is_some_and(|batch| batch.status == "failed" && batch.page_start == job.snapshot.completed_pages) {
            job.snapshot.batches.pop();
        }
        self.launch_batch(job, base, context)
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
        self.refresh_unchanged_proof(session)?;
        ensure_publication_idle(session)?;
        if let Some(target_uid) = self.batch.as_ref().and_then(|job| job.snapshot.document_uid)
            && let Some(target_doc) = session.documents().iter().find(|doc| doc.uid == target_uid)
            && (target_doc.interaction.is_some() || target_doc.preview_stash.is_some())
        {
            return Err("Finish the current interaction, IME composition or preview before publishing AI production".to_string());
        }
        let mut job = self.batch.take().ok_or("Batch checkpoint disappeared")?;
        let validation: Result<usize, String> = (|| {
            if !Arc::ptr_eq(&job.publication_cache, &session.cache) {
                return Err("Font cache changed after proof generation; approval is invalid".to_string());
            }
            if serde_json::to_value(&session.prefs).map_err(|e| e.to_string())? != serde_json::to_value(&job.prefs).map_err(|e| e.to_string())? {
                return Err("Application preferences changed after proof generation; approval is invalid".to_string());
            }
            let Some(target_index) = session.documents().iter().position(|doc| Some(doc.uid) == job.snapshot.document_uid) else {
                return Err(
                    "The book changed after proof generation; approval is invalid. Start a new task against the corrected document".to_string()
                );
            };
            let current = session.documents().get(target_index).ok_or("Approved book disappeared")?;
            if Some(current.revision) != job.snapshot.document_revision {
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
                if !metadata.is_file() || metadata.len() > MAX_PROOF_FILE_BYTES as u64 {
                    return Err("Batch proof file changed type or exceeds its limit".into());
                }
                let bytes =
                    file_access::confine(Some(&self.profile_access), || designcraft_engine::io::read_capped(path, MAX_PROOF_FILE_BYTES as u64))
                        .map_err(|e| e.to_string())?;
                if bytes.len() > MAX_PROOF_FILE_BYTES {
                    return Err("Batch proof grew beyond its byte limit".into());
                }
                if hash(&bytes) != *expected {
                    return Err("Batch proof bytes changed after generation; approval is invalid".into());
                }
            }
            Ok(target_index)
        })();
        let target_index = match validation {
            Ok(idx) => idx,
            Err(error) => {
                job.snapshot.status = "failed".into();
                self.snapshot.batch = Some(job.snapshot.clone());
                self.snapshot.stage = "failed".into();
                let _ = self.persist_batch(&job.snapshot);
                return Err(error);
            }
        };
        let next_launch = if job.snapshot.completed_pages < job.snapshot.total_pages {
            let prep: Result<(DocState, Value, u32), String> = (|| {
                let target_doc = session.documents().get(target_index).ok_or("Approved book disappeared")?;
                let mut base = crate::facing_pages::canonicalize(target_doc, &session.prefs)?;
                base.selection = Default::default();
                let mut projected = Session::new();
                projected.prefs = session.prefs.clone();
                projected.add_document(base.clone());
                let mut next_job_view = job.clone();
                if let Some(last) = next_job_view.snapshot.batches.last_mut() {
                    last.status = "approved".into();
                }
                let context = batch_context(&projected, &next_job_view, &self.root)?;
                let next_batch = job.snapshot.current_batch.checked_add(1).ok_or("Batch index overflow")?;
                Ok((base, context, next_batch))
            })();
            match prep {
                Ok(ready) => Some(ready),
                Err(error) => {
                    job.snapshot.status = "failed".into();
                    self.snapshot.batch = Some(job.snapshot.clone());
                    self.snapshot.stage = "failed".into();
                    let _ = self.persist_batch(&job.snapshot);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let unapproved_job = job.clone();
        let unapproved_reports = self.snapshot.reports.clone();
        if let Some(last) = job.snapshot.batches.last_mut() {
            last.status = "approved".into();
        }
        let approved_id = batch_report_id(&job);
        if let Some(report) = self.snapshot.reports.iter_mut().rev().find(|report| report.id == approved_id) {
            report.status = "approved".into();
            report.message = "Human approved the exact native/PDF/PNG proof checkpoint".into();
        }
        // Record the human decision before mutating active tab or launching network work.
        if let Err(io_err) =
            self.persist_batch(&job.snapshot).and_then(|()| write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports))
        {
            self.snapshot.reports = unapproved_reports;
            self.snapshot.batch = Some(unapproved_job.snapshot.clone());
            self.batch = Some(unapproved_job);
            return Err(io_err);
        }
        if session.active_index() != Some(target_index) {
            session.set_active(target_index);
        }
        let Some((base, context, next_batch)) = next_launch else {
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
        };
        job.snapshot.current_batch = next_batch;
        // Consumes this exact approval before any next request is launched.
        job.snapshot.status = "running".into();
        self.launch_batch(job, base, context)
    }

    pub(super) fn cancel_batch(&mut self) {
        let Some(mut job) = self.batch.take() else { return };
        job.snapshot.retryable = false;
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
            job.snapshot.retryable = retry_candidate(&job);
            self.snapshot.batch = Some(job.snapshot.clone());
            let _ = self.persist_batch(&job.snapshot);
            self.batch = Some(job);
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
            design_style: self.style_use(&pending.skills, pending.task.as_ref().map(|task| &task.target)),
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
            issues: self.snapshot.current_issues.clone(),
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
                    let (plans, proofs) = match product {
                        worker::Product::BatchGroup { plans, proofs } => (plans, proofs),
                        worker::Product::BatchPlan { plan, proofs } => (vec![plan], proofs),
                        _ => return Err("Batch worker did not return native proof evidence".into()),
                    };
                    self.publish_batch(session, &mut job, &plans, proofs, scope, &mut batch_report, &mut report)
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
                if let Err(error) = self.bind_production_draft(session, &job.snapshot.job_id) {
                    report.message.push_str(&format!("; saved continuation instructions could not be updated: {error}"));
                }
            }
            Err(error) => {
                job.snapshot.status = if cancelled { "cancelled" } else { "failed" }.into();
                batch_report.status = job.snapshot.status.clone();
                report.status = job.snapshot.status.clone();
                report.message = error;
            }
        }
        job.snapshot.batches.push(batch_report);
        job.snapshot.retryable = retry_candidate(&job);
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
        if matches!(job.snapshot.status.as_str(), "awaiting_human_proof" | "failed") {
            self.batch = Some(job);
        }
    }

    fn publish_batch(
        &mut self,
        session: &mut Session,
        job: &mut BatchJob,
        plans: &[ProductionPlan],
        proofs: Vec<worker::ProofArtifact>,
        scope: worker::BatchScope,
        batch_report: &mut BatchReport,
        report: &mut TaskReport,
    ) -> Result<(), String> {
        ensure_publication_idle(session)?;
        if !Arc::ptr_eq(&job.publication_cache, &session.cache) {
            return Err("Font cache changed during generation; proof no longer matches publication".into());
        }
        if serde_json::to_value(&session.prefs).map_err(|e| e.to_string())? != serde_json::to_value(&job.prefs).map_err(|e| e.to_string())? {
            return Err("Application preferences changed during generation; proof no longer matches publication".into());
        }
        let first_plan = plans.first().ok_or("A review group must contain native plans")?;
        let actions = plans.iter().try_fold(0usize, |total, plan| total.checked_add(plan.commands.len())).ok_or("Review action count overflow")?;
        let scope_end = scope
            .start
            .checked_add(scope.count)
            .filter(|end| (1..=1000).contains(&scope.count) && *end <= 1000)
            .ok_or("Invalid review proof page range")?;
        let maximum_proofs = scope
            .count
            .checked_add(scope.count.div_ceil(32))
            .and_then(|count| count.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or("Batch proof count overflow")?;
        if proofs.len() != maximum_proofs {
            return Err("Batch proof count or byte limit exceeded".into());
        }
        let mut total_bytes = 0usize;
        let mut names = std::collections::BTreeSet::new();
        for proof in &proofs {
            let extension = Path::new(&proof.name).extension().and_then(|x| x.to_str());
            if proof.name.contains(['/', '\\', ':'])
                || !matches!(extension, Some("png" | "pdf" | "designcraft"))
                || !names.insert(proof.name.as_str())
            {
                return Err("Invalid or duplicate batch proof artifact name".into());
            }
            let cap = if extension == Some("png") { MAX_PAGE_PROOF_BYTES } else { MAX_PROOF_FILE_BYTES };
            if proof.bytes.len() > cap {
                return Err("Batch proof file exceeds its byte limit".into());
            }
            total_bytes = total_bytes
                .checked_add(proof.bytes.len())
                .filter(|total| *total <= MAX_PROOF_TOTAL_BYTES)
                .ok_or("Batch proofs exceed their aggregate byte limit")?;
        }
        let mut expected_names = std::collections::BTreeSet::from(["document.designcraft".to_owned()]);
        for page in scope.start..scope_end {
            expected_names.insert(format!("page-{}.png", page.saturating_add(1)));
        }
        let mut pdf_start = scope.start;
        while pdf_start < scope_end {
            let pdf_end = pdf_start.checked_add(32).ok_or("Review PDF page range overflow")?.min(scope_end);
            expected_names.insert(if scope.count <= 32 {
                "proof.pdf".into()
            } else {
                format!("proof-pages-{}-{pdf_end}.pdf", pdf_start.saturating_add(1))
            });
            pdf_start = pdf_end;
        }
        if names.len() != expected_names.len() || names.iter().any(|name| !expected_names.contains(*name)) {
            return Err("Batch proofs must cover exactly every review page with one native document and complete PDF ranges".into());
        }
        let first = job.snapshot.document_uid.is_none();
        let previous = session.active_index();
        if first {
            let profile = job.skills.iter().find_map(|skill| skill.design_profile.as_ref());
            session
                .execute("file.new", &production_document_params(profile, job.snapshot.total_pages, &job.task.prompt)?)
                .map_err(|e| e.to_string())?;
            initialize_print_document(session, &job.task.prompt)?;
            // Print preferences define the new document's creation baseline. Facing setup
            // and reviewed artwork are published together by applyGroup below, so undoing
            // this batch restores its complete pre-publication state in one step.
            let created = session.doc_mut().map_err(|e| e.to_string())?;
            created.history.undo.clear();
            created.history.redo.clear();
        }
        let mut switched_tab = false;
        let result = (|| {
            if !first {
                let Some(target_index) = session.documents().iter().position(|doc| Some(doc.uid) == job.snapshot.document_uid) else {
                    return Err("The book changed while its batch was generating; publication is refused".into());
                };
                let target = session.documents().get(target_index).ok_or("Target book disappeared")?;
                if target.interaction.is_some() || target.preview_stash.is_some() {
                    return Err("Finish the current interaction, IME composition or preview before publishing AI production".into());
                }
                if !job.publication_base.as_ref().is_some_and(|original| {
                    Arc::ptr_eq(&original.doc, &target.doc)
                        && original.active_layer == target.active_layer
                        && original.editing_parents == target.editing_parents
                }) {
                    return Err("The book changed while its batch was generating; publication is refused".into());
                }
                if session.active_index() != Some(target_index) {
                    session.set_active(target_index);
                    switched_tab = true;
                }
            }
            let current = session.doc().map_err(|e| e.to_string())?;
            let mut base = current.clone();
            base.selection = Default::default();
            let change_layout = job.task.settings.facing_pages && !base.doc.settings.facing_pages;
            let facing = if change_layout {
                Some(crate::facing_pages::formatted_proof_state(&base, &job.prefs, book_binding(&job.task.settings, &job.task.prompt))?)
            } else {
                None
            };
            let projected_base = facing.as_ref().unwrap_or(&base);
            let canonical = crate::facing_pages::canonicalize(projected_base, &job.prefs)?;
            let required_min_ppi = crate::print_contract::PrintContract::from_prompt(&job.task.prompt)?.and_then(|contract| contract.min_image_ppi());
            let min_ppi = required_min_ppi.unwrap_or(150.0);
            worker::simulate_group_scoped_with_ppi(
                plans,
                &canonical,
                &job.prefs,
                job.task.settings.output_mode,
                &AtomicBool::new(false),
                scope,
                required_min_ppi,
            )?;
            let mapped: Vec<_> = plans.iter().map(|plan| crate::facing_pages::map_plan(plan, projected_base)).collect::<Result<_, _>>()?;
            let chunks = mapped.iter().map(|plan| json!({"commands":plan.commands,"label":plan.title})).collect::<Vec<_>>();
            let mut projected = Session::new();
            projected.prefs = job.prefs.clone();
            projected.add_document(base.clone());
            let mut group_params = json!({"chunks":chunks,"outputMode":job.task.settings.output_mode,"requirePreflight":true,"minPpi":min_ppi});
            if change_layout {
                group_params["documentSetup"] =
                    json!({"facingPages":true,"binding":if book_binding(&job.task.settings,&job.task.prompt){"rightToLeft"}else{"leftToRight"}});
            }
            projected.execute("production.applyGroup", &group_params).map_err(|e| e.to_string())?;
            crate::facing_pages::validate_mapped_result(projected_base, projected.doc().map_err(|e| e.to_string())?, scope)?;
            if required_min_ppi.is_some() {
                let projected_preflight = projected.execute("preflight.run", &json!({"minPpi": min_ppi})).map_err(|e| e.to_string())?;
                worker::ensure_no_new_low_resolution(&projected_preflight, &base, required_min_ppi)?;
            }
            let uid = current.uid;
            let revision = current.revision;
            let directory_name = if job.retry_sequence == 0 {
                format!("batch-{}", job.snapshot.current_batch)
            } else {
                format!("batch-{}-retry-{}", job.snapshot.current_batch, job.retry_sequence)
            };
            let directory = self.root.join("runs").join(&job.snapshot.job_id).join(directory_name);
            self.profile_access.check(&directory, file_access::Access::Write)?;
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            for proof in proofs {
                let path = directory.join(proof.name);
                write_profile(&self.profile_access, &path, &proof.bytes)?;
                batch_report.proof_sha256.push(hash(&proof.bytes));
                batch_report.proof_paths.push(path.to_string_lossy().into_owned());
            }
            let trace = directory.join("trace.json");
            write_json(
                &self.profile_access,
                &trace,
                &json!({"jobId":job.snapshot.job_id,"batch":job.snapshot.current_batch,"scope":scope,"settings":job.task.settings,"skills":report.skills,"designStyle":report.design_style,"usage":report.usage,"elapsedMs":report.elapsed_ms,"plans":plans,"plan":(plans.len()==1).then_some(first_plan),"proofs":batch_report.proof_paths,"sha256":batch_report.proof_sha256}),
            )?;
            let label = format!("{} · batch {}", limited_text(&first_plan.title, 200), job.snapshot.current_batch);
            group_params["label"] = json!(label);
            group_params["expectedDocumentUid"] = json!(uid);
            group_params["expectedRevision"] = json!(revision);
            session.execute("production.applyGroup", &group_params).map_err(|e| e.to_string())?;
            let state = session.doc().map_err(|e| e.to_string())?;
            job.snapshot.document_uid = Some(state.uid);
            job.snapshot.document_revision = Some(state.revision);
            job.publication_base = Some(state.clone());
            if let Some(contract) = crate::print_contract::PrintContract::from_prompt(&job.task.prompt)? {
                self.print_contracts.insert(state.uid, contract);
            }
            let preflight = session.execute("preflight.run", &json!({"minPpi":min_ppi})).map_err(|e| e.to_string())?;
            report.issues.extend(preflight.get("issues").and_then(Value::as_array).into_iter().flatten().cloned());
            report.title = first_plan.title.clone();
            batch_report.title = first_plan.title.clone();
            report.actions = actions;
            report.artifacts = batch_report.proof_paths.clone();
            report.artifacts.push(trace.to_string_lossy().into_owned());
            Ok(())
        })();
        if first && result.is_err() {
            session.execute("file.close", &json!({})).map_err(|e| e.to_string())?;
            if let Some(index) = previous {
                session.execute("file.activate", &json!({"index":index})).map_err(|e| e.to_string())?;
            }
        } else if !first
            && switched_tab
            && result.is_err()
            && let Some(index) = previous
        {
            session.set_active(index);
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
        let contract = crate::print_contract::PrintContract::from_prompt(&task.prompt)?;
        let params = match &contract {
            Some(contract) => contract.pdf_export_params(Some(pdf.to_str().ok_or("PDF export path is not valid Unicode")?))?,
            None => json!({"path":pdf,"spreads":false,"bleed":false,"marks":false}),
        };
        session.execute("file.exportPdf", &params).map_err(|e| e.to_string())?;
        Ok(vec![source.to_string_lossy().into_owned(), pdf.to_string_lossy().into_owned()])
    }
}
fn batch_report_id(job: &BatchJob) -> String {
    let base = format!("{}-batch-{}", job.snapshot.job_id, job.snapshot.current_batch);
    if job.retry_sequence == 0 { base } else { format!("{base}-retry-{}", job.retry_sequence) }
}
fn retry_candidate(job: &BatchJob) -> bool {
    job.snapshot.status == "failed"
        && job.ledger.is_some()
        && job.snapshot.completed_pages < job.snapshot.total_pages
        && job
            .snapshot
            .batches
            .last()
            .is_some_and(|batch| batch.status == "failed" && batch.page_start == job.snapshot.completed_pages && batch.page_count > 0)
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
fn batch_context(session: &Session, job: &BatchJob, root: &Path) -> Result<Value, String> {
    let state = session.doc().map_err(|e| e.to_string())?;
    let completed = usize::try_from(job.snapshot.completed_pages).map_err(|_| "Invalid approved page count")?;
    let preceding = job.snapshot.batches.iter().rev().find(|batch| {
        batch.status == "approved"
            && batch.page_count > 0
            && batch.page_start.checked_add(batch.page_count).is_some_and(|end| end <= job.snapshot.completed_pages)
    });
    let end = completed;
    let start = if let Some(batch) = preceding {
        usize::try_from(batch.page_start).map_err(|_| "Invalid preceding batch page")?
    } else {
        // A fresh continuation observes the existing prefix without inventing
        // historical human approvals or reading a failed unpublished suffix.
        completed.saturating_sub(usize::try_from(job.snapshot.pages_per_batch.clamp(1, 32)).map_err(|_| "Invalid review size")?)
    };
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
    let mut ranges: Vec<_> = job
        .snapshot
        .batches
        .iter()
        .filter(|batch| {
            batch.status == "approved"
                && batch.page_count > 0
                && batch.page_start.checked_add(batch.page_count).is_some_and(|end| end <= job.snapshot.completed_pages)
        })
        .rev()
        .take(64)
        .map(|batch| json!([batch.batch, batch.page_start, batch.page_count, limited_text(&batch.title, 80)]))
        .collect();
    ranges.reverse();
    let title = limited_text(&job.task.prompt, 256);
    let mut context = compact_context(session)?;
    let object = context.as_object_mut().ok_or("Invalid native book context")?;
    object.remove("objects");
    object.remove("objectsTruncated");
    let continuity = json!({"units":"points; each canonical native spread is one actual document page","approvedPageCount":job.snapshot.completed_pages,"approvedPageBriefs":briefs,"recentApprovedBatchRanges":ranges,"earlierBatchRangesOmitted":job.snapshot.batches.len()>64,"recentApprovedObjects":objects,"recentObjectsTruncated":truncated,"bookRequestSummary":title,"continuityInstruction":"Continue the selected original book after its existing completed prefix. Page briefs are bounded observations of editable content, not invented historical approval or educational certification. Do not repeat the beginning or regenerate earlier pages."});
    object.extend(continuity.as_object().ok_or("Invalid book continuity data")?.clone());
    enrich_production_context(&mut context, root, state, &job.task)?;
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
    #[test]
    fn continuation_refuses_blank_books_filled_books_and_holes_but_preserves_custom_trim() {
        let mut session = Session::new();
        session.execute("file.new", &new_a4(4)).unwrap();
        assert!(continuation_progress(session.doc().unwrap(), None, "").is_err());
        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
        assert_eq!(continuation_progress(session.doc().unwrap(), None, "").unwrap(), (4, 1));
        session.execute("frame.create", &json!({"spread":2,"rect":[50,50,100,100]})).unwrap();
        assert!(continuation_progress(session.doc().unwrap(), None, "").is_err());
        session.execute("frame.create", &json!({"spread":1,"rect":[50,50,100,100]})).unwrap();
        session.execute("frame.create", &json!({"spread":3,"rect":[50,50,100,100]})).unwrap();
        assert!(continuation_progress(session.doc().unwrap(), None, "").is_err());
        session.execute("file.new", &json!({"width":400,"height":600,"pages":2,"facingPages":false})).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
        assert_eq!(continuation_progress(session.doc().unwrap(), None, "").unwrap(), (2, 1));
        assert!(continuation_progress(session.doc().unwrap(), None, "Trim Size: A4").is_err());
    }

    #[test]
    fn continuation_preserves_actual_trim_when_style_or_default_sizes_differ() {
        let directory = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(directory.path()).unwrap();
        let mut profile = runner.snapshot.skills.iter().find_map(|card| card.design_profile.clone()).unwrap();
        profile.page_mm = [240.0, 320.0];
        profile.margin_mm = 10.0;
        let mut session = Session::new();
        session.execute("file.new", &crate::design::document_params(Some(&profile), 3)).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
        assert_eq!(continuation_progress(session.doc().unwrap(), None, "").unwrap(), (3, 1));
        assert_eq!(continuation_progress(session.doc().unwrap(), Some(&profile), "").unwrap(), (3, 1));
        profile.page_mm = [250.0, 350.0];
        assert_eq!(continuation_progress(session.doc().unwrap(), Some(&profile), "").unwrap(), (3, 1));
        assert!(continuation_progress(session.doc().unwrap(), Some(&profile), "Trim Size: 250 x 350 mm").is_err());
    }

    #[test]
    fn ai20_continuation_preserves_opened_custom_trim_instead_of_forcing_a_style_size() {
        let directory = tempfile::tempdir().unwrap();
        let runner = ProductionRunner::open(directory.path()).unwrap();
        let profile = runner.snapshot.skills.iter().find_map(|card| card.design_profile.clone()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":195.0*72.0/25.4,"height":265.0*72.0/25.4,"pages":3,"facingPages":false})).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
        assert_eq!(continuation_progress(session.doc().unwrap(), Some(&profile), "").unwrap(), (3, 1));
    }

    #[test]
    fn continuation_reports_page_map_gaps_and_rejects_script_page_count_mismatch() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":4,"facingPages":false})).unwrap();
        session.execute("frame.create", &json!({"spread":1,"rect":[50,50,100,100]})).unwrap();
        session.execute("frame.create", &json!({"spread":2,"rect":[50,50,100,100]})).unwrap();
        let leading_blank_err = continuation_progress(session.doc().unwrap(), None, "").unwrap_err().to_string();
        assert!(leading_blank_err.contains("completed pages: 2–3; blank pages: 1, 4"), "{leading_blank_err}");

        session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
        let script = (1..=3).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect::<String>();
        let mismatch_err = continuation_progress(session.doc().unwrap(), None, &script).unwrap_err().to_string();
        assert!(mismatch_err.contains("Explicit page-script count (3) must match the selected book page count (4)"), "{mismatch_err}");
    }

    #[test]
    fn ai20_continuation_accepts_matching_explicit_trim_and_rejects_a_conflicting_brief_before_http() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        for (trim, allowed) in [("195 x 265 mm", true), ("210 x 297 mm", false)] {
            let directory = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(directory.path()).unwrap();
            let mut session = Session::new();
            session.execute("file.new", &json!({"width":195.0*72.0/25.4,"height":265.0*72.0/25.4,"pages":3,"facingPages":false})).unwrap();
            session.execute("frame.create", &json!({"spread":0,"rect":[50,50,100,100]})).unwrap();
            let before = session.doc().unwrap().doc.clone();
            let mut request = task("http://127.0.0.1:9/".into(), 1);
            request.target = TaskTarget::ContinueBook;
            request.prompt = format!("Trim Size: {trim}\nContinue the remaining blank pages with editable native artwork.");
            let started = runner.handle(&mut session, AiRequest::Start(request));
            assert_eq!(started.busy, allowed, "{}", started.message);
            if allowed {
                wait(&mut runner, &mut session);
            } else {
                assert!(runner.pending.is_none());
            }
            assert!(Arc::ptr_eq(&before, &session.doc().unwrap().doc));
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
            publication_cache: session.cache.clone(),
            publication_base: session.active().cloned(),
            retry_sequence: 0,
        });
        ("fixture-book".into(), 1)
    }

    #[test]
    fn continuation_failed_worker_retains_budget_and_book_for_explicit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let before = runner.batch.as_ref().unwrap().ledger.as_ref().unwrap().summary();
        runner.lose_batch_worker();
        assert_eq!(runner.snapshot.batch.as_ref().unwrap().status, "failed");
        assert!(runner.batch.is_some(), "A failed live job needs an explicit retry path without resetting its ledger");
        let retained = runner.batch.as_ref().unwrap();
        assert_eq!(retained.snapshot.completed_pages, 1);
        assert_eq!(retained.ledger.as_ref().unwrap().summary().requests, before.requests);
        assert!(runner.pending.is_none(), "Failure retention cannot auto-launch another request");
    }

    #[test]
    fn continuation_view_only_changes_do_not_invalidate_unchanged_proof() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let before = session.doc().unwrap().clone();
        session.execute("select.firstAbove", &json!({})).unwrap();
        let current = session.doc().unwrap();
        assert!(Arc::ptr_eq(&before.doc, &current.doc));
        assert_ne!(before.revision, current.revision);
        let revision = current.revision;
        let result = runner.handle(&mut session, AiRequest::Poll);
        let batch = result.batch.unwrap();
        assert_eq!(batch.document_revision, Some(revision), "An unchanged native allocation must remain reviewable after selection/navigation");
        assert_eq!(batch.status, "awaiting_human_proof");
        assert_eq!(batch.cumulative_usage.requests, 1);
        assert!(runner.pending.is_none(), "Refreshing unchanged proof authority must not request production");
    }

    #[test]
    fn continuation_proof_repin_refuses_content_font_cache_and_preference_changes() {
        for change in ["content", "cache", "preferences"] {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            let original = runner.snapshot.batch.as_ref().unwrap().document_revision;
            match change {
                "content" => {
                    session.execute("frame.create", &json!({"spread":0,"rect":[200,60,300,150],"content":"unassigned"})).unwrap();
                }
                "cache" => session.cache = Arc::new(designcraft_engine::compose::Cache::new()),
                _ => session.prefs.typographers_quotes = !session.prefs.typographers_quotes,
            }
            session.execute("select.firstAbove", &json!({})).unwrap();
            let result = runner.handle(&mut session, AiRequest::Poll);
            assert_eq!(result.batch.unwrap().document_revision, original, "{change} must invalidate unchanged-proof rebinding");
            assert!(runner.pending.is_none());
        }
    }

    #[test]
    fn public_poll_publishes_new_rtl_facing_book_with_one_undo_and_human_proofs() {
        synthetic_facing_publication(false, false);
    }

    #[test]
    fn public_poll_continues_single_page_book_as_rtl_facing_without_changing_prefix() {
        synthetic_facing_publication(true, false);
    }

    #[test]
    fn public_poll_keeps_new_print_baseline_in_creation_not_a_second_undo() {
        synthetic_facing_publication(false, true);
    }

    fn synthetic_facing_publication(continuing: bool, baseline: bool) {
        use base64::Engine as _;
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(if continuing { 6 } else { 1 })).unwrap();
        let prefix = if continuing { 2 } else { 1 };
        for page in 0..prefix {
            let created = session.execute("frame.create", &json!({"spread":page,"rect":[60,60,160,160]})).unwrap();
            session.execute("object.label", &json!({"ids":[created["id"]],"label":format!("Preserved prefix {}",page+1)})).unwrap();
        }
        let before = session.doc().unwrap().clone();
        let previous_index = session.active_index().unwrap();
        let document_count = session.documents().len();
        let mut request = task("http://127.0.0.1:9/".into(), 1);
        request.prompt = "Content Copy Language: Arabic\nSix-page editable book with independently reviewed native pages.".into();
        if baseline {
            request.prompt.push_str("\nGrid System: 8 mm baseline grid.");
        }
        request.settings.facing_pages = true;
        request.settings.right_to_left_binding = Some(true);
        request.settings.pages_per_batch = if continuing { 2 } else { 3 };
        request.target = if continuing { TaskTarget::ContinueBook } else { TaskTarget::NewA4 { pages: 6 } };
        let start = if continuing { 2 } else { 0 };
        let count = request.settings.pages_per_batch;
        let (canonical, _) = task_document_context(&session, &request.target, None, &request.prompt, &request.settings).unwrap();
        assert_eq!(canonical.doc.spreads.len(), 6);
        assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc), "Private planning cannot reformat the live book");
        let plans: Vec<_> = (start..start + count)
            .map(|page| ProductionPlan {
                title: format!("Absolute page {}", page + 1),
                commands: vec![
                    EngineAction { command: "frame.create".into(), params: json!({"spread":page,"rect":[60,180,160,280]}) },
                    EngineAction { command: "object.label".into(), params: json!({"ids":["$0.id"],"label":format!("Reviewed page {}",page+1)}) },
                ],
                images: vec![],
            })
            .collect();
        let mut proof_session = Session::new();
        proof_session.prefs = session.prefs.clone();
        proof_session.add_document(canonical);
        let chunks: Vec<_> = plans.iter().map(|plan| json!({"commands":plan.commands,"label":plan.title})).collect();
        proof_session.execute("production.applyGroup", &json!({"chunks":chunks,"outputMode":"native_only","requirePreflight":true})).unwrap();
        let formatted = crate::facing_pages::formatted_proof_state(proof_session.doc().unwrap(), &proof_session.prefs, true).unwrap();
        proof_session.add_document(formatted);
        let mut proofs = vec![worker::ProofArtifact {
            name: "document.designcraft".into(),
            bytes: designcraft_format::save(&proof_session.doc().unwrap().doc).unwrap(),
        }];
        let pdf = proof_session
            .execute("file.exportPdf", &json!({"pages":(start+1..=start+count).collect::<Vec<_>>(),"spreads":false,"bleed":false,"marks":false}))
            .unwrap();
        proofs.push(worker::ProofArtifact {
            name: "proof.pdf".into(),
            bytes: base64::engine::general_purpose::STANDARD.decode(pdf["base64"].as_str().unwrap()).unwrap(),
        });
        let mut renderer = designcraft_render::Renderer::new();
        for page in start..start + count {
            let image = renderer
                .render_page(&proof_session.doc().unwrap().doc, &proof_session.cache, page as usize, 0.25, false, &Default::default())
                .unwrap();
            proofs.push(worker::ProofArtifact { name: format!("page-{}.png", page + 1), bytes: image.to_png() });
        }
        let ledger = UsageLedger::new(request.settings.budget.clone()).unwrap();
        let snapshot = BatchSnapshot {
            job_id: if continuing { "synthetic-facing-continue" } else { "synthetic-facing-new" }.into(),
            total_pages: 6,
            pages_per_batch: count,
            current_batch: 1,
            total_batches: (6 - start).div_ceil(count),
            completed_pages: start,
            status: "running".into(),
            document_uid: continuing.then_some(before.uid),
            document_revision: continuing.then_some(before.revision),
            ..Default::default()
        };
        runner.batch = Some(BatchJob {
            task: request.clone(),
            skills: vec![],
            roots: None,
            ledger: None,
            before: Default::default(),
            snapshot: snapshot.clone(),
            prefs: session.prefs.clone(),
            publication_cache: session.cache.clone(),
            publication_base: continuing.then_some(before.clone()),
            retry_sequence: 0,
        });
        runner.snapshot.batch = Some(snapshot.clone());
        runner.snapshot.busy = true;
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(worker::Message::FinishedWithLedger(
                worker::Outcome { result: Ok(worker::Product::BatchGroup { plans, proofs }), usage: Default::default() },
                ledger,
            ))
            .unwrap();
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: Arc::new(AtomicBool::new(false)) },
            id: format!("{}-batch-1", snapshot.job_id),
            started: Instant::now(),
            task: Some(request.clone()),
            expected: continuing.then_some((before.uid, before.revision)),
            expected_prefs: None,
            skills: vec![],
            settings: request.settings,
        });
        let completed = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(completed.stage, "awaiting_human_proof", "{}", completed.message);
        assert!(!completed.busy);
        assert!(runner.pending.is_none());
        assert!(runner.batch.is_some(), "Human proof authority must remain unconsumed");
        let checkpoint = completed.batch.as_ref().unwrap();
        assert_eq!((checkpoint.completed_pages, checkpoint.checkpoint), (start + count, 1));
        assert_eq!(checkpoint.cumulative_usage.requests, 0, "Synthetic completion must not call any provider");
        let published = session.doc().unwrap().clone();
        assert!(published.doc.settings.facing_pages && published.doc.settings.right_to_left_binding);
        if baseline {
            assert!((published.doc.settings.baseline_grid.increment - 8.0 * 72.0 / 25.4).abs() < 0.001);
        }
        assert_eq!((published.doc.page_count(), published.doc.spreads.len()), (6, 4));
        let local = crate::facing_pages::canonicalize(&published, &session.prefs).unwrap();
        for page in 0..6 {
            let spread = &local.doc.spreads[page];
            if (start..start + count).contains(&(page as u32)) {
                assert_eq!(spread.items.len(), 1);
                assert_eq!(spread.items[0].label, format!("Reviewed page {}", page + 1));
                assert_eq!(spread.items[0].bounds().x0, 60.0);
                assert_eq!(spread.items[0].bounds().y0, 180.0);
            } else if continuing && page < prefix {
                assert_eq!(spread.items, before.doc.spreads[page].items, "Prefix artwork must remain exactly editable and unchanged");
                assert_eq!(spread.pages[0].id, before.doc.spreads[page].pages[0].id);
            } else {
                assert!(spread.items.is_empty(), "Future absolute pages must stay blank");
            }
        }
        assert_eq!(
            published.history.undo.len(),
            if continuing { before.history.undo.len() + 1 } else { 1 },
            "One reviewed batch must produce one undo entry"
        );
        if !continuing {
            assert_eq!(session.documents().len(), document_count + 1);
            assert!(Arc::ptr_eq(&session.documents()[previous_index].doc, &before.doc), "Creating a book must preserve the former document");
        }
        let evidence = &checkpoint.batches[0];
        assert_eq!(evidence.status, "awaiting_human_proof");
        assert_eq!(evidence.proof_paths.len(), (count + 2) as usize);
        for path in &evidence.proof_paths {
            let bytes = std::fs::read(path).unwrap();
            match Path::new(path).extension().and_then(|extension| extension.to_str()).unwrap() {
                "designcraft" => {
                    let proof = designcraft_format::load(&bytes).unwrap();
                    assert!(proof.settings.facing_pages && proof.settings.right_to_left_binding);
                    assert_eq!((proof.page_count(), proof.spreads.len()), (6, 4));
                }
                "pdf" => assert!(bytes.starts_with(b"%PDF-")),
                "png" => {
                    let image = image::load_from_memory(&bytes).unwrap();
                    assert!(image.width() < image.height(), "Each PNG must show one absolute portrait page, not a facing spread");
                }
                other => panic!("Unexpected proof format {other}"),
            }
        }
        let mut undo = Session::new();
        undo.add_document(published);
        undo.execute("edit.undo", &json!({})).unwrap();
        if continuing {
            assert!(Arc::ptr_eq(&undo.doc().unwrap().doc, &before.doc));
            assert!(!undo.doc().unwrap().doc.settings.facing_pages, "One undo must restore single-page topology as well as artwork");
        } else {
            assert!(undo.doc().unwrap().doc.spreads.iter().all(|spread| spread.items.is_empty()));
            if baseline {
                assert!((undo.doc().unwrap().doc.settings.baseline_grid.increment - 8.0 * 72.0 / 25.4).abs() < 0.001);
            }
        }
        for _ in 0..3 {
            let unchanged = runner.handle(&mut session, AiRequest::Poll);
            assert_eq!(unchanged.stage, "awaiting_human_proof");
            assert_eq!(unchanged.batch.unwrap().checkpoint, 1);
            assert!(runner.pending.is_none(), "Polling cannot send another request before human approval");
        }
    }

    #[test]
    fn explicit_approval_is_the_only_path_between_real_http_batches() {
        reviewed_http_batches(0, false);
    }

    #[test]
    fn continuing_an_existing_book_preserves_its_prefix_and_human_http_checkpoints() {
        reviewed_http_batches(2, false);
    }

    #[test]
    fn continuation_retry_keeps_failed_usage_prefix_and_human_checkpoints() {
        reviewed_http_batches(2, true);
    }

    #[test]
    fn continuation_retry_before_first_publication_creates_only_its_new_book() {
        reviewed_http_batches(0, true);
    }

    #[test]
    fn four_page_review_uses_two_internal_parts_one_checkpoint_and_one_undo() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
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
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let request = loop {
                    if bytes.len() > 1024 * 1024 || Instant::now() >= deadline {
                        return Err("Mock content request exceeded bounds".into());
                    }
                    let count = socket.read(&mut chunk).map_err(|e| e.to_string())?;
                    if count == 0 {
                        return Err("Mock content request ended early".into());
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    let Some((headers, body)) = String::from_utf8_lossy(&bytes).split_once("\r\n\r\n").map(|(h, b)| (h.to_owned(), b.to_owned()))
                    else {
                        continue;
                    };
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .ok_or("Missing mock Content-Length")?;
                    if let Some(body) = body.as_bytes().get(..length) {
                        break serde_json::from_slice::<Value>(body).map_err(|e| e.to_string())?;
                    }
                };
                let index = server_calls.fetch_add(1, Ordering::Relaxed);
                if index > 2 {
                    return Err("Unexpected request beyond the approved five-page fixture".into());
                }
                let content = request.pointer("/messages/1/content").and_then(Value::as_str).ok_or("Missing content-planning request")?;
                let input: Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
                let scope: worker::BatchScope = serde_json::from_value(input["scope"].clone()).map_err(|e| e.to_string())?;
                let expected = if index < 2 { (index * 2, 2) } else { (4, 1) };
                if (scope.start, scope.count) != expected {
                    return Err(format!("Unexpected internal scope: {} + {}", scope.start, scope.count));
                }
                let pages = (scope.start..scope.start + scope.count)
                    .map(|page| {
                        json!({
                            "page":page,"family":"review-test","text":{"body":format!("Page {}", page + 1)},"illustrations":[]
                        })
                    })
                    .collect::<Vec<_>>();
                let reply = json!({"done":true,"message":{"content":json!({"title":"Full review group","pages":pages}).to_string()},
                    "prompt_eval_count":120,"eval_count":30})
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
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut profile = runner.snapshot.skills.iter().find_map(|card| card.design_profile.clone()).unwrap();
        profile.page_families.truncate(1);
        let family = &mut profile.page_families[0];
        family.id = "review-test".into();
        family.template = "templates/review-test.json".into();
        family.text_slots =
            vec![designcraft_ai_types::design::DesignTextSlot { id: "body".into(), purpose: "Page text".into(), command: 0, max_chars: 32 }];
        family.image_slots.clear();
        let fonts = designcraft_fonts::FontDb::global();
        let font = fonts
            .families()
            .into_iter()
            .find(|family| {
                fonts.styles(family).iter().any(|style| style == "Regular")
                    && "Page 12345".chars().all(|c| fonts.face(family, "Regular").glyph_for(c) != 0)
            })
            .unwrap();
        let mut actions = vec![
            EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,300,120],"content":"text","text":"Example"}) },
            EngineAction { command: "type.char".into(), params: json!({"attrs":{"fontFamily":font,"fontStyle":"Regular","size":18}}) },
            EngineAction { command: "type.para".into(), params: json!({"attrs":{"direction":"leftToRight","align":"left"}}) },
        ];
        actions.resize(200, EngineAction { command: "object.label".into(), params: json!({"ids":["$0.id"],"label":"Editable page"}) });
        let style = SkillSnapshot {
            skill_id: "review-test".into(),
            revision: "fixture".into(),
            instructions: "One editable heading per page".into(),
            references: Default::default(),
            reference_hashes: Default::default(),
            templates: std::collections::BTreeMap::from([("templates/review-test.json".into(), actions)]),
            design_profile: Some(profile),
        };
        assert_eq!(crate::template_plan::capacity(&style, designcraft_ai_types::OutputMode::NativeOnly).unwrap(), 2);
        let mut request = task(endpoint, 4);
        request.target = TaskTarget::NewA4 { pages: 5 };
        request.settings.pages_per_batch = 4;
        let mut session = Session::new();
        session.execute("file.new", &new_a4(5)).unwrap();
        let base = session.doc().unwrap().clone();
        runner.start_batches(&mut session, request, vec![style], base, json!({})).unwrap();
        let first = wait(&mut runner, &mut session);
        assert_eq!(first.stage, "awaiting_human_proof", "{}", first.message);
        let checkpoint = first.batch.as_ref().unwrap();
        assert_eq!((checkpoint.pages_per_batch, checkpoint.completed_pages, checkpoint.checkpoint), (4, 4, 1));
        assert_eq!(checkpoint.batches.len(), 1);
        assert_eq!(checkpoint.batches[0].page_count, 4);
        assert_eq!(checkpoint.batches[0].proof_paths.len(), 6, "Four PNG pages plus complete PDF and native evidence");
        assert_eq!(calls.load(Ordering::Relaxed), 2, "Internal capacity changes provider parts, never the human review size");
        let state = session.doc().unwrap();
        assert!(state.doc.spreads.iter().take(4).all(|page| !page.items.is_empty()));
        assert!(state.doc.spreads[4].items.is_empty());
        assert_eq!(state.history.undo.len(), 1, "The complete four-page group is one native undo step");
        let report = first.reports.last().unwrap();
        assert_eq!(report.actions, 800);
        let trace: Value = serde_json::from_slice(&std::fs::read(report.artifacts.last().unwrap()).unwrap()).unwrap();
        assert_eq!(trace["plans"].as_array().unwrap().len(), 2);
        let mut undo_probe = Session::new();
        undo_probe.add_document(state.clone());
        undo_probe.execute("edit.undo", &json!({})).unwrap();
        assert!(undo_probe.doc().unwrap().doc.spreads.iter().all(|page| page.items.is_empty()));
        for _ in 0..8 {
            runner.handle(&mut session, AiRequest::Poll);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2, "Page five must not reach HTTP before human approval");
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint });
        assert!(approved.busy, "{}", approved.message);
        let second = wait(&mut runner, &mut session);
        assert_eq!(second.stage, "awaiting_human_proof", "{}", second.message);
        assert_eq!(second.batch.as_ref().unwrap().completed_pages, 5);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(session.doc().unwrap().history.undo.len(), 2);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    }

    fn reviewed_http_batches(initial_completed: u32, fail_first: bool) {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
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
                let request_index = server_calls.fetch_add(1, Ordering::Relaxed);
                if request_index > 1 + u32::from(fail_first) {
                    return Err("Unexpected unapproved provider request".into());
                }
                let page = initial_completed + request_index.saturating_sub(u32::from(fail_first));
                let mut plan = json!({"title":"Reviewed native book","commands":[{"command":"frame.create","params":{"spread":page,"rect":[50,60,200,180],"content":"unassigned"}},{"command":"object.label","params":{"ids":["$0.id"],"label":format!("page-{}",page+1)}}],"images":[]});
                if fail_first && request_index == 0 {
                    plan["commands"][0]["command"] = json!("unsupported.fixture.command");
                }
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
        let mut request = task(endpoint, 3);
        let mut prefix = vec![];
        if initial_completed > 0 {
            session.execute("file.new", &new_a4(initial_completed + 2)).unwrap();
            for page in 0..initial_completed {
                session.execute("frame.create", &json!({"spread":page,"rect":[50,50,100,100],"content":"unassigned"})).unwrap();
            }
            prefix = session.doc().unwrap().doc.spreads.iter().take(initial_completed as usize).cloned().collect();
            request.target = TaskTarget::ContinueBook;
        }
        let prior_undo = session.doc().map(|state| state.history.undo.len()).unwrap_or(0);
        let started = runner.handle(&mut session, AiRequest::Start(request));
        assert!(started.busy, "{}", started.message);
        if initial_completed > 0 {
            let original = session.doc().unwrap().clone();
            session.execute("select.firstAbove", &json!({})).unwrap();
            assert_ne!(session.doc().unwrap().revision, original.revision);
            assert!(Arc::ptr_eq(&session.doc().unwrap().doc, &original.doc), "Selection changed without editing document content");
        }
        let mut first = wait(&mut runner, &mut session);
        if fail_first {
            assert_eq!(first.stage, "failed", "{}", first.message);
            let failed = first.batch.as_ref().unwrap();
            assert!(failed.retryable);
            assert_eq!(failed.completed_pages, initial_completed);
            assert_eq!(failed.cumulative_usage.requests, 1);
            assert_eq!(session.doc().map(|state| state.history.undo.len()).unwrap_or(0), prior_undo);
            let recovered = ProductionRunner::open(dir.path()).unwrap();
            assert!(!recovered.snapshot.batch.as_ref().unwrap().retryable);
            assert!(recovered.batch.is_none() && recovered.pending.is_none());
            let id = failed.job_id.clone();
            runner.handle(&mut session, AiRequest::Poll);
            assert_eq!(calls.load(Ordering::Relaxed), 1, "Failure cannot retry automatically");
            let started = runner.handle(&mut session, AiRequest::RetryBatch { job_id: id });
            assert!(started.busy, "{}", started.message);
            first = wait(&mut runner, &mut session);
        }
        let prior_requests = u32::from(fail_first);
        assert_eq!(first.stage, "awaiting_human_proof", "{}", first.message);
        let checkpoint = first.batch.as_ref().unwrap();
        assert_eq!(checkpoint.completed_pages, initial_completed + 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1 + prior_requests);
        assert_eq!(checkpoint.batches[0].proof_paths.len(), 3);
        let approved_page = session.doc().unwrap().doc.spreads[initial_completed as usize].clone();
        assert_eq!(&session.doc().unwrap().doc.spreads[..initial_completed as usize], prefix.as_slice());
        for _ in 0..8 {
            runner.handle(&mut session, AiRequest::Poll);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1 + prior_requests, "Polling must never prefetch the next batch");
        let wrong = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint + 1 });
        assert!(!wrong.busy);
        assert_eq!(calls.load(Ordering::Relaxed), 1 + prior_requests);
        let mut reopened = ProductionRunner::open(dir.path()).unwrap();
        assert_eq!(reopened.snapshot.batch.as_ref().unwrap().status, "interrupted");
        assert!(reopened.pending.is_none());
        assert!(reopened.batch.is_none());
        let denied = reopened.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint });
        assert!(!denied.busy);
        assert_eq!(calls.load(Ordering::Relaxed), 1 + prior_requests);
        drop(reopened);
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: checkpoint.job_id.clone(), checkpoint: checkpoint.checkpoint });
        assert!(approved.busy, "{}", approved.message);
        let second = wait(&mut runner, &mut session);
        assert_eq!(second.stage, "awaiting_human_proof", "{}", second.message);
        let final_checkpoint = second.batch.as_ref().unwrap();
        assert_eq!(final_checkpoint.completed_pages, initial_completed + 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2 + prior_requests);
        assert_eq!(final_checkpoint.cumulative_usage.requests, 2 + prior_requests);
        assert_eq!(final_checkpoint.cumulative_usage.input_tokens, u64::from(2 + prior_requests) * 120);
        assert!(final_checkpoint.batches.iter().all(|batch| batch.usage.requests == 1));
        assert_eq!(session.doc().unwrap().doc.spreads[initial_completed as usize], approved_page);
        assert_eq!(session.doc().unwrap().doc.spreads[(initial_completed + 1) as usize].items[0].label, format!("page-{}", initial_completed + 2));
        assert_eq!(&session.doc().unwrap().doc.spreads[..initial_completed as usize], prefix.as_slice());
        assert_eq!(session.doc().unwrap().history.undo.len(), prior_undo + 2);
        let completed =
            runner.handle(&mut session, AiRequest::ApproveBatch { job_id: final_checkpoint.job_id.clone(), checkpoint: final_checkpoint.checkpoint });
        assert_eq!(completed.batch.as_ref().unwrap().status, "completed");
        assert!(runner.pending.is_none());
        assert!(runner.batch.is_none());
        runner.handle(&mut session, AiRequest::ApproveBatch { job_id: final_checkpoint.job_id.clone(), checkpoint: final_checkpoint.checkpoint });
        for _ in 0..4 {
            runner.handle(&mut session, AiRequest::Poll);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2 + prior_requests, "Final or repeated approval cannot issue another request");
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn cumulative_request_limit_survives_a_human_checkpoint() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let (job_id, checkpoint) = waiting(&mut runner, &mut session, format!("http://{}/", listener.local_addr().unwrap()));
        let before = session.doc().unwrap().doc.clone();
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint });
        assert!(approved.busy, "{}", approved.message);
        let result = wait(&mut runner, &mut session);
        assert_eq!(result.batch.as_ref().unwrap().status, "failed");
        assert!(result.message.contains("request limit"), "{}", result.message);
        assert_eq!(result.batch.as_ref().unwrap().cumulative_usage.requests, 1);
        assert_eq!(result.batch.as_ref().unwrap().batches.last().unwrap().usage.requests, 0);
        assert!(listener.accept().is_err(), "Exhausted cumulative budget must stop before HTTP");
        assert!(Arc::ptr_eq(&before, &session.doc().unwrap().doc));
        let retried = runner.handle(&mut session, AiRequest::RetryBatch { job_id });
        assert!(retried.busy, "{}", retried.message);
        let stopped = wait(&mut runner, &mut session);
        assert!(stopped.message.contains("request limit"), "{}", stopped.message);
        assert_eq!(stopped.batch.as_ref().unwrap().cumulative_usage.requests, 1, "Retry cannot reset exhausted allowances");
        assert!(listener.accept().is_err());
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
        rejected_changed_publication("preferences");
    }

    #[test]
    fn document_content_changed_during_generation_cannot_be_overwritten() {
        rejected_changed_publication("document");
    }

    #[test]
    fn active_layer_changed_without_content_edits_cannot_change_the_published_layer() {
        rejected_changed_publication("active-layer");
    }

    #[test]
    fn font_cache_changed_during_generation_cannot_publish_stale_proofs() {
        rejected_changed_publication("font-cache");
    }

    #[test]
    fn font_cache_changed_before_first_book_publication_cannot_create_a_document() {
        rejected_changed_publication("font-cache-first");
    }

    #[test]
    fn real_font_reload_during_generation_refuses_stale_batch_publication() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        rejected_changed_publication("font-reload");
    }

    #[test]
    fn real_font_reload_before_first_book_publication_cannot_create_a_document() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        rejected_changed_publication("font-reload-first");
    }

    fn reload_fonts_without_document_edits(session: &mut Session) {
        let before = session.doc().unwrap().clone();
        let cache = session.cache.clone();
        let prefs = serde_json::to_value(&session.prefs).unwrap();
        session.execute("font.reload", &json!({})).unwrap();
        let after = session.doc().unwrap();
        assert!(!Arc::ptr_eq(&cache, &session.cache));
        assert!(Arc::ptr_eq(&before.doc, &after.doc));
        assert_eq!(before.is_dirty(), after.is_dirty());
        assert!(after.revision > before.revision);
        assert_eq!(serde_json::to_value(&session.prefs).unwrap(), prefs);
    }

    #[test]
    fn real_font_reload_invalidates_human_proof_approval_without_document_edits() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        reload_fonts_without_document_edits(&mut session);
        let before = session.doc().unwrap().clone();
        let rejected = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
        assert!(rejected.message.contains("Font cache changed"), "{}", rejected.message);
        assert!(!rejected.busy);
        assert!(runner.pending.is_none());
        assert!(runner.batch.is_none());
        assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert_eq!(before.is_dirty(), session.doc().unwrap().is_dirty());
        assert_eq!(before.revision, session.doc().unwrap().revision);
        assert_eq!(rejected.batch.as_ref().unwrap().cumulative_usage.requests, 1);
        assert!(!runner.root.join("runs").exists());
    }

    fn rejected_changed_publication(change: &str) {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let mut job = runner.batch.take().unwrap();
        let first = matches!(change, "font-cache-first" | "font-reload-first");
        let target_page = if first { 0 } else { 1 };
        if first {
            job.snapshot.document_uid = None;
            job.snapshot.document_revision = None;
            job.snapshot.completed_pages = 0;
            job.publication_base = None;
        }
        if change == "active-layer" {
            session.execute("layer.new", &json!({"name":"Other layer"})).unwrap();
            job.publication_base = session.active().cloned();
        }
        let before = session.doc().unwrap().clone();
        if change == "preferences" {
            session.prefs.typographers_quotes = !session.prefs.typographers_quotes;
            assert_eq!(session.doc().unwrap().revision, before.revision, "Preferences are independent of document revisions");
        } else if change == "active-layer" {
            session.execute("layer.activate", &json!({"id":1})).unwrap();
            assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
            assert_ne!(before.active_layer, session.doc().unwrap().active_layer);
        } else if matches!(change, "font-cache" | "font-cache-first") {
            let cache = session.cache.clone();
            session.cache = Arc::new(Default::default());
            session.doc_mut().unwrap().revision = before.revision.checked_add(1).unwrap();
            assert!(!Arc::ptr_eq(&cache, &session.cache));
            assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc), "Font reload preserves document content");
        } else if matches!(change, "font-reload" | "font-reload-first") {
            reload_fonts_without_document_edits(&mut session);
        } else {
            session.execute("frame.create", &json!({"spread":0,"rect":[200,60,300,180],"content":"unassigned"})).unwrap();
            assert!(!Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        }
        let before = session.doc().unwrap().clone();
        let documents = session.documents().len();
        let plan = ProductionPlan {
            title: "Smart quotes".into(),
            commands: vec![
                EngineAction { command: "frame.create".into(), params: json!({"spread":target_page,"rect":[50,50,300,180],"content":"text"}) },
                EngineAction { command: "text.insert".into(), params: json!({"text":"\"Proof text\""}) },
            ],
            images: vec![],
        };
        let mut report = TaskReport {
            design_style: None,
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
            worker::ProofArtifact { name: format!("page-{}.png", target_page + 1), bytes: vec![3] },
        ];
        let error = runner
            .publish_batch(
                &mut session,
                &mut job,
                &[plan],
                proofs,
                worker::BatchScope { start: target_page, count: 1 },
                &mut BatchReport::default(),
                &mut report,
            )
            .unwrap_err();
        let expected = if change == "preferences" {
            "preferences changed"
        } else if matches!(change, "font-cache" | "font-cache-first" | "font-reload" | "font-reload-first") {
            "Font cache changed"
        } else {
            "book changed"
        };
        assert!(error.contains(expected), "{error}");
        assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
        assert_eq!(session.documents().len(), documents, "Refusal must precede creating a new book");
        assert!(report.artifacts.is_empty());
        assert!(!runner.root.join("runs").exists(), "A mismatched proof cannot be persisted or published");
    }

    #[test]
    fn incomplete_or_misnamed_review_proofs_cannot_publish_native_content() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let mut job = runner.batch.take().unwrap();
        let before = session.doc().unwrap().clone();
        let plan = ProductionPlan {
            title: "Review".into(),
            commands: vec![EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":1,"rect":[60,60,160,160],"content":"unassigned"}),
            }],
            images: vec![],
        };
        let mut report = TaskReport {
            design_style: None,
            id: "proof-set".into(),
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
        for names in [
            ["document.designcraft", "proof.pdf", "page-3.png"],
            ["document.designcraft", "proof.pdf", "other.pdf"],
            ["document.designcraft", "proof-pages-2-2.pdf", "page-2.png"],
            ["other.designcraft", "proof.pdf", "page-2.png"],
            ["document.designcraft", "proof.pdf", "proof.pdf"],
        ] {
            let proofs = names.into_iter().map(|name| worker::ProofArtifact { name: name.into(), bytes: vec![1] }).collect();
            assert!(
                runner
                    .publish_batch(
                        &mut session,
                        &mut job,
                        std::slice::from_ref(&plan),
                        proofs,
                        worker::BatchScope { start: 1, count: 1 },
                        &mut BatchReport::default(),
                        &mut report
                    )
                    .is_err()
            );
            assert!(Arc::ptr_eq(&before.doc, &session.doc().unwrap().doc));
            assert_eq!(before.history.undo.len(), session.doc().unwrap().history.undo.len());
            assert!(report.artifacts.is_empty());
            assert!(!runner.root.join("runs").exists());
        }
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
            design_style: None,
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
            .publish_batch(
                &mut session,
                &mut job,
                &[plan],
                proofs,
                worker::BatchScope { start: 0, count: 1 },
                &mut BatchReport::default(),
                &mut report,
            )
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
        let context = batch_context(&session, &job, dir.path()).unwrap();
        assert_eq!(context["approvedPageCount"], 2);
        assert_eq!(context["approvedPageBriefs"].as_array().unwrap().len(), 2);
        let recent = context["recentApprovedObjects"].to_string();
        assert!(recent.contains("roots and leaves"));
        assert!(!recent.contains("Earlier approved chapter"));
        assert!(context["recentApprovedBatchRanges"].to_string().contains("Plants"));
        assert!(serde_json::to_vec(&context).unwrap().len() < 48 * 1024);
        assert!(limited_text("حروف عربية", 7).len() <= 7);
    }

    #[test]
    fn ai20_later_and_retry_context_keeps_environment_and_only_completed_native_observations() {
        for prior_report in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(directory.path()).unwrap();
            let mut session = Session::new();
            waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            session
                .execute(
                    "frame.create",
                    &json!({"spread":0,"rect":[200,60,400,120],"content":"text","text":"Existing approved roots and leaves","caret":false}),
                )
                .unwrap();
            let mut job = runner.batch.take().unwrap();
            job.task.settings.facing_pages = true;
            job.task.prompt = "Trim Size: A4\nMargins: 15 mm\nContinue the bilingual book".into();
            if prior_report {
                job.snapshot.batches[0].status = "approved".into();
            } else {
                job.snapshot.batches.clear();
            }
            job.snapshot.batches.push(BatchReport {
                batch: 2,
                page_start: 1,
                page_count: 1,
                status: "failed".into(),
                title: "UNPUBLISHED".into(),
                ..Default::default()
            });
            let context = batch_context(&session, &job, directory.path()).unwrap();
            assert!(context["fontFaces"].is_array());
            assert!(context["swatchNames"].as_array().unwrap().iter().any(|name| name == "[Black]"));
            assert!(context.get("_production_cache_root").is_some());
            assert_eq!(context["facing_editorial_direction"]["enabled"], true);
            assert!(context["production_print_contract"].is_object());
            assert!(context["recentApprovedObjects"].to_string().contains("roots and leaves"));
            assert!(!context["recentApprovedBatchRanges"].to_string().contains("UNPUBLISHED"));
            assert!(serde_json::to_vec(&context).unwrap().len() < 48 * 1024);
        }
    }

    #[test]
    fn multi_tab_switching_preserves_batch_approval_and_publication_for_unchanged_target_book() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let book_uid = session.doc().unwrap().uid;
        assert_eq!(runner.batch.as_ref().unwrap().snapshot.document_uid, Some(book_uid));

        // Open a second scratch document tab and edit IT while the target book tab remains untouched.
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        session.execute("frame.create", &json!({"spread": 0, "rect": [72, 72, 240, 140], "content": "text", "text": "Scratch tab notes"})).unwrap();
        let scratch_uid = session.doc().unwrap().uid;
        assert_ne!(book_uid, scratch_uid);
        assert_eq!(session.active_index(), Some(1));

        // Refreshing unchanged proof while viewing the scratch tab must succeed because target book is intact.
        runner.refresh_unchanged_proof(&session).unwrap();
        let mut job = runner.batch.take().unwrap();

        // Publishing batch 2 while viewing the scratch tab must activate the target book by uid and publish cleanly.
        let plan = ProductionPlan {
            title: "Page two".into(),
            commands: vec![EngineAction {
                command: "frame.create".into(),
                params: json!({"spread": 1, "rect": [50, 50, 300, 140], "content": "text", "text": "Second batch published across tabs", "caret": false}),
            }],
            images: vec![],
        };
        let mut report = TaskReport {
            design_style: None,
            id: "multi-tab-publish".into(),
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
        runner
            .publish_batch(
                &mut session,
                &mut job,
                &[plan],
                proofs,
                worker::BatchScope { start: 1, count: 1 },
                &mut BatchReport::default(),
                &mut report,
            )
            .unwrap();
        assert_eq!(session.doc().unwrap().uid, book_uid);
        assert!(session.doc().unwrap().doc.stories.values().any(|s| s.text == "Second batch published across tabs"));
        // Scratch tab remains intact in session.documents()[1].
        assert_eq!(session.documents()[1].uid, scratch_uid);
        assert!(session.documents()[1].doc.stories.values().any(|s| s.text == "Scratch tab notes"));
    }

    #[test]
    fn approve_batch_rebinds_unchanged_revision_and_preserves_checkpoint_across_transient_busy_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        // Mark as final batch so final approval completes locally without network IO.
        if let Some(snap) = runner.batch.as_mut().map(|job| {
            job.snapshot.completed_pages = job.snapshot.total_pages;
            job.snapshot.clone()
        }) {
            runner.persist_batch(&snap).unwrap();
            runner.snapshot.batch = Some(snap);
        }
        runner.snapshot.stage = "awaiting_human_proof".into();

        // 1. Active preview_stash on the target book (doc 0) must reject transiently without consuming the checkpoint.
        session.doc_mut().unwrap().bump_revision();
        session.doc_mut().unwrap().preview_stash = Some(session.doc().unwrap().doc.clone());

        let transient_snap = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint });
        assert_eq!(transient_snap.stage, "awaiting_human_proof", "transient preview must not fail the checkpoint");
        assert!(transient_snap.message.contains("Finish the current interaction, IME composition or preview"), "msg={}", transient_snap.message);
        assert_eq!(runner.batch.as_ref().unwrap().snapshot.status, "awaiting_human_proof");
        assert_eq!(transient_snap.batch.as_ref().unwrap().status, "awaiting_human_proof");
        session.stop_data_preview();

        // 2. Open a scratch tab (doc 1), start an uncommitted interaction on target book (doc 0),
        // then switch to doc 1: ApproveBatch must still reject transiently without consuming the checkpoint!
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        assert_eq!(session.active_index(), Some(1));
        session.set_active(0);
        {
            let st = session.doc_mut().unwrap();
            st.interaction = Some(designcraft_engine::Interaction {
                label: "drag".into(),
                doc: st.doc.clone(),
                selection: st.selection.clone(),
                preview: None,
                revision: st.revision,
            });
        }
        session.set_active(1);
        session.doc_mut().unwrap().preview_stash = Some(session.doc().unwrap().doc.clone());

        let active_busy_snap = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint });
        assert_eq!(active_busy_snap.stage, "awaiting_human_proof");
        assert_eq!(runner.batch.as_ref().unwrap().snapshot.status, "awaiting_human_proof");

        // Clear scratch tab preview_stash while target book (doc 0) still has uncommitted interaction:
        session.stop_data_preview();
        let target_busy_snap = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint });
        assert_eq!(target_busy_snap.stage, "awaiting_human_proof");
        assert!(target_busy_snap.message.contains("Finish the current interaction, IME composition or preview"));
        assert_eq!(runner.batch.as_ref().unwrap().snapshot.status, "awaiting_human_proof");

        // 3. Cancel uncommitted interaction on doc 0, bump revision (selection/navigation without Poll),
        // switch back to scratch tab (doc 1), and approve directly: must rebind and complete cleanly!
        session.set_active(0);
        session.doc_mut().unwrap().interaction = None;
        session.doc_mut().unwrap().bump_revision();
        session.set_active(1);
        let approved_snap = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
        assert_eq!(approved_snap.stage, "completed", "msg={}", approved_snap.message);
        assert_eq!(approved_snap.batch.as_ref().unwrap().status, "completed");
    }

    #[test]
    fn approve_batch_preserves_active_tab_and_state_consistency_on_proof_or_prep_failure_and_recovery_save() {
        // 1. Tampered proof file on inactive Tab 0 must NOT switch active tab away from Tab 1.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
            assert_eq!(session.active_index(), Some(1));
            std::fs::write(dir.path().join("fixture-proof.png"), b"tampered-bytes").unwrap();

            let failed = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
            assert_eq!(failed.stage, "failed");
            assert_eq!(failed.batch.as_ref().unwrap().status, "failed");
            assert!(failed.message.contains("Batch proof bytes changed"));
            assert_eq!(session.active_index(), Some(1), "failed approval must not switch the user's active tab");
        }

        // 2. Post-validation continuity overflow (`current_batch == u32::MAX`) must NOT leave snapshot.batch
        // claiming `awaiting_human_proof` while `self.batch` is `None`, nor switch active tab.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            runner.batch.as_mut().unwrap().snapshot.current_batch = u32::MAX;
            session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
            assert_eq!(session.active_index(), Some(1));

            let failed = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
            assert_eq!(failed.stage, "failed");
            assert_eq!(failed.batch.as_ref().unwrap().status, "failed", "snapshot.batch must not remain stuck on awaiting_human_proof");
            assert_eq!(session.active_index(), Some(1));
        }

        // 3. Background `file.recovery.save` across open tabs must preserve unchanged proof approval.
        {
            let dir = tempfile::tempdir().unwrap();
            let rec_dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            if let Some(snap) = runner.batch.as_mut().map(|job| {
                job.snapshot.completed_pages = job.snapshot.total_pages;
                job.snapshot.clone()
            }) {
                runner.persist_batch(&snap).unwrap();
                runner.snapshot.batch = Some(snap);
            }
            session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
            session.execute("frame.create", &json!({"spread": 0, "rect": [60, 60, 140, 120]})).unwrap();
            session.execute("file.recovery.save", &json!({"dir": rec_dir.path()})).unwrap();
            assert_eq!(session.active_index(), Some(1));

            let done = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
            assert_eq!(done.stage, "completed", "msg={}", done.message);
            assert_eq!(session.active_index(), Some(0));
        }
    }
}
