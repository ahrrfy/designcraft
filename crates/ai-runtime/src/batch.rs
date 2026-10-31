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

pub(crate) fn layout_relevant_prefs_value(prefs: &Prefs) -> Result<Value, String> {
    let mut value = serde_json::to_value(prefs).map_err(|e| e.to_string())?;
    if let Some(obj) = value.as_object_mut() {
        for non_layout_key in [
            "recentFiles",
            "recent_files",
            "recentFilesMax",
            "recent_files_max",
            "recentFonts",
            "recent_fonts",
            "favoriteFonts",
            "favorite_fonts",
            "showHiddenCharacters",
            "show_hidden_characters",
            "highlightHj",
            "highlight_hj",
            "highlightKeeps",
            "highlight_keeps",
            "highlightCustomTracking",
            "highlight_custom_tracking",
            "highlightSubstitutedFonts",
            "highlight_substituted_fonts",
            "showAddedText",
            "show_added_text",
            "showNoteAnchors",
            "show_note_anchors",
            "recoveryMinutes",
            "recovery_minutes",
            "showFontNamesInEnglish",
            "show_font_names_in_english",
            "cjkFeatures",
            "cjk_features",
            "documentPresets",
            "document_presets",
            "autocorrect",
            "autocorrectList",
            "autocorrect_list",
            "absolutePageNumbers",
            "absolute_page_numbers",
            "polygonSides",
            "polygon_sides",
            "starInset",
            "star_inset",
        ] {
            obj.remove(non_layout_key);
        }
    }
    Ok(value)
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
#[cfg(test)]
pub(super) fn continuation_progress(
    base: &DocState,
    profile: Option<&designcraft_ai_types::DesignProfile>,
    brief: &str,
) -> Result<(u32, u32), String> {
    continuation_progress_inner(base, profile, brief, false)
}

pub(super) fn continuation_or_blank_progress(
    base: &DocState,
    profile: Option<&designcraft_ai_types::DesignProfile>,
    brief: &str,
) -> Result<(u32, u32), String> {
    continuation_progress_inner(base, profile, brief, !brief.trim().is_empty())
}

fn continuation_progress_inner(
    base: &DocState,
    profile: Option<&designcraft_ai_types::DesignProfile>,
    brief: &str,
    allow_all_blank: bool,
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
    let has_trailing_items = base.doc.spreads.iter().skip(start).any(|spread| !spread.items.is_empty());
    if has_trailing_items || (start == 0 && !allow_all_blank) {
        let completed = page_ranges_summary(&base.doc.spreads, true);
        let blank = page_ranges_summary(&base.doc.spreads, false);
        return Err(format!(
            "Continue book requires completed opening pages followed only by blank pages; existing content will not be overwritten (completed pages: {completed}; blank pages: {blank})"
        ));
    }
    Ok((pages, u32::try_from(start).map_err(|_| "Invalid continuation page")?))
}

pub(crate) fn extract_requested_page_count(brief: &str) -> Result<Option<u32>, String> {
    if let Some(contract) = crate::script_contract::parse(brief)? {
        let count = u32::try_from(contract.pages.len()).map_err(|_| "Explicit page-script count exceeds limit")?;
        return Ok((1..=1000).contains(&count).then_some(count));
    }
    let normalized: String = brief
        .chars()
        .map(|c| match c {
            '\u{0660}'..='\u{0669}' => char::from(b'0' + (u32::from(c) - 0x0660) as u8),
            '\u{06F0}'..='\u{06F9}' => char::from(b'0' + (u32::from(c) - 0x06F0) as u8),
            _ => c.to_ascii_lowercase(),
        })
        .collect();
    let tokens: Vec<&str> =
        normalized.split(|c: char| !c.is_alphanumeric() && !('\u{0600}'..='\u{06FF}').contains(&c)).filter(|tok| !tok.is_empty()).collect();
    for window in tokens.windows(2) {
        if let [num_tok, unit_tok] = window
            && let Ok(n) = num_tok.parse::<u32>()
            && (1..=1000).contains(&n)
            && matches!(*unit_tok, "pages" | "page" | "صفحة" | "صفحه" | "صفحات")
        {
            return Ok(Some(n));
        }
    }
    if crate::template_plan::is_stationery_or_planner_prompt(brief)
        && ["365", "سنوي", "سنوية", "annual", "yearly", "year-round"].iter().any(|kw| normalized.contains(kw))
    {
        return Ok(Some(365));
    }
    Ok(None)
}

pub(super) fn desired_continuation_pages(
    brief: &str,
    draft: Option<&designcraft_ai_types::protocol::ProductionDraft>,
) -> Result<Option<u32>, String> {
    if let Some(explicit) = extract_requested_page_count(brief)? {
        return Ok(Some(explicit));
    }
    Ok(draft.and_then(|d| d.progress.target_total_pages).filter(|p| (1..=1000).contains(p)))
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
            || layout_relevant_prefs_value(&job.prefs)? != layout_relevant_prefs_value(&session.prefs)?
        {
            return Ok(());
        }
        // Native selection/navigation may bump the session revision. Rebind only
        // the exact still-pinned document allocation in memory (no disk I/O on selection clicks).
        let mut snapshot = job.snapshot.clone();
        snapshot.document_revision = Some(current.revision);
        if let Some(job) = &mut self.batch {
            job.snapshot = snapshot.clone();
        }
        self.snapshot.batch = Some(snapshot);
        Ok(())
    }

    pub(super) fn sync_failed_batch_settings(&mut self, settings: &AiSettings) -> Result<(), String> {
        if self.pending.is_some() {
            return Ok(());
        }
        let Some(job) = self.batch.as_mut().filter(|job| job.snapshot.status == "failed") else {
            return Ok(());
        };
        if let Some(ledger) = job.ledger.as_mut() {
            ledger.update_budget(settings.budget.clone(), settings.image_rates.as_ref())?;
            job.snapshot.cumulative_usage = ledger.summary();
        }
        job.task.settings.budget = settings.budget.clone();
        job.task.settings.text_model = settings.text_model.clone();
        job.task.settings.image_model = settings.image_model.clone();
        job.task.settings.output_mode = settings.output_mode;
        job.task.settings.visual_density = settings.visual_density;
        job.task.settings.execution_pacing = settings.execution_pacing;
        job.task.settings.verify_generated_images = settings.verify_generated_images;
        job.task.settings.allow_paid_repairs = settings.allow_paid_repairs;
        job.task.settings.image_rates = settings.image_rates.clone();
        job.task.settings.comfy_workflow = settings.comfy_workflow.clone();
        job.task.settings.resources = settings.resources.clone();
        self.snapshot.batch = Some(job.snapshot.clone());
        Ok(())
    }

    pub(super) fn prepare_continuable_book(
        &mut self,
        session: &mut Session,
        task: &TaskRequest,
        profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<(), String> {
        let (current_pages, all_blank, has_holes, requested_trim_conflicts, current_facing) = {
            let current = session.doc().map_err(|e| e.to_string())?;
            let first_blank = current.doc.spreads.iter().position(|s| s.items.is_empty());
            let has_holes = first_blank.is_some_and(|first| current.doc.spreads.iter().skip(first).any(|s| !s.items.is_empty()));
            let all_blank = current.doc.spreads.iter().all(|s| s.items.is_empty());
            let current_pages = u32::try_from(current.doc.page_count()).map_err(|_| "Invalid existing book page count")?;
            let requested_trim = crate::print_contract::PrintContract::from_prompt(&task.prompt)?.and_then(|c| c.page_mm);
            let requested_trim_conflicts = requested_trim.is_some_and(|[w, h]| {
                current
                    .doc
                    .spreads
                    .iter()
                    .any(|sp| sp.pages.iter().any(|pg| (pg.width - w * 72.0 / 25.4).abs() > 0.1 || (pg.height - h * 72.0 / 25.4).abs() > 0.1))
            });
            (current_pages, all_blank, has_holes, requested_trim_conflicts, current.doc.settings.facing_pages)
        };
        if has_holes {
            return Ok(());
        }
        let desired_pages = desired_continuation_pages(&task.prompt, self.snapshot.production_draft.as_ref())?;
        if all_blank {
            let target_pages = desired_pages.unwrap_or(current_pages).clamp(1, 1000);
            let needs_setup = profile.is_some() || requested_trim_conflicts || target_pages != current_pages;
            if needs_setup {
                ensure_publication_idle(session)?;
                let mut setup = production_document_params(profile, target_pages, &task.settings, &task.prompt)?;
                if setup.get("width").is_none() && setup.get("preset").and_then(Value::as_str) == Some("A4") {
                    setup["width"] = json!(210.0 * 72.0 / 25.4);
                    setup["height"] = json!(297.0 * 72.0 / 25.4);
                }
                setup["facingPages"] = json!(current_facing);
                session.execute("layout.documentSetup", &setup).map_err(|e| e.to_string())?;
                let after_setup_pages = session.doc().map_err(|e| e.to_string())?.doc.page_count();
                if after_setup_pages < target_pages as usize {
                    session
                        .execute("layout.pages.insert", &json!({"count": (target_pages as usize).saturating_sub(after_setup_pages)}))
                        .map_err(|e| e.to_string())?;
                }
                initialize_print_document(session, &task.prompt)?;
                crate::template_plan::initialize_relational_book_master(session, profile, &task.settings, &task.prompt)?;
            }
        } else if !requested_trim_conflicts && let Some(want) = desired_pages.filter(|want| *want > current_pages && *want <= 1000) {
            ensure_publication_idle(session)?;
            session.execute("layout.pages.insert", &json!({"count": want - current_pages})).map_err(|e| e.to_string())?;
        }
        let updated_total = u32::try_from(session.doc().map_err(|e| e.to_string())?.doc.page_count()).ok();
        if let Some(mut draft) = self.snapshot.production_draft.clone()
            && draft.progress.target_total_pages != updated_total
        {
            draft.progress.target_total_pages = updated_total;
            let _ = self.persist_production_draft(&draft);
            self.snapshot.production_draft = Some(draft);
        }
        Ok(())
    }

    pub(super) fn project_continuable_preview_base(
        &self,
        base: &DocState,
        prefs: &Prefs,
        task: &TaskRequest,
        profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<DocState, String> {
        let first_blank = base.doc.spreads.iter().position(|s| s.items.is_empty());
        let has_holes = first_blank.is_some_and(|first| base.doc.spreads.iter().skip(first).any(|s| !s.items.is_empty()));
        if has_holes {
            return Ok(base.clone());
        }
        let all_blank = base.doc.spreads.iter().all(|s| s.items.is_empty());
        let current_pages = u32::try_from(base.doc.page_count()).map_err(|_| "Invalid existing book page count")?;
        let desired_pages = desired_continuation_pages(&task.prompt, self.snapshot.production_draft.as_ref())?;
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        if all_blank {
            let target_pages = desired_pages.unwrap_or(current_pages).clamp(1, 1000);
            let mut setup = production_document_params(profile, target_pages, &task.settings, &task.prompt)?;
            if setup.get("width").is_none() && setup.get("preset").and_then(Value::as_str) == Some("A4") {
                setup["width"] = json!(210.0 * 72.0 / 25.4);
                setup["height"] = json!(297.0 * 72.0 / 25.4);
            }
            setup["facingPages"] = json!(false);
            scratch.execute("layout.documentSetup", &setup).map_err(|e| e.to_string())?;
            let after_setup_pages = scratch.doc().map_err(|e| e.to_string())?.doc.page_count();
            if after_setup_pages < target_pages as usize {
                scratch
                    .execute("layout.pages.insert", &json!({"count": (target_pages as usize).saturating_sub(after_setup_pages)}))
                    .map_err(|e| e.to_string())?;
            }
        } else if let Some(want) = desired_pages.filter(|want| *want > current_pages && *want <= 1000) {
            scratch.execute("layout.pages.insert", &json!({"count": want - current_pages})).map_err(|e| e.to_string())?;
        }
        Ok(scratch.doc().map_err(|e| e.to_string())?.clone())
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
                let (pages, completed) =
                    continuation_or_blank_progress(&base, skills.iter().find_map(|skill| skill.design_profile.as_ref()), &task.prompt)?;
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
        let spread_aligned = completed == 0 && task.settings.facing_pages && size == 2 && pages > 1;
        let total_batches =
            if spread_aligned { 1_u32.saturating_add(pages.saturating_sub(1).div_ceil(2)) } else { pages.saturating_sub(completed).div_ceil(size) };
        let snapshot = BatchSnapshot {
            job_id: id,
            total_pages: pages,
            pages_per_batch: size,
            spread_aligned,
            current_batch: 1,
            total_batches,
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
        let expected_doc = job.publication_base.as_ref().map(|st| st.doc.clone());
        self.pending = Some(Pending {
            roots: job.roots.clone(),
            worker,
            id,
            started: Instant::now(),
            task: Some(task),
            expected,
            expected_doc,
            expected_prefs: None,
            skills: pins,
            settings,
            deferred_outcome: None,
            deferred_batch: None,
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
        crate::runtime::require_text_model(&job.task.settings)?;
        crate::runtime::require_generation_models(&job.task.settings)?;
        ensure_publication_idle(session)?;
        let profile = job.skills.iter().find_map(|skill| skill.design_profile.as_ref());
        let (base, mut context) = if let Some(uid) = job.snapshot.document_uid {
            let current = session.doc().map_err(|e| e.to_string())?;
            if current.uid != uid {
                return Err("Select this failed task's book before Retry; another open document will not be modified".into());
            }
            let (base, context) = task_document_context(session, &TaskTarget::ContinueBook, profile, &job.task.prompt, &job.task.settings)?;
            let (total, completed) = continuation_or_blank_progress(&base, profile, &job.task.prompt)?;
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
            && (target_doc.interaction.is_some() || target_doc.editing_parents || target_doc.preview_stash.is_some())
        {
            return Err("Finish the current interaction, IME composition or preview, or parent-page edit before publishing AI production".to_string());
        }
        let (target_index, next_launch) = {
            let job_ref = self.batch.as_ref().ok_or("Batch checkpoint disappeared")?;
            if !Arc::ptr_eq(&job_ref.publication_cache, &session.cache) {
                return Err("Font cache changed after proof generation; approval is invalid".to_string());
            }
            if layout_relevant_prefs_value(&session.prefs)? != layout_relevant_prefs_value(&job_ref.prefs)? {
                return Err("Application preferences changed after proof generation; approval is invalid".to_string());
            }
            let Some(target_index) = session.documents().iter().position(|doc| Some(doc.uid) == job_ref.snapshot.document_uid) else {
                return Err(
                    "The book changed after proof generation; approval is invalid. Start a new task against the corrected document".to_string()
                );
            };
            let current = session.documents().get(target_index).ok_or("Approved book disappeared")?;
            if Some(current.revision) != job_ref.snapshot.document_revision {
                return Err(
                    "The book changed after proof generation; approval is invalid. Start a new task against the corrected document".to_string()
                );
            }
            let proof = job_ref.snapshot.batches.last().ok_or("Batch proof is unavailable")?;
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
            let next_launch = if job_ref.snapshot.completed_pages < job_ref.snapshot.total_pages {
                let target_doc = session.documents().get(target_index).ok_or("Approved book disappeared")?;
                let mut base = crate::facing_pages::canonicalize(target_doc, &session.prefs)?;
                base.selection = Default::default();
                let mut projected = Session::new();
                projected.prefs = session.prefs.clone();
                projected.add_document(base.clone());
                let mut next_job_view = job_ref.clone();
                if let Some(last) = next_job_view.snapshot.batches.last_mut() {
                    last.status = "approved".into();
                }
                let context = batch_context(&projected, &next_job_view, &self.root)?;
                let next_batch = job_ref.snapshot.current_batch.checked_add(1).ok_or("Batch index overflow")?;
                Some((base, context, next_batch))
            } else {
                None
            };
            (target_index, next_launch)
        };
        let mut job = self.batch.take().ok_or("Batch checkpoint disappeared")?;
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
        let previous_active = session.active_index();
        if previous_active != Some(target_index) {
            session.set_active(target_index);
        }
        let Some((base, context, next_batch)) = next_launch else {
            // Final proof approval is deliberately a local transition: no worker or provider call.
            let exported = file_access::confine(job.roots.as_ref(), || self.export_finished_batch(session, &job.task, &job.snapshot.job_id));
            let paths = match exported {
                Ok(paths) => paths,
                Err(export_err) => {
                    if let Some(prev) = previous_active
                        && session.active_index() != Some(prev)
                    {
                        session.set_active(prev);
                    }
                    self.snapshot.reports = unapproved_reports;
                    self.snapshot.batch = Some(unapproved_job.snapshot.clone());
                    self.snapshot.stage = "awaiting_human_proof".into();
                    let _ = self.persist_batch(&unapproved_job.snapshot);
                    let _ = write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports);
                    self.batch = Some(unapproved_job);
                    return Err(export_err);
                }
            };
            job.snapshot.status = "completed".into();
            if let Some(report) = self.snapshot.reports.iter_mut().find(|report| report.id == approved_id) {
                report.artifacts.extend(paths);
            }
            if let Err(io_err) = self
                .persist_batch(&job.snapshot)
                .and_then(|()| write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports))
            {
                if let Some(prev) = previous_active
                    && session.active_index() != Some(prev)
                {
                    session.set_active(prev);
                }
                self.snapshot.reports = unapproved_reports;
                self.snapshot.batch = Some(unapproved_job.snapshot.clone());
                self.snapshot.stage = "awaiting_human_proof".into();
                let _ = self.persist_batch(&unapproved_job.snapshot);
                let _ = write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports);
                self.batch = Some(unapproved_job);
                return Err(io_err);
            }
            self.snapshot.stage = "completed".into();
            self.snapshot.message = "All batches approved; no additional provider request was sent".into();
            self.snapshot.batch = Some(job.snapshot);
            return Ok(());
        };
        job.snapshot.current_batch = next_batch;
        // Consumes this exact approval before any next request is launched.
        job.snapshot.status = "running".into();
        self.launch_batch(job, base, context)
    }

    pub(super) fn pause_batch(&mut self, id: &str) -> Result<(), String> {
        let job = self.batch.as_mut().ok_or("No active production batch job to pause")?;
        if job.snapshot.job_id != id {
            return Err("Pause request does not match the active production batch job".into());
        }
        job.snapshot.paused = true;
        let snap = job.snapshot.clone();
        self.persist_batch(&snap)?;
        self.snapshot.batch = Some(snap);
        self.snapshot.message = if self.pending.is_some() {
            "Pausing cleanly after the current batch finishes; in-flight work will be published and saved.".into()
        } else {
            "Production batch job paused; click Resume to continue.".into()
        };
        Ok(())
    }

    pub(super) fn resume_batch(&mut self, session: &mut Session, id: &str) -> Result<(), String> {
        let (checkpoint, is_waiting) = {
            let job = self.batch.as_mut().ok_or("No active production batch job to resume")?;
            if job.snapshot.job_id != id {
                return Err("Resume request does not match the active production batch job".into());
            }
            job.snapshot.paused = false;
            let snap = job.snapshot.clone();
            let waiting = snap.status == "awaiting_human_proof" && self.pending.is_none();
            let cp = snap.checkpoint;
            let _ = job;
            self.persist_batch(&snap)?;
            self.snapshot.batch = Some(snap);
            (cp, waiting)
        };
        if is_waiting {
            self.approve_batch(session, id, checkpoint)
        } else {
            self.snapshot.message = "Autonomous continuation resumed; the next batch will launch automatically.".into();
            Ok(())
        }
    }

    pub(super) fn refresh_batch_proofs(&mut self, session: &mut Session, id: &str) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("A provider request is already active".into());
        }
        let job_view = self.batch.as_ref().ok_or("No live batch job is awaiting human proof review; recovered checkpoints cannot resume")?;
        if job_view.snapshot.job_id != id || job_view.snapshot.status != "awaiting_human_proof" {
            return Err("Batch job is not awaiting human proof review".into());
        }
        ensure_publication_idle(session)?;
        if !Arc::ptr_eq(&job_view.publication_cache, &session.cache) {
            return Err("Font cache changed after proof generation; start a new task against the current document".into());
        }
        if layout_relevant_prefs_value(&session.prefs)? != layout_relevant_prefs_value(&job_view.prefs)? {
            return Err("Application preferences changed after proof generation; start a new task against the current document".into());
        }
        let Some(target_uid) = job_view.snapshot.document_uid else {
            return Err("Batch proof document identifier is unavailable".into());
        };
        let Some(current) = session.documents().iter().find(|doc| doc.uid == target_uid).cloned() else {
            return Err("Target book is no longer open; reopen it or cancel the batch".into());
        };
        if current.interaction.is_some() || current.editing_parents || current.preview_stash.is_some() {
            return Err("Finish the current interaction, IME composition or preview, or parent-page edit before refreshing batch proofs".into());
        }
        let Some(baseline) = job_view.publication_base.as_ref() else {
            return Err("Batch proof baseline is unavailable; recovered checkpoints cannot refresh proofs".into());
        };
        let last = job_view
            .snapshot
            .batches
            .last()
            .filter(|batch| batch.status == "awaiting_human_proof" && batch.page_count > 0)
            .ok_or("Current batch proof report is unavailable")?;
        let scope = worker::BatchScope { start: last.page_start, count: last.page_count };
        verify_refresh_scope_authority(baseline, &current, scope, job_view.snapshot.completed_pages, job_view.snapshot.total_pages)?;
        if job_view.snapshot.completed_pages < job_view.snapshot.total_pages {
            let _ = crate::facing_pages::canonicalize(&current, &session.prefs)?;
        }
        let settings = job_view.task.settings.clone();
        let prompt = job_view.task.prompt.clone();
        let roots = job_view.roots.clone();
        let current_batch = job_view.snapshot.current_batch;
        let retry_sequence = job_view.retry_sequence;
        let job_id = job_view.snapshot.job_id.clone();
        let report_id = batch_report_id(job_view);
        let (proofs, issues) = file_access::confine(roots.as_ref(), || {
            worker::render_live_batch_proofs(&current, &session.prefs, &session.cache, &settings, &prompt, scope)
        })?;
        validate_batch_proof_set(&proofs, scope)?;
        let directory_name =
            if retry_sequence == 0 { format!("batch-{current_batch}") } else { format!("batch-{current_batch}-retry-{retry_sequence}") };
        let directory = self.root.join("runs").join(&job_id).join(directory_name);
        self.profile_access.check(&directory, file_access::Access::Write)?;
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let mut new_paths = Vec::with_capacity(proofs.len());
        let mut new_sha256 = Vec::with_capacity(proofs.len());
        for proof in proofs {
            let path = directory.join(proof.name);
            write_profile(&self.profile_access, &path, &proof.bytes)?;
            new_sha256.push(hash(&proof.bytes));
            new_paths.push(path.to_string_lossy().into_owned());
        }
        let job = self.batch.as_mut().ok_or("Batch checkpoint disappeared")?;
        let next_checkpoint = job.snapshot.checkpoint.checked_add(1).ok_or("Batch checkpoint overflow")?;
        if let Some(last_mut) = job.snapshot.batches.last_mut() {
            last_mut.proof_paths = new_paths.clone();
            last_mut.proof_sha256 = new_sha256;
        }
        job.snapshot.document_revision = Some(current.revision);
        job.snapshot.checkpoint = next_checkpoint;
        job.publication_base = Some(current);
        let updated_snapshot = job.snapshot.clone();
        if let Some(report) = self.snapshot.reports.iter_mut().rev().find(|report| report.id == report_id) {
            let trace = directory.join("trace.json").to_string_lossy().into_owned();
            let had_trace = report.artifacts.iter().any(|p| p == &trace);
            report.artifacts = new_paths;
            if had_trace {
                report.artifacts.push(trace);
            }
            report.issues = issues.clone();
            report.message = "Refreshed batch proofs after native document edits; human approval is required".into();
        }
        self.persist_batch(&updated_snapshot)?;
        write_json(&self.profile_access, &self.root.join("history.json"), &self.snapshot.reports)?;
        if session.doc().is_ok_and(|doc| doc.uid == target_uid) {
            let _ = self.bind_production_draft(session, &job_id);
        }
        self.snapshot.current_issues = issues;
        self.snapshot.batch = Some(updated_snapshot);
        self.snapshot.stage = "awaiting_human_proof".into();
        self.snapshot.message = "Refreshed batch proofs for edited document; review every page proof before continuing".into();
        Ok(())
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
        let mut auto_next_launch: Option<(DocState, Value, u32)> = None;
        match result {
            Ok(()) => {
                job.snapshot.completed_pages = job.snapshot.completed_pages.saturating_add(batch_report.page_count);
                job.snapshot.checkpoint = job.snapshot.checkpoint.saturating_add(1);
                let draft_note = self.bind_production_draft(session, &job.snapshot.job_id).err();
                let pacing_continues = job.task.settings.execution_pacing == ExecutionPacing::AutoContinue
                    || (job.task.settings.execution_pacing == ExecutionPacing::StopAfterFirstBatch && job.snapshot.current_batch > 1);
                if !job.snapshot.paused && pacing_continues && job.snapshot.completed_pages >= job.snapshot.total_pages {
                    match file_access::confine(job.roots.as_ref(), || self.export_finished_batch(session, &job.task, &job.snapshot.job_id)) {
                        Ok(paths) => {
                            report.artifacts.extend(paths);
                            job.snapshot.status = "completed".into();
                            batch_report.status = "approved".into();
                            report.status = "completed".into();
                            report.message = format!(
                                "Autonomous production completed all {} pages across {} batches.",
                                job.snapshot.total_pages, job.snapshot.current_batch
                            );
                        }
                        Err(export_err) => {
                            job.snapshot.status = "awaiting_human_proof".into();
                            batch_report.status = "awaiting_human_proof".into();
                            report.status = "awaiting_human_proof".into();
                            report.message = format!(
                                "All {} pages were published, but automatic export failed ({export_err}); review and approve to finish.",
                                job.snapshot.total_pages
                            );
                        }
                    }
                } else if !job.snapshot.paused && pacing_continues && job.snapshot.completed_pages < job.snapshot.total_pages {
                    let prep = (|| -> Result<(DocState, Value, u32), String> {
                        let target_doc = session
                            .documents()
                            .iter()
                            .find(|doc| Some(doc.uid) == job.snapshot.document_uid)
                            .ok_or_else(|| "Published book disappeared before continuing to the next batch".to_string())?;
                        let mut next_base = crate::facing_pages::canonicalize(target_doc, &session.prefs)?;
                        next_base.selection = Default::default();
                        let mut projected = Session::new();
                        projected.prefs = session.prefs.clone();
                        projected.add_document(next_base.clone());
                        let mut next_job_view = job.clone();
                        let mut approved_entry = batch_report.clone();
                        approved_entry.status = "approved".into();
                        next_job_view.snapshot.batches.push(approved_entry);
                        let next_context = batch_context(&projected, &next_job_view, &self.root)?;
                        let next_batch = job.snapshot.current_batch.checked_add(1).ok_or_else(|| "Batch index overflow".to_string())?;
                        Ok((next_base, next_context, next_batch))
                    })();
                    match prep {
                        Ok(next_tuple) => {
                            job.snapshot.status = "running".into();
                            batch_report.status = "approved".into();
                            report.status = "approved".into();
                            report.message = format!(
                                "Batch {} of {} published ({}/{} pages); continuing automatically…",
                                job.snapshot.current_batch, job.snapshot.total_batches, job.snapshot.completed_pages, job.snapshot.total_pages
                            );
                            auto_next_launch = Some(next_tuple);
                        }
                        Err(prep_err) => {
                            job.snapshot.status = "awaiting_human_proof".into();
                            batch_report.status = "awaiting_human_proof".into();
                            report.status = "awaiting_human_proof".into();
                            report.message = format!("Batch published; automatic continuation paused ({prep_err})");
                        }
                    }
                } else {
                    job.snapshot.status = "awaiting_human_proof".into();
                    batch_report.status = "awaiting_human_proof".into();
                    report.status = "awaiting_human_proof".into();
                    report.message = if job.snapshot.paused {
                        format!(
                            "Paused cleanly after batch {} ({}/{} pages). Click Resume to continue autonomous generation.",
                            job.snapshot.current_batch, job.snapshot.completed_pages, job.snapshot.total_pages
                        )
                    } else if job.task.settings.execution_pacing == ExecutionPacing::StopAfterFirstBatch && job.snapshot.current_batch == 1 {
                        "First batch published for style check. Approve once to continue the remaining book automatically.".into()
                    } else {
                        "Batch published with native/PDF/PNG proofs. Human approval is required before any subsequent provider request".into()
                    };
                }
                if let Some(error) = draft_note {
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
            auto_next_launch = None;
            job.snapshot.status = "failed".into();
            self.snapshot.stage = "failed".into();
            self.snapshot.message = format!("Batch evidence could not be persisted; continuation is disabled: {error}");
            self.snapshot.batch = Some(job.snapshot.clone());
        }
        if let Some((next_base, next_context, next_batch)) = auto_next_launch {
            job.snapshot.current_batch = next_batch;
            job.snapshot.status = "running".into();
            if let Err(launch_err) = self.launch_batch(job, next_base, next_context) {
                self.snapshot.stage = "failed".into();
                self.snapshot.message = launch_err;
            }
            return;
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
        if layout_relevant_prefs_value(&session.prefs)? != layout_relevant_prefs_value(&job.prefs)? {
            return Err("Application preferences changed during generation; proof no longer matches publication".into());
        }
        let first_plan = plans.first().ok_or("A review group must contain native plans")?;
        let actions = plans.iter().try_fold(0usize, |total, plan| total.checked_add(plan.commands.len())).ok_or("Review action count overflow")?;
        validate_batch_proof_set(&proofs, scope)?;
        let first = job.snapshot.document_uid.is_none();
        let previous = session.active_index();
        if first {
            let profile = job.skills.iter().find_map(|skill| skill.design_profile.as_ref());
            session
                .execute("file.new", &production_document_params(profile, job.snapshot.total_pages, &job.task.settings, &job.task.prompt)?)
                .map_err(|e| e.to_string())?;
            initialize_print_document(session, &job.task.prompt)?;
            crate::template_plan::initialize_relational_book_master(session, profile, &job.task.settings, &job.task.prompt)?;
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
            let desired_rtl = book_binding(&job.task.settings, &job.task.prompt);
            let change_layout = job.task.settings.facing_pages && !base.doc.settings.facing_pages;
            let change_single_binding = !job.task.settings.facing_pages
                && !base.doc.settings.facing_pages
                && base.doc.spreads.iter().all(|s| s.items.is_empty())
                && base.doc.settings.right_to_left_binding != desired_rtl;
            let setup_projection = if change_layout {
                Some(crate::facing_pages::formatted_proof_state(&base, &job.prefs, desired_rtl)?)
            } else if change_single_binding {
                Some(crate::facing_pages::single_page_proof_state(&base, &job.prefs, desired_rtl)?)
            } else {
                None
            };
            let projected_base = setup_projection.as_ref().unwrap_or(&base);
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
            if change_layout || change_single_binding {
                group_params["documentSetup"] = json!({
                    "facingPages": job.task.settings.facing_pages,
                    "binding": if desired_rtl { "rightToLeft" } else { "leftToRight" }
                });
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
            let run_root = self.root.join("runs").join(&job.snapshot.job_id);
            let directory = run_root.join(directory_name);
            self.profile_access.check(&directory, file_access::Access::Write)?;
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let next_completed = job.snapshot.completed_pages.saturating_add(batch_report.page_count);
            let pacing_continues = job.task.settings.execution_pacing == ExecutionPacing::AutoContinue
                || (job.task.settings.execution_pacing == ExecutionPacing::StopAfterFirstBatch && job.snapshot.current_batch > 1);
            let will_auto_continue = !job.snapshot.paused && pacing_continues && next_completed < job.snapshot.total_pages;
            for proof in proofs {
                if will_auto_continue && proof.name == "proof.pdf" {
                    continue;
                }
                if will_auto_continue && proof.name == "document.designcraft" {
                    let rolling = run_root.join("latest-checkpoint.designcraft");
                    write_profile(&self.profile_access, &rolling, &proof.bytes)?;
                    continue;
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
    pub(super) fn persist_batch(&self, snapshot: &BatchSnapshot) -> Result<(), String> {
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
fn validate_batch_proof_set(proofs: &[worker::ProofArtifact], scope: worker::BatchScope) -> Result<(), String> {
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
    for proof in proofs {
        let extension = Path::new(&proof.name).extension().and_then(|x| x.to_str());
        if proof.name.contains(['/', '\\', ':']) || !matches!(extension, Some("png" | "pdf" | "designcraft")) || !names.insert(proof.name.as_str()) {
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
    Ok(())
}

fn page_geometry_matches(a: &designcraft_engine::doc::Page, b: &designcraft_engine::doc::Page) -> bool {
    a.id == b.id
        && a.width == b.width
        && a.height == b.height
        && a.x == b.x
        && a.margins == b.margins
        && a.columns == b.columns
        && a.local_margins == b.local_margins
        && a.parent == b.parent
        && a.side == b.side
        && a.overridden == b.overridden
        && a.show_parent_items == b.show_parent_items
        && a.liquid == b.liquid
}

fn rect_overlaps_interior(a: designcraft_geom::Rect, b: designcraft_geom::Rect) -> bool {
    const EPS: f64 = 1e-3;
    a.x0 + EPS < b.x1 && a.x1 - EPS > b.x0 && a.y0 + EPS < b.y1 && a.y1 - EPS > b.y0
}

fn validate_refresh_item_trees(state: &DocState) -> Result<(), String> {
    const MAX_ITEMS: usize = 10_000;
    const MAX_DEPTH: usize = 64;
    let mut visited = 0usize;
    let mut ids = std::collections::BTreeSet::new();
    for spread in state.doc.spreads.iter().chain(&state.doc.parents) {
        if spread.items.len() > MAX_ITEMS {
            return Err("Spread item count exceeds batch proof inspection limit".into());
        }
        let mut pending: Vec<(&designcraft_engine::doc::Item, usize)> = spread.items.iter().map(|item| (item.as_ref(), 0usize)).collect();
        while let Some((item, depth)) = pending.pop() {
            visited = visited.checked_add(1).ok_or("Item count overflow")?;
            if visited > MAX_ITEMS || depth > MAX_DEPTH || !ids.insert(item.id) {
                return Err("Item tree exceeds batch proof inspection limit or repeats an item ID".into());
            }
            let children = item.children();
            if pending.len().checked_add(children.len()).is_none_or(|n| n > MAX_ITEMS) {
                return Err("Nested item tree exceeds batch proof inspection limit".into());
            }
            for child in children {
                pending.push((child.as_ref(), depth.saturating_add(1)));
            }
        }
    }
    Ok(())
}

fn collect_item_story_and_asset_refs(
    item: &designcraft_engine::doc::Item,
    stories: &mut std::collections::BTreeSet<designcraft_engine::doc::StoryId>,
    assets: &mut std::collections::BTreeSet<designcraft_engine::doc::AssetId>,
    depth: usize,
) -> Result<(), String> {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        return Err("Nested group depth exceeds batch proof inspection limit".into());
    }
    match &item.content {
        designcraft_engine::doc::Content::Text(tf) => {
            stories.insert(tf.story);
        }
        designcraft_engine::doc::Content::Graphic(g) => {
            assets.insert(g.asset);
        }
        designcraft_engine::doc::Content::Group { items } => {
            for child in items {
                collect_item_story_and_asset_refs(child, stories, assets, depth.saturating_add(1))?;
            }
        }
        designcraft_engine::doc::Content::Unassigned => {}
    }
    if let Some(poster) = item.media.as_ref().and_then(|m| m.poster) {
        assets.insert(poster);
    }
    Ok(())
}

fn collect_story_sub_refs(
    story: &designcraft_engine::doc::Story,
    nested_stories: &mut std::collections::BTreeSet<designcraft_engine::doc::StoryId>,
    assets: &mut std::collections::BTreeSet<designcraft_engine::doc::AssetId>,
    depth: usize,
) -> Result<(), String> {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        return Err("Nested story depth exceeds batch proof inspection limit".into());
    }
    if let Some((linked_id, _)) = story.link {
        nested_stories.insert(linked_id);
    }
    for anchored in &story.objects {
        collect_item_story_and_asset_refs(&anchored.item, nested_stories, assets, depth.saturating_add(1))?;
    }
    for table in story.tables.values() {
        for cell in &table.cells {
            if let Some(g) = &cell.graphic {
                assets.insert(g.asset);
            }
            collect_story_sub_refs(&cell.text, nested_stories, assets, depth.saturating_add(1))?;
        }
    }
    for note in story.notes.iter().chain(&story.endnotes) {
        collect_story_sub_refs(&note.text, nested_stories, assets, depth.saturating_add(1))?;
    }
    Ok(())
}

fn expand_story_transitive_refs(
    doc: &designcraft_engine::doc::Document,
    stories: &mut std::collections::BTreeSet<designcraft_engine::doc::StoryId>,
    assets: &mut std::collections::BTreeSet<designcraft_engine::doc::AssetId>,
) -> Result<(), String> {
    const MAX_VISITED_STORIES: usize = 10_000;
    let mut queue: Vec<designcraft_engine::doc::StoryId> = stories.iter().copied().collect();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(story_id) = queue.pop() {
        if !visited.insert(story_id) {
            continue;
        }
        if visited.len() > MAX_VISITED_STORIES {
            return Err("Story reference graph exceeds batch proof inspection limit".into());
        }
        if let Some(story) = doc.stories.get(&story_id) {
            let mut nested_stories = std::collections::BTreeSet::new();
            collect_story_sub_refs(story, &mut nested_stories, assets, 0)?;
            for nested in nested_stories {
                if stories.insert(nested) {
                    queue.push(nested);
                }
            }
        }
    }
    Ok(())
}

fn append_only_named<T: PartialEq>(base: &[T], curr: &[T], name_of: impl Fn(&T) -> &str) -> bool {
    if curr.len() < base.len() || curr.get(..base.len()) != Some(base) {
        return false;
    }
    let mut seen: std::collections::BTreeSet<&str> = base.iter().map(&name_of).collect();
    curr.get(base.len()..).is_some_and(|extra| extra.iter().all(|item| !name_of(item).is_empty() && seen.insert(name_of(item))))
}

fn verify_sealed_swatches(base: &[designcraft_engine::doc::color::Swatch], curr: &[designcraft_engine::doc::color::Swatch]) -> Result<(), String> {
    if curr.len() < base.len() || curr.get(..base.len()) != Some(base) {
        return Err("Already-approved swatches cannot be modified, reordered or deleted before refreshing batch proofs".into());
    }
    let mut seen: std::collections::BTreeSet<&str> = base.iter().map(|s| s.name.as_str()).collect();
    if let Some(extra) = curr.get(base.len()..) {
        for swatch in extra {
            let hidden_gradient =
                !swatch.named && swatch.hidden && matches!(swatch.value, designcraft_engine::doc::color::SwatchValue::Gradient { .. });
            if !hidden_gradient && (swatch.name.is_empty() || !seen.insert(swatch.name.as_str())) {
                return Err("Appended swatch name collides with an already-approved swatch".into());
            }
        }
    }
    Ok(())
}

fn verify_sealed_styles(base: &designcraft_engine::doc::Styles, curr: &designcraft_engine::doc::Styles) -> Result<(), String> {
    if !append_only_named(&base.paragraph, &curr.paragraph, |s| s.name.as_str())
        || !append_only_named(&base.character, &curr.character, |s| s.name.as_str())
        || !append_only_named(&base.object, &curr.object, |s| s.name.as_str())
        || !append_only_named(&base.cell, &curr.cell, |s| s.name.as_str())
        || !append_only_named(&base.table, &curr.table, |s| s.name.as_str())
        || !append_only_named(&base.composite_fonts, &curr.composite_fonts, |s| s.name.as_str())
        || !append_only_named(&base.mojikumi_tables, &curr.mojikumi_tables, |s| s.name.as_str())
    {
        return Err("Already-approved styles or typography tables cannot be modified, reordered or deleted before refreshing batch proofs".into());
    }
    if !base.export_tags.iter().all(|(k, v)| curr.export_tags.get(k) == Some(v)) {
        return Err("Export tags on already-approved styles cannot be modified or removed".into());
    }
    let base_para_names: std::collections::BTreeSet<&str> = base.paragraph.iter().map(|s| s.name.as_str()).collect();
    let base_char_names: std::collections::BTreeSet<&str> = base.character.iter().map(|s| s.name.as_str()).collect();
    for key in curr.export_tags.keys() {
        if !base.export_tags.contains_key(key) {
            let targets_sealed = key.strip_prefix("p:").is_some_and(|name| base_para_names.contains(name))
                || key.strip_prefix("c:").is_some_and(|name| base_char_names.contains(name));
            if targets_sealed {
                return Err("Export tags cannot be added retroactively to already-approved styles".into());
            }
        }
    }
    Ok(())
}

fn verify_refresh_scope_authority(
    baseline: &DocState,
    current: &DocState,
    scope: worker::BatchScope,
    completed_pages: u32,
    total_pages: u32,
) -> Result<(), String> {
    validate_refresh_item_trees(baseline)?;
    validate_refresh_item_trees(current)?;
    let total = usize::try_from(total_pages).map_err(|_| "Invalid total page count")?;
    let completed = usize::try_from(completed_pages).map_err(|_| "Invalid completed page count")?;
    if baseline.uid != current.uid
        || baseline.doc.spreads.len() != current.doc.spreads.len()
        || baseline.doc.page_count() != total
        || current.doc.page_count() != total
    {
        return Err("Edited document page count no longer matches the active book batch".into());
    }
    if baseline.doc.settings != current.doc.settings
        || baseline.doc.parents != current.doc.parents
        || baseline.doc.layers != current.doc.layers
        || baseline.doc.sections != current.doc.sections
        || baseline.doc.font_scope != current.doc.font_scope
    {
        return Err("Document setup, layers, sections or parent spreads changed after proof generation; cannot refresh batch proofs".into());
    }
    let start = usize::try_from(scope.start).map_err(|_| "Invalid batch proof page start")?;
    let count = usize::try_from(scope.count).map_err(|_| "Invalid batch proof page count")?;
    let end = start.checked_add(count).filter(|end| count > 0 && *end <= completed && completed <= total).ok_or("Invalid batch proof page range")?;

    let has_sealed_authority = start > 0 || baseline.doc.parents.iter().any(|p| !p.items.is_empty());
    if has_sealed_authority {
        verify_sealed_swatches(&baseline.doc.swatches, &current.doc.swatches)?;
        verify_sealed_styles(&baseline.doc.styles, &current.doc.styles)?;
        if !append_only_named(&baseline.doc.stroke_styles, &current.doc.stroke_styles, |s| s.name.as_str()) {
            return Err("Already-approved stroke styles cannot be modified, reordered or deleted before refreshing batch proofs".into());
        }
        let mut normalized = current.doc.as_ref().clone();
        normalized.spreads = baseline.doc.spreads.clone();
        normalized.stories = baseline.doc.stories.clone();
        normalized.assets = baseline.doc.assets.clone();
        normalized.swatches = baseline.doc.swatches.clone();
        normalized.styles = baseline.doc.styles.clone();
        normalized.stroke_styles = baseline.doc.stroke_styles.clone();
        normalized.color_groups = baseline.doc.color_groups.clone();
        normalized.user_words = baseline.doc.user_words.clone();
        normalized.next_id = baseline.doc.next_id;
        normalized.modified = baseline.doc.modified;
        if &normalized != baseline.doc.as_ref() {
            return Err(
                "Document-wide print, note, variable, XML or interactive definitions cannot be modified after earlier batches are approved".into()
            );
        }
    }

    let mut sealed_stories = std::collections::BTreeSet::new();
    let mut sealed_assets = std::collections::BTreeSet::new();
    let mut scope_stories = std::collections::BTreeSet::new();
    let mut scope_assets = std::collections::BTreeSet::new();
    let mut scope_item_counts = vec![0usize; count];

    let mut absolute = 0usize;
    for (base_sp, curr_sp) in baseline.doc.spreads.iter().zip(&current.doc.spreads) {
        if base_sp.id != curr_sp.id
            || base_sp.parent != curr_sp.parent
            || base_sp.allow_shuffle != curr_sp.allow_shuffle
            || base_sp.pages.len() != curr_sp.pages.len()
        {
            return Err("Page geometry, margins, columns or parent overrides changed after proof generation; cannot refresh batch proofs".into());
        }
        for (offset, (pa, pb)) in base_sp.pages.iter().zip(&curr_sp.pages).enumerate() {
            let abs = absolute.checked_add(offset).ok_or("Batch proof page index overflow")?;
            if (start..end).contains(&abs) {
                if !page_geometry_matches(pa, pb) {
                    return Err(
                        "Page geometry, margins, columns or parent overrides changed after proof generation; cannot refresh batch proofs".into()
                    );
                }
            } else if pa != pb {
                return Err("Already-approved or unproduced pages outside the current review batch cannot be modified".into());
            }
        }

        let mut base_non_scope: Vec<(usize, &Arc<designcraft_engine::doc::Item>)> = Vec::new();
        for item in &base_sp.items {
            let offset = base_sp.page_at_x(item.bounds().center().x).ok_or("Baseline item has no owning page")?;
            let abs = absolute.checked_add(offset).ok_or("Baseline item page index overflow")?;
            if !(start..end).contains(&abs) {
                base_non_scope.push((abs, item));
                collect_item_story_and_asset_refs(item, &mut sealed_stories, &mut sealed_assets, 0)?;
            }
        }

        let mut curr_non_scope: Vec<(usize, &Arc<designcraft_engine::doc::Item>)> = Vec::new();
        for item in &curr_sp.items {
            let bounds = item.visible_bounds();
            if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|n| n.is_finite()) {
                return Err("Edited artwork has non-finite bounds".into());
            }
            let offset = curr_sp.page_at_x(item.bounds().center().x).ok_or("Edited item has no owning page")?;
            let abs = absolute.checked_add(offset).ok_or("Edited item page index overflow")?;
            if abs >= completed {
                return Err("Unproduced trailing pages must remain blank before refreshing batch proofs".into());
            }
            if (start..end).contains(&abs) {
                if let Some(slot) = scope_item_counts.get_mut(abs.saturating_sub(start)) {
                    *slot = slot.saturating_add(1);
                }
                for (page_offset, page) in curr_sp.pages.iter().enumerate() {
                    let page_abs = absolute.checked_add(page_offset).ok_or("Batch proof page index overflow")?;
                    if !(start..end).contains(&page_abs) && rect_overlaps_interior(bounds, page.bounds()) {
                        return Err("Edited batch artwork cannot overlap an already-approved or unproduced page on a shared spread".into());
                    }
                }
                collect_item_story_and_asset_refs(item, &mut scope_stories, &mut scope_assets, 0)?;
            } else {
                curr_non_scope.push((abs, item));
            }
        }

        if base_non_scope != curr_non_scope {
            return Err("Already-approved or unproduced pages outside the current review batch cannot be modified".into());
        }
        absolute = absolute.checked_add(base_sp.pages.len()).ok_or("Batch proof page count overflow")?;
    }

    if scope_item_counts.contains(&0) {
        return Err("Reviewed batch pages cannot be blank; keep native content on every batch page or cancel".into());
    }

    for parent in &baseline.doc.parents {
        for item in &parent.items {
            collect_item_story_and_asset_refs(item, &mut sealed_stories, &mut sealed_assets, 0)?;
        }
    }
    if has_sealed_authority {
        if let Some(sid) = baseline.doc.endnote_story {
            sealed_stories.insert(sid);
        }
        if let Some(sid) = baseline.doc.settings.primary_story {
            sealed_stories.insert(sid);
        }
    }
    expand_story_transitive_refs(&baseline.doc, &mut sealed_stories, &mut sealed_assets)?;
    for story_id in &sealed_stories {
        if baseline.doc.stories.get(story_id) != current.doc.stories.get(story_id) {
            return Err("Already-approved text stories from earlier batches or parent spreads cannot be modified".into());
        }
    }
    for asset_id in &sealed_assets {
        if baseline.doc.assets.get(asset_id) != current.doc.assets.get(asset_id) {
            return Err("Already-approved placed assets from earlier batches or parent spreads cannot be modified".into());
        }
    }
    expand_story_transitive_refs(&current.doc, &mut scope_stories, &mut scope_assets)?;
    if !sealed_stories.is_disjoint(&scope_stories) {
        return Err("Current review batch frames cannot thread into an already-approved story".into());
    }
    Ok(())
}

fn scope(snapshot: &BatchSnapshot) -> Result<worker::BatchScope, String> {
    let remaining = snapshot.total_pages.checked_sub(snapshot.completed_pages).ok_or("Invalid completed page count")?;
    let count = if snapshot.spread_aligned && snapshot.pages_per_batch == 2 && snapshot.completed_pages == 0 && snapshot.total_pages > 1 {
        1
    } else {
        remaining.min(snapshot.pages_per_batch)
    };
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

    fn complete_http_body(bytes: &[u8]) -> Result<Option<&[u8]>, &'static str> {
        let Some(sep) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
            return Ok(None);
        };
        let headers = std::str::from_utf8(&bytes[..sep]).map_err(|_| "Invalid mock HTTP header UTF-8")?;
        let length = headers
            .lines()
            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|s| s.trim().parse::<usize>().ok()))
            .ok_or("Missing mock Content-Length")?;
        Ok(bytes.get(sep + 4..sep + 4 + length))
    }

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
                Box::new(ledger),
            ))
            .unwrap();
        runner.pending = Some(Pending {
            roots: None,
            worker: worker::Worker { receiver, cancel: Arc::new(AtomicBool::new(false)) },
            id: format!("{}-batch-1", snapshot.job_id),
            started: Instant::now(),
            task: Some(request.clone()),
            expected: continuing.then_some((before.uid, before.revision)),
            expected_doc: continuing.then(|| before.doc.clone()),
            expected_prefs: None,
            skills: vec![],
            settings: request.settings,
            deferred_outcome: None,
            deferred_batch: None,
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
                    if let Some(body) = complete_http_body(&bytes)? {
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
                    if complete_http_body(&bytes)?.is_some() {
                        break;
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
            if change == "cancel" {
                assert!(runner.batch.is_none());
            } else {
                assert!(runner.batch.is_some(), "{change} validation refusal must preserve live batch authority");
                assert_eq!(rejected.stage, "awaiting_human_proof");
                assert_eq!(rejected.batch.as_ref().unwrap().status, "awaiting_human_proof");
            }
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
        assert!(runner.batch.is_some());
        assert_eq!(rejected.stage, "awaiting_human_proof");
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
            assert_eq!(failed.stage, "awaiting_human_proof");
            assert_eq!(failed.batch.as_ref().unwrap().status, "awaiting_human_proof");
            assert!(runner.batch.is_some(), "recoverable proof validation error must preserve self.batch");
            assert!(failed.message.contains("Batch proof bytes changed"));
            assert_eq!(session.active_index(), Some(1), "failed approval must not switch the user's active tab");
        }

        // 2. Post-validation continuity overflow (`current_batch == u32::MAX`) must keep `self.batch` and
        // `snapshot.batch` consistent (`Some` + `awaiting_human_proof`) and never switch active tab.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let (job_id, checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
            runner.batch.as_mut().unwrap().snapshot.current_batch = u32::MAX;
            session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
            assert_eq!(session.active_index(), Some(1));

            let failed = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
            assert_eq!(failed.stage, "awaiting_human_proof");
            assert_eq!(failed.batch.as_ref().unwrap().status, "awaiting_human_proof");
            assert!(runner.batch.is_some(), "self.batch and snapshot.batch must stay consistent");
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

    #[test]
    fn failed_batch_syncs_updated_budget_and_model_on_retry_while_retaining_cumulative_usage() {
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
                let request_json = loop {
                    if bytes.len() > 1024 * 1024 || Instant::now() >= deadline {
                        return Err("Mock request exceeded bounds".into());
                    }
                    let count = socket.read(&mut chunk).map_err(|e| e.to_string())?;
                    if count == 0 {
                        return Err("Mock request ended early".into());
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(slice) = complete_http_body(&bytes)? {
                        break serde_json::from_slice::<Value>(slice).map_err(|e| e.to_string())?;
                    }
                };
                let index = server_calls.fetch_add(1, Ordering::Relaxed);
                let expected_model = if index == 0 { "batch-regression" } else { "gemini-3.8-flash" };
                if request_json.get("model").and_then(Value::as_str) != Some(expected_model) {
                    return Err(format!("Unexpected model on call {index}: {:?}", request_json.get("model")));
                }
                let plan = json!({
                    "title": "Reviewed native book",
                    "commands": [
                        {"command": "frame.create", "params": {"spread": index, "rect": [50, 60, 200, 180], "content": "unassigned"}},
                        {"command": "object.label", "params": {"ids": ["$0.id"], "label": format!("page-{}", index + 1)}}
                    ],
                    "images": []
                });
                let (in_tok, out_tok) = if index == 0 { (18_000u64, 2_000u64) } else { (4_000u64, 1_000u64) };
                let reply = json!({
                    "done": true,
                    "message": {"content": plan.to_string()},
                    "prompt_eval_count": in_tok,
                    "eval_count": out_tok
                })
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
        let mut request = task(endpoint.clone(), 1);
        request.settings.budget.token_limit = 25_000;

        let started = runner.handle(&mut session, AiRequest::Start(request.clone()));
        assert!(started.busy, "{}", started.message);
        let first = wait(&mut runner, &mut session);
        assert_eq!(first.stage, "awaiting_human_proof", "{}", first.message);
        let checkpoint = first.batch.as_ref().unwrap();
        let job_id = checkpoint.job_id.clone();
        assert_eq!(checkpoint.cumulative_usage.requests, 1);
        assert_eq!(checkpoint.cumulative_usage.input_tokens, 18_000);
        assert_eq!(checkpoint.cumulative_usage.output_tokens, 2_000);

        // Approving batch 1 attempts batch 2, which fails immediately on max_requests == 1.
        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: checkpoint.checkpoint });
        if approved.busy {
            let _ = wait(&mut runner, &mut session);
        }
        let failed = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(failed.stage, "failed");
        let failed_batch = failed.batch.as_ref().unwrap();
        assert_eq!(failed_batch.status, "failed");
        assert!(failed_batch.retryable);
        assert_eq!(failed_batch.cumulative_usage.requests, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Retrying without UpdateSettings still fails and preserves cumulative usage.
        let retry_unchanged = runner.handle(&mut session, AiRequest::RetryBatch { job_id: job_id.clone() });
        if retry_unchanged.busy {
            let _ = wait(&mut runner, &mut session);
        }
        let after_unchanged = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(after_unchanged.stage, "failed");
        assert!(after_unchanged.message.to_ascii_lowercase().contains("request limit"), "msg={}", after_unchanged.message);
        assert_eq!(after_unchanged.batch.as_ref().unwrap().cumulative_usage.requests, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Raising max_requests alone (keeping token_limit = 25_000) proves prior 20_000 charged_tokens were retained.
        let mut partial_settings = request.settings.clone();
        partial_settings.budget.max_requests = 2;
        let saved_partial = runner.handle(&mut session, AiRequest::UpdateSettings(partial_settings));
        assert_eq!(saved_partial.batch.as_ref().unwrap().status, "failed");
        assert!(saved_partial.batch.as_ref().unwrap().retryable);
        let retry_token_blocked = runner.handle(&mut session, AiRequest::RetryBatch { job_id: job_id.clone() });
        if retry_token_blocked.busy {
            let _ = wait(&mut runner, &mut session);
        }
        let after_token_blocked = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(after_token_blocked.stage, "failed");
        assert!(after_token_blocked.message.to_ascii_lowercase().contains("token budget"), "msg={}", after_token_blocked.message);
        assert_eq!(after_token_blocked.batch.as_ref().unwrap().cumulative_usage.input_tokens, 18_000);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Saving an empty or incompatible model via UpdateSettings must be rejected on RetryBatch without losing retryability!
        let mut empty_model_settings = request.settings.clone();
        empty_model_settings.budget.max_requests = 2;
        empty_model_settings.budget.token_limit = 64_000;
        empty_model_settings.text_model.model.clear();
        runner.handle(&mut session, AiRequest::UpdateSettings(empty_model_settings));
        let empty_retry = runner.handle(&mut session, AiRequest::RetryBatch { job_id: job_id.clone() });
        assert_eq!(empty_retry.stage, "failed");
        assert!(empty_retry.message.contains("Choose a text model"), "msg={}", empty_retry.message);
        assert!(empty_retry.batch.as_ref().unwrap().retryable, "model readiness rejection must keep failed batch retryable");

        let mut incompatible_img_settings = request.settings.clone();
        incompatible_img_settings.budget.max_requests = 2;
        incompatible_img_settings.budget.token_limit = 64_000;
        incompatible_img_settings.text_model.model = "gemini-3.8-flash".into();
        incompatible_img_settings.output_mode = designcraft_ai_types::OutputMode::NativeWithImages;
        incompatible_img_settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        runner.handle(&mut session, AiRequest::UpdateSettings(incompatible_img_settings));
        let incompatible_retry = runner.handle(&mut session, AiRequest::RetryBatch { job_id: job_id.clone() });
        assert_eq!(incompatible_retry.stage, "failed");
        assert!(incompatible_retry.message.contains("non-image model"), "msg={}", incompatible_retry.message);
        assert!(incompatible_retry.batch.as_ref().unwrap().retryable);

        // Raising token_limit + max_requests and switching model (while passing a different pages_per_batch) recovers cleanly.
        let mut recovered_settings = request.settings.clone();
        recovered_settings.budget.max_requests = 2;
        recovered_settings.budget.token_limit = 64_000;
        recovered_settings.text_model.model = "gemini-3.8-flash".into();
        recovered_settings.pages_per_batch = 8;
        runner.handle(&mut session, AiRequest::UpdateSettings(recovered_settings));
        assert_eq!(runner.batch.as_ref().unwrap().task.settings.pages_per_batch, 1, "active job pagination must stay unchanged");

        let retried = runner.handle(&mut session, AiRequest::RetryBatch { job_id });
        assert!(retried.busy, "{}", retried.message);
        let second = wait(&mut runner, &mut session);
        assert_eq!(second.stage, "awaiting_human_proof", "{}", second.message);
        let done_batch = second.batch.as_ref().unwrap();
        assert_eq!(done_batch.completed_pages, 2);
        assert_eq!(done_batch.cumulative_usage.requests, 2);
        assert_eq!(done_batch.cumulative_usage.input_tokens, 22_000);
        assert_eq!(done_batch.cumulative_usage.output_tokens, 3_000);
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn failed_remote_batch_mid_job_cost_cap_rejects_missing_rates_atomically_and_enforces_backfilled_usd_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 2, "facingPages": false})).unwrap();
        session.execute("frame.create", &json!({"spread": 0, "rect": [40, 40, 200, 160]})).unwrap();
        let doc = session.active().unwrap().clone();

        let mut base_settings = AiSettings {
            text_model: ModelSelection {
                provider: ProviderKind::Gemini,
                model: "gemini-3.8-flash".into(),
                endpoint: Some("https://gemini.example.invalid:9/".into()),
            },
            pages_per_batch: 1,
            ..Default::default()
        };
        base_settings.budget.token_limit = 256_000;
        base_settings.budget.max_requests = 4;
        runner.handle(&mut session, AiRequest::UpdateSettings(base_settings.clone()));

        // Simulate a failed batch 2 where batch 1 ran remotely on Gemini without rates (10_000 input + 5_000 output tokens).
        let mut ledger = UsageLedger::new(base_settings.budget.clone()).unwrap();
        let r1 = ledger.reserve_with_output(3000, 0, ProviderKind::Gemini, false, None, Some(4096)).unwrap();
        ledger.record(r1, Some(&designcraft_ai_providers::Usage { input_tokens: Some(10_000), output_tokens: Some(5_000), ..Default::default() }));
        assert_eq!(ledger.summary().estimated_cost_usd, None);

        let task_req = TaskRequest {
            prompt: "Continue two-page book".into(),
            settings: base_settings.clone(),
            skill_ids: Vec::new(),
            auto_select_skills: false,
            target: TaskTarget::ContinueBook,
            export_directory: None,
        };
        runner.save_production_draft(&session, &task_req).unwrap();
        runner.bind_production_draft(&session, "job-usd-backfill").unwrap();

        let snapshot = BatchSnapshot {
            job_id: "job-usd-backfill".into(),
            total_pages: 2,
            pages_per_batch: 1,
            spread_aligned: false,
            current_batch: 2,
            total_batches: 2,
            completed_pages: 1,
            status: "failed".into(),
            paused: false,
            retryable: true,
            checkpoint: 1,
            document_uid: Some(doc.uid),
            document_revision: Some(doc.revision),
            cumulative_usage: ledger.summary(),
            elapsed_ms: 100,
            batches: vec![BatchReport { batch: 2, page_start: 1, page_count: 1, status: "failed".into(), ..Default::default() }],
        };
        runner.snapshot.batch = Some(snapshot.clone());
        runner.batch = Some(BatchJob {
            task: task_req,
            skills: Vec::new(),
            roots: None,
            ledger: Some(ledger),
            before: UsageSummary::default(),
            snapshot,
            prefs: session.prefs.clone(),
            publication_cache: session.cache.clone(),
            publication_base: Some(doc),
            retry_sequence: 0,
        });

        // 1. UpdateSettings with max_cost_usd but NO rates must fail BEFORE writing settings.json or production-draft.json!
        let disk_settings_before = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let disk_draft_before = std::fs::read_to_string(dir.path().join("production-draft.json")).unwrap();
        let mut missing_rates = base_settings.clone();
        missing_rates.budget.max_cost_usd = Some(0.10);
        missing_rates.text_model.model = "should-not-persist".into();
        let err_snap = runner.handle(&mut session, AiRequest::UpdateSettings(missing_rates));
        assert_eq!(err_snap.stage, "failed");
        assert!(err_snap.message.contains("Set rates"), "msg={}", err_snap.message);
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.json")).unwrap(), disk_settings_before);
        assert_eq!(std::fs::read_to_string(dir.path().join("production-draft.json")).unwrap(), disk_draft_before);
        assert_eq!(err_snap.settings.text_model.model, "gemini-3.8-flash");

        // 2. Supplying rates + tight max_cost_usd ($0.08) backfills prior $0.07 cost onto cumulative_usage,
        // and blocks RetryBatch BEFORE contacting the endpoint because next admission estimate exceeds remaining $0.01!
        let mut tight_usd = base_settings;
        tight_usd.budget.max_cost_usd = Some(0.08);
        tight_usd.budget.rates = Some(designcraft_ai_types::protocol::TokenRates {
            input_per_million: 2.0,
            cached_input_per_million: 0.5,
            output_per_million: 10.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: None,
        });
        runner.store_session_key(&tight_usd.text_model, SecretString::new("test-key".into())).unwrap();
        let saved_usd = runner.handle(&mut session, AiRequest::UpdateSettings(tight_usd));
        let backfilled_usd = saved_usd.batch.as_ref().unwrap().cumulative_usage.estimated_cost_usd.expect("backfilled USD");
        assert!((backfilled_usd - 0.07).abs() < 1e-10, "backfilled_usd={backfilled_usd}");

        let retry_blocked = runner.handle(&mut session, AiRequest::RetryBatch { job_id: "job-usd-backfill".into() });
        if retry_blocked.busy {
            let _ = wait(&mut runner, &mut session);
        }
        let after_blocked = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(after_blocked.stage, "failed");
        assert!(after_blocked.message.contains("monetary budget"), "msg={}", after_blocked.message);
    }

    #[test]
    fn ai28_spread_aligned_facing_batches_slice_cover_spreads_and_back_page() {
        let mut snap_32 = BatchSnapshot {
            job_id: "spread-32".into(),
            total_pages: 32,
            pages_per_batch: 2,
            spread_aligned: true,
            current_batch: 1,
            total_batches: 1 + 31_u32.div_ceil(2),
            completed_pages: 0,
            status: "running".into(),
            ..Default::default()
        };
        assert_eq!(snap_32.total_batches, 17);
        let mut slices = Vec::new();
        while snap_32.completed_pages < snap_32.total_pages {
            let s = scope(&snap_32).unwrap();
            slices.push((s.start, s.count));
            snap_32.completed_pages += s.count;
        }
        assert_eq!(slices.len(), 17);
        assert_eq!(slices[0], (0, 1));
        assert_eq!(slices[1], (1, 2));
        assert_eq!(slices[2], (3, 2));
        assert_eq!(slices[15], (29, 2));
        assert_eq!(slices[16], (31, 1));

        let mut snap_5 = BatchSnapshot {
            job_id: "spread-5".into(),
            total_pages: 5,
            pages_per_batch: 2,
            spread_aligned: true,
            current_batch: 1,
            total_batches: 1 + 4_u32.div_ceil(2),
            completed_pages: 0,
            status: "running".into(),
            ..Default::default()
        };
        assert_eq!(snap_5.total_batches, 3);
        let mut odd_slices = Vec::new();
        while snap_5.completed_pages < snap_5.total_pages {
            let s = scope(&snap_5).unwrap();
            odd_slices.push((s.start, s.count));
            snap_5.completed_pages += s.count;
        }
        assert_eq!(odd_slices, vec![(0, 1), (1, 2), (3, 2)]);

        // End-to-end start_batches sets spread_aligned=true and total_batches=4 on a 6-page facing-pages book with pages_per_batch=2.
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let mut req = task("http://127.0.0.1:9/".into(), 8);
        req.target = TaskTarget::NewA4 { pages: 6 };
        req.settings.facing_pages = true;
        req.settings.pages_per_batch = 2;
        let started = runner.handle(&mut session, AiRequest::Start(req));
        let batch = started.batch.as_ref().expect("batch started");
        assert!(batch.spread_aligned);
        assert_eq!(batch.total_batches, 4);
        let _ = runner.handle(&mut session, AiRequest::Cancel);
        let _ = wait(&mut runner, &mut session);
    }

    #[test]
    fn ai28_refresh_batch_proofs_rebinds_edited_document_and_gates_oversets() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        // Prepare local mock server for Batch 2 after approval.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let _ = stream.read(&mut buf);
            let plan = json!({
                "title": "Page 2",
                "commands": [
                    {"command": "frame.create", "params": {"spread": 1, "rect": [60, 60, 200, 160], "content": "text", "text": "Second page"}}
                ]
            });
            let body = json!({
                "message": {"content": plan.to_string()},
                "done": true,
                "done_reason": "stop",
                "prompt_eval_count": 50,
                "eval_count": 30
            })
            .to_string();
            let resp =
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(resp.as_bytes()).unwrap();
        });

        waiting(&mut runner, &mut session, format!("http://{addr}"));
        {
            let job = runner.batch.as_mut().unwrap();
            job.task.settings.budget.max_requests = 4;
            let mut ledger = UsageLedger::new(job.task.settings.budget.clone()).unwrap();
            let reservation = ledger.reserve(10, 0, ProviderKind::Ollama, false, None).unwrap();
            ledger.record(reservation, Some(&Usage { input_tokens: Some(7), output_tokens: Some(3), ..Default::default() }));
            job.ledger = Some(ledger);
        }
        let before_batch = runner.snapshot.batch.clone().unwrap();
        let job_id = before_batch.job_id.clone();
        assert_eq!(before_batch.checkpoint, 1);
        let old_hashes = before_batch.batches[0].proof_sha256.clone();

        // 1. Human makes a valid native edit on Page 1 during awaiting_human_proof.
        session
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [72, 220, 300, 280], "content": "text", "text": "Human typo correction", "caret": false}),
            )
            .unwrap();
        assert_ne!(Some(session.doc().unwrap().revision), before_batch.document_revision);

        // 2. Human accidentally introduces overset text on Page 1 -> RefreshBatchProofs rejects without killing the live batch!
        session
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [72, 300, 110, 320], "content": "text", "text": "Overflowing words ".repeat(40), "caret": false}),
            )
            .unwrap();
        let rejected = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(rejected.stage, "awaiting_human_proof");
        assert!(rejected.message.contains("Preflight rejected edited document"), "msg={}", rejected.message);
        assert_eq!(rejected.batch.as_ref().unwrap().checkpoint, 1);

        // 3. Human undoes the overset frame (preserving the valid typo correction) and refreshes proofs.
        session.execute("edit.undo", &json!({})).unwrap();
        let refreshed = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(refreshed.stage, "awaiting_human_proof", "msg={}", refreshed.message);
        let updated_batch = refreshed.batch.as_ref().unwrap();
        assert_eq!(updated_batch.checkpoint, 2);
        assert_eq!(updated_batch.document_revision, Some(session.doc().unwrap().revision));
        assert_ne!(updated_batch.batches[0].proof_sha256, old_hashes);

        // 4. Stale checkpoint 1 is rejected without destroying the batch, while checkpoint 2 approves cleanly and launches Batch 2!
        let stale = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: 1 });
        assert_eq!(stale.stage, "awaiting_human_proof");
        assert!(stale.message.contains("does not match"), "msg={}", stale.message);

        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: 2 });
        assert!(approved.busy, "msg={}", approved.message);
        let finished = wait(&mut runner, &mut session);
        assert_eq!(finished.stage, "awaiting_human_proof", "msg={}", finished.message);
        assert_eq!(finished.batch.as_ref().unwrap().completed_pages, 2);
        let batch2_checkpoint = finished.batch.as_ref().unwrap().checkpoint;
        handle.join().unwrap();

        // 5. While Batch 2 (page 2, index 1) is awaiting_human_proof, mutating sealed Page 1's story or items is rejected!
        let page1_story = session.doc().unwrap().doc.spreads[0]
            .items
            .iter()
            .find_map(|item| match &item.content {
                designcraft_engine::doc::Content::Text(tf) => Some(tf.story),
                _ => None,
            })
            .expect("Page 1 has a text story");
        session.execute("story.setText", &json!({"story": page1_story, "text": "Tampered sealed Page 1 story"})).unwrap();
        let story_tamper = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(story_tamper.stage, "awaiting_human_proof");
        assert!(story_tamper.message.contains("Already-approved text stories"), "msg={}", story_tamper.message);
        assert_eq!(story_tamper.batch.as_ref().unwrap().checkpoint, batch2_checkpoint);
        session.execute("edit.undo", &json!({})).unwrap();

        session
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [72, 420, 220, 460], "content": "text", "text": "Tampered sealed Page 1 item", "caret": false}),
            )
            .unwrap();
        let item_tamper = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(item_tamper.stage, "awaiting_human_proof");
        assert!(item_tamper.message.contains("outside the current review batch"), "msg={}", item_tamper.message);
        assert_eq!(item_tamper.batch.as_ref().unwrap().checkpoint, batch2_checkpoint);
        session.execute("edit.undo", &json!({})).unwrap();

        // 6. Legitimate human edit on Batch 2's own page (spread 1) refreshes cleanly and increments checkpoint.
        session
            .execute(
                "frame.create",
                &json!({"spread": 1, "rect": [60, 200, 280, 260], "content": "text", "text": "Valid Batch 2 human edit", "caret": false}),
            )
            .unwrap();
        let batch2_refreshed = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id });
        assert_eq!(batch2_refreshed.stage, "awaiting_human_proof", "msg={}", batch2_refreshed.message);
        assert_eq!(batch2_refreshed.batch.as_ref().unwrap().checkpoint, batch2_checkpoint + 1);
    }

    #[test]
    fn ai28_facing_pages_refresh_batch_proofs_spine_bleed_and_sealed_definitions() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let spawn_slice_server = |slices: Vec<Vec<usize>>| -> (String, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = std::thread::spawn(move || {
                for spreads in slices {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    loop {
                        let n = stream.read(&mut chunk).unwrap();
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if complete_http_body(&bytes).ok().flatten().is_some() {
                            break;
                        }
                    }
                    let commands: Vec<Value> = spreads
                        .into_iter()
                        .map(|page_idx| {
                            json!({
                                "command": "frame.create",
                                "params": {
                                    "spread": page_idx,
                                    "rect": [60, 60, 220, 160],
                                    "content": "text",
                                    "text": format!("Facing page {}", page_idx + 1)
                                }
                            })
                        })
                        .collect();
                    let plan = json!({
                        "title": "Facing slice",
                        "commands": commands
                    });
                    let body = json!({
                        "message": {"content": plan.to_string()},
                        "done": true,
                        "done_reason": "stop",
                        "prompt_eval_count": 40,
                        "eval_count": 25
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(resp.as_bytes()).unwrap();
                }
            });
            (format!("http://{addr}"), handle)
        };

        // Part A: 4-page facing_pages=true, pages_per_batch=2 (spread_aligned: Batch 1 = Page 1, Batch 2 = Pages 2-3, Batch 3 = Page 4).
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let (endpoint, server_a) = spawn_slice_server(vec![vec![0], vec![1, 2], vec![3]]);
        let mut req = task(endpoint, 16);
        req.target = TaskTarget::NewA4 { pages: 4 };
        req.settings.facing_pages = true;
        req.settings.pages_per_batch = 2;
        let started = runner.handle(&mut session, AiRequest::Start(req));
        assert!(started.busy);
        let b1 = wait(&mut runner, &mut session);
        assert_eq!(b1.stage, "awaiting_human_proof", "msg={}", b1.message);
        let b1_snap = b1.batch.as_ref().unwrap();
        assert!(b1_snap.spread_aligned);
        assert_eq!(b1_snap.completed_pages, 1);
        let job_id = b1_snap.job_id.clone();
        let cp1 = b1_snap.checkpoint;

        // editing_parents=true is rejected by both RefreshBatchProofs and ApproveBatch without destroying the batch.
        session.doc_mut().unwrap().editing_parents = true;
        let parent_refresh_err = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(parent_refresh_err.stage, "awaiting_human_proof");
        assert!(parent_refresh_err.message.contains("parent-page edit"), "msg={}", parent_refresh_err.message);
        let parent_approve_err = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: cp1 });
        assert_eq!(parent_approve_err.stage, "awaiting_human_proof");
        assert!(parent_approve_err.message.contains("parent-page edit"), "msg={}", parent_approve_err.message);
        session.doc_mut().unwrap().editing_parents = false;

        // ID-allocating edit (frame.create) on Batch 1 cover (spread 0) succeeds on facing_pages=true and approves cleanly!
        session
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [60, 220, 280, 280], "content": "text", "text": "Cover subtitle edit", "caret": false}),
            )
            .unwrap();
        let b1_refreshed = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(b1_refreshed.stage, "awaiting_human_proof", "msg={}", b1_refreshed.message);
        let cp1_refreshed = b1_refreshed.batch.as_ref().unwrap().checkpoint;
        assert_eq!(cp1_refreshed, cp1 + 1);

        let b1_approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: cp1_refreshed });
        assert!(b1_approved.busy, "msg={}", b1_approved.message);
        let b2 = wait(&mut runner, &mut session);
        assert_eq!(b2.stage, "awaiting_human_proof", "msg={}", b2.message);
        assert_eq!(b2.batch.as_ref().unwrap().completed_pages, 3);
        let cp2 = b2.batch.as_ref().unwrap().checkpoint;

        // During Batch 2 (Pages 2-3, start=1 > 0):
        // (1) Mutating sealed Page 1 (spread 0) is rejected.
        session
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [60, 320, 240, 370], "content": "text", "text": "Unauthorized cover edit", "caret": false}),
            )
            .unwrap();
        let cover_tamper = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(cover_tamper.stage, "awaiting_human_proof");
        assert!(cover_tamper.message.contains("outside the current review batch"), "msg={}", cover_tamper.message);
        session.execute("edit.undo", &json!({})).unwrap();

        // (2) Mutating an existing swatch or paragraph style is rejected when start > 0.
        let existing_swatch =
            session.doc().unwrap().doc.swatches.iter().find(|s| !s.locked).map(|s| s.name.clone()).expect("unlocked default swatch");
        session.execute("swatch.options", &json!({"name": existing_swatch, "color": "#123456"})).unwrap();
        let swatch_tamper = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(swatch_tamper.stage, "awaiting_human_proof");
        assert!(swatch_tamper.message.contains("Already-approved swatches"), "msg={}", swatch_tamper.message);
        session.execute("edit.undo", &json!({})).unwrap();

        session.execute("style.paragraph.edit", &json!({"name": "[Basic Paragraph]", "chars": {"size": 15}})).unwrap();
        let style_tamper = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(style_tamper.stage, "awaiting_human_proof");
        assert!(style_tamper.message.contains("Already-approved styles"), "msg={}", style_tamper.message);
        session.execute("edit.undo", &json!({})).unwrap();

        // (3) Appending a brand-new paragraph style + frame.create on Batch 2's facing spread (spread 1) succeeds and approves!
        session.execute("style.paragraph.create", &json!({"name": "Batch2Callout", "chars": {"size": 11}})).unwrap();
        session
            .execute(
                "frame.create",
                &json!({"spread": 1, "rect": [60, 220, 280, 280], "content": "text", "text": "Spread left edit", "caret": false}),
            )
            .unwrap();
        let b2_refreshed = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(b2_refreshed.stage, "awaiting_human_proof", "msg={}", b2_refreshed.message);
        let cp2_refreshed = b2_refreshed.batch.as_ref().unwrap().checkpoint;
        assert_eq!(cp2_refreshed, cp2 + 1);

        let b2_approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint: cp2_refreshed });
        assert!(b2_approved.busy, "msg={}", b2_approved.message);
        let b3 = wait(&mut runner, &mut session);
        assert_eq!(b3.stage, "awaiting_human_proof", "msg={}", b3.message);
        assert_eq!(b3.batch.as_ref().unwrap().completed_pages, 4);
        server_a.join().unwrap();

        // Part B: Shared facing spread with pages_per_batch=1 (Page 2 sealed, Page 3 in-scope on spread 1) rejects spine-bleed overlap!
        let dir_b = tempfile::tempdir().unwrap();
        let mut runner_b = ProductionRunner::open(dir_b.path()).unwrap();
        let mut session_b = Session::new();
        let (endpoint_b, server_b) = spawn_slice_server(vec![vec![0], vec![1], vec![2]]);
        let mut req_b = task(endpoint_b, 16);
        req_b.target = TaskTarget::NewA4 { pages: 4 };
        req_b.settings.facing_pages = true;
        req_b.settings.pages_per_batch = 1;
        let _ = runner_b.handle(&mut session_b, AiRequest::Start(req_b));
        let p1 = wait(&mut runner_b, &mut session_b);
        let job_b = p1.batch.as_ref().unwrap().job_id.clone();
        let cp_p1 = p1.batch.as_ref().unwrap().checkpoint;
        let _ = runner_b.handle(&mut session_b, AiRequest::ApproveBatch { job_id: job_b.clone(), checkpoint: cp_p1 });
        let p2 = wait(&mut runner_b, &mut session_b);
        let cp_p2 = p2.batch.as_ref().unwrap().checkpoint;
        let _ = runner_b.handle(&mut session_b, AiRequest::ApproveBatch { job_id: job_b.clone(), checkpoint: cp_p2 });
        let p3 = wait(&mut runner_b, &mut session_b);
        assert_eq!(p3.stage, "awaiting_human_proof", "msg={}", p3.message);
        assert_eq!(p3.batch.as_ref().unwrap().completed_pages, 3);
        let cp_p3 = p3.batch.as_ref().unwrap().checkpoint;
        server_b.join().unwrap();

        // Spread 1 holds sealed Page 2 (x = 0..595.28) and in-scope Page 3 (x = 595.28..1190.56).
        // Frame centered at x=650 (owned by Page 3) with x0=560 (< 595.28) bleeds across the spine onto sealed Page 2 -> rejected!
        session_b
            .execute(
                "frame.create",
                &json!({"spread": 1, "rect": [560, 220, 740, 280], "content": "text", "text": "Spine bleed onto Page 2", "caret": false}),
            )
            .unwrap();
        let bleed_rejected = runner_b.handle(&mut session_b, AiRequest::RefreshBatchProofs { job_id: job_b.clone() });
        assert_eq!(bleed_rejected.stage, "awaiting_human_proof");
        assert!(bleed_rejected.message.contains("shared spread"), "msg={}", bleed_rejected.message);
        assert_eq!(bleed_rejected.batch.as_ref().unwrap().checkpoint, cp_p3);
        session_b.execute("edit.undo", &json!({})).unwrap();

        // Frame cleanly inside Page 3 (x = 660..860) refreshes cleanly!
        session_b
            .execute(
                "frame.create",
                &json!({"spread": 1, "rect": [660, 220, 860, 280], "content": "text", "text": "Clean Page 3 edit", "caret": false}),
            )
            .unwrap();
        let p3_refreshed = runner_b.handle(&mut session_b, AiRequest::RefreshBatchProofs { job_id: job_b });
        assert_eq!(p3_refreshed.stage, "awaiting_human_proof", "msg={}", p3_refreshed.message);
        assert_eq!(p3_refreshed.batch.as_ref().unwrap().checkpoint, cp_p3 + 1);

        // Part C: Transitive sub-story references (story.link, footnotes, endnotes, and table cell stories/graphics).
        let mut scratch_session = Session::new();
        scratch_session.execute("file.new", &json!({"pages": 1})).unwrap();
        scratch_session.execute("frame.create", &json!({"spread": 0, "rect": [50, 50, 120, 120]})).unwrap();
        let template_item = scratch_session.doc().unwrap().doc.spreads[0].items[0].as_ref().clone();
        let mut doc = scratch_session.doc().unwrap().doc.as_ref().clone();

        let linked_sid = designcraft_engine::doc::StoryId(701);
        let cell_linked_sid = designcraft_engine::doc::StoryId(704);
        let endnote_linked_sid = designcraft_engine::doc::StoryId(705);
        let root_sid = designcraft_engine::doc::StoryId(700);
        let note_asset = designcraft_engine::doc::AssetId(801);
        let cell_asset = designcraft_engine::doc::AssetId(802);

        let make_graphic = |asset: designcraft_engine::doc::AssetId| -> designcraft_engine::doc::Graphic {
            serde_json::from_value(json!({
                "asset": asset,
                "size": [20.0, 20.0],
                "xf": [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
            }))
            .unwrap()
        };

        let linked_story = designcraft_engine::doc::Story::new(linked_sid);
        let cell_linked_story = designcraft_engine::doc::Story::new(cell_linked_sid);
        let endnote_linked_story = designcraft_engine::doc::Story::new(endnote_linked_sid);
        let mut root_story = designcraft_engine::doc::Story::new(root_sid);
        root_story.link = Some((linked_sid, 1));

        let mut note_story = designcraft_engine::doc::Story::new(designcraft_engine::doc::StoryId(702));
        let mut anchored_graphic = template_item;
        anchored_graphic.id = designcraft_engine::doc::ItemId(901);
        anchored_graphic.content = designcraft_engine::doc::Content::Graphic(make_graphic(note_asset));
        note_story.objects.push(Arc::new(designcraft_engine::doc::AnchoredObject { item: anchored_graphic, ..Default::default() }));
        root_story.notes.push(Arc::new(designcraft_engine::doc::Footnote { id: 1, text: note_story }));

        let mut endnote_story = designcraft_engine::doc::Story::new(designcraft_engine::doc::StoryId(703));
        endnote_story.link = Some((endnote_linked_sid, 1));
        root_story.endnotes.push(Arc::new(designcraft_engine::doc::Footnote { id: 2, text: endnote_story }));

        let mut table = designcraft_engine::doc::Table::new(1, 1, 1, 0, 0, 100.0);
        table.cells[0].graphic = Some(make_graphic(cell_asset));
        table.cells[0].text.link = Some((cell_linked_sid, 1));
        root_story.tables.insert(1, Arc::new(table));

        doc.stories.insert(linked_sid, Arc::new(linked_story));
        doc.stories.insert(cell_linked_sid, Arc::new(cell_linked_story));
        doc.stories.insert(endnote_linked_sid, Arc::new(endnote_linked_story));
        doc.stories.insert(root_sid, Arc::new(root_story));

        let mut stories = std::collections::BTreeSet::from([root_sid]);
        let mut assets = std::collections::BTreeSet::new();
        expand_story_transitive_refs(&doc, &mut stories, &mut assets).unwrap();
        assert!(stories.contains(&linked_sid));
        assert!(stories.contains(&cell_linked_sid));
        assert!(stories.contains(&endnote_linked_sid));
        assert!(assets.contains(&note_asset));
        assert!(assets.contains(&cell_asset));
    }

    #[test]
    fn ai28_single_page_arabic_batch_publish_and_blank_reuse_set_rtl_binding_and_r2l_proof_pdf() {
        for blank_reuse in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut runner = ProductionRunner::open(dir.path()).unwrap();
            let mut session = Session::new();
            let existing = if blank_reuse {
                session.execute("file.new", &json!({"pages": 2, "facingPages": false})).unwrap();
                Some(session.doc().unwrap().clone())
            } else {
                None
            };
            let mut request = task("http://127.0.0.1:9/".into(), 2);
            request.prompt = "أنشئ كتاب عمل عربي من صفحتين منفردتين في مادة العلوم.".into();
            request.settings.facing_pages = false;
            request.settings.right_to_left_binding = None;
            request.settings.pages_per_batch = 2;
            request.target = if blank_reuse { TaskTarget::ContinueBook } else { TaskTarget::NewA4 { pages: 2 } };

            let (canonical, _) = task_document_context(&session, &request.target, None, &request.prompt, &request.settings).unwrap();
            assert!(!canonical.doc.settings.facing_pages);
            assert!(canonical.doc.settings.right_to_left_binding, "blank_reuse={blank_reuse}");

            let plans: Vec<_> = (0..2u32)
                .map(|page| ProductionPlan {
                    title: format!("صفحة {}", page + 1),
                    commands: vec![EngineAction { command: "frame.create".into(), params: json!({"spread": page, "rect": [60, 120, 240, 220]}) }],
                    images: vec![],
                })
                .collect();
            let mut proof_session = Session::new();
            proof_session.prefs = session.prefs.clone();
            proof_session.add_document(canonical);
            let chunks: Vec<_> = plans.iter().map(|plan| json!({"commands": plan.commands, "label": plan.title})).collect();
            proof_session
                .execute("production.applyGroup", &json!({"chunks": chunks, "outputMode": "native_only", "requirePreflight": true}))
                .unwrap();
            let mut proofs = vec![worker::ProofArtifact {
                name: "document.designcraft".into(),
                bytes: designcraft_format::save(&proof_session.doc().unwrap().doc).unwrap(),
            }];
            use base64::Engine as _;
            let pdf = proof_session.execute("file.exportPdf", &json!({"pages": [1, 2], "spreads": false, "bleed": false, "marks": false})).unwrap();
            let pdf_bytes = base64::engine::general_purpose::STANDARD.decode(pdf["base64"].as_str().unwrap()).unwrap();
            let pdf_raw = String::from_utf8_lossy(&pdf_bytes);
            assert!(pdf_raw.contains("/Direction") && pdf_raw.contains("/R2L"), "blank_reuse={blank_reuse}");
            proofs.push(worker::ProofArtifact { name: "proof.pdf".into(), bytes: pdf_bytes });
            let mut renderer = designcraft_render::Renderer::new();
            for page in 0..2usize {
                let image =
                    renderer.render_page(&proof_session.doc().unwrap().doc, &proof_session.cache, page, 0.25, false, &Default::default()).unwrap();
                proofs.push(worker::ProofArtifact { name: format!("page-{}.png", page + 1), bytes: image.to_png() });
            }

            let ledger = UsageLedger::new(request.settings.budget.clone()).unwrap();
            let job_id = if blank_reuse { "single-ar-blank-reuse" } else { "single-ar-new" };
            let snapshot = BatchSnapshot {
                job_id: job_id.into(),
                total_pages: 2,
                pages_per_batch: 2,
                current_batch: 1,
                total_batches: 1,
                completed_pages: 0,
                status: "running".into(),
                document_uid: existing.as_ref().map(|d| d.uid),
                document_revision: existing.as_ref().map(|d| d.revision),
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
                publication_base: existing.clone(),
                retry_sequence: 0,
            });
            runner.snapshot.batch = Some(snapshot);
            runner.snapshot.busy = true;
            let (sender, receiver) = std::sync::mpsc::channel();
            sender
                .send(worker::Message::FinishedWithLedger(
                    worker::Outcome { result: Ok(worker::Product::BatchGroup { plans, proofs }), usage: Default::default() },
                    Box::new(ledger),
                ))
                .unwrap();
            runner.pending = Some(Pending {
                roots: None,
                worker: worker::Worker { receiver, cancel: Arc::new(AtomicBool::new(false)) },
                id: format!("{job_id}-batch-1"),
                started: Instant::now(),
                task: Some(request.clone()),
                expected: existing.as_ref().map(|d| (d.uid, d.revision)),
                expected_doc: existing.as_ref().map(|d| d.doc.clone()),
                expected_prefs: None,
                skills: vec![],
                settings: request.settings,
                deferred_outcome: None,
                deferred_batch: None,
            });
            let completed = runner.handle(&mut session, AiRequest::Poll);
            assert_eq!(completed.stage, "awaiting_human_proof", "blank_reuse={blank_reuse}: {}", completed.message);
            let published = session.doc().unwrap();
            assert!(!published.doc.settings.facing_pages, "blank_reuse={blank_reuse}");
            assert!(published.doc.settings.right_to_left_binding, "blank_reuse={blank_reuse}");
        }
    }

    #[test]
    fn bug01_non_layout_prefs_and_mru_changes_do_not_invalidate_approve_batch() {
        let dir = tempfile::tempdir().unwrap();
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

        // Mutate MRU and screen-only UI preference fields before ApproveBatch.
        session.prefs.note_recent_file("/tmp/another-book.designcraft");
        session.prefs.recent_files_max = 42;
        session.prefs.show_hidden_characters = !session.prefs.show_hidden_characters;
        session.prefs.highlight_hj = !session.prefs.highlight_hj;
        session.prefs.highlight_keeps = !session.prefs.highlight_keeps;
        session.prefs.recent_fonts.push("Inter".into());
        session.prefs.favorite_fonts.push("Lora".into());
        session.prefs.show_added_text = !session.prefs.show_added_text;
        session.prefs.show_note_anchors = !session.prefs.show_note_anchors;
        session.prefs.recovery_minutes = 7.5;
        session.prefs.show_font_names_in_english = !session.prefs.show_font_names_in_english;

        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint });
        assert_eq!(approved.stage, "completed", "msg={}", approved.message);
        assert_eq!(approved.batch.as_ref().unwrap().status, "completed");
    }

    #[test]
    fn bug01_validation_error_in_approve_batch_preserves_batch_for_refresh_and_approval() {
        let dir = tempfile::tempdir().unwrap();
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

        // Edit page 1 without refreshing proofs first -> ApproveBatch rejects, but preserves self.batch!
        session
            .execute("frame.create", &json!({"spread": 0, "rect": [60, 220, 260, 280], "content": "text", "text": "Human typo fix", "caret": false}))
            .unwrap();
        let rejected = runner.handle(&mut session, AiRequest::ApproveBatch { job_id: job_id.clone(), checkpoint });
        assert_eq!(rejected.stage, "awaiting_human_proof", "msg={}", rejected.message);
        assert_eq!(rejected.batch.as_ref().unwrap().status, "awaiting_human_proof");
        assert!(runner.batch.is_some(), "Recoverable validation error in approve_batch must keep self.batch alive");

        // User refreshes batch proofs and approves cleanly without losing the batch job!
        let refreshed = runner.handle(&mut session, AiRequest::RefreshBatchProofs { job_id: job_id.clone() });
        assert_eq!(refreshed.stage, "awaiting_human_proof", "msg={}", refreshed.message);
        let new_checkpoint = refreshed.batch.as_ref().unwrap().checkpoint;
        assert_eq!(new_checkpoint, checkpoint + 1);

        let approved = runner.handle(&mut session, AiRequest::ApproveBatch { job_id, checkpoint: new_checkpoint });
        assert_eq!(approved.stage, "completed", "msg={}", approved.message);
        assert_eq!(approved.batch.as_ref().unwrap().status, "completed");
    }

    #[test]
    fn bug01_selection_revision_bumps_rebind_in_memory_without_rewriting_batch_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let (_job_id, _checkpoint) = waiting(&mut runner, &mut session, "http://127.0.0.1:9/".into());
        let checkpoint_file = dir.path().join("batch-checkpoint.json");
        let saved_before = std::fs::read_to_string(&checkpoint_file).unwrap();

        // Simulate selection-only revision bump on the exact pinned `Arc<Document>`.
        session.active_mut().unwrap().bump_revision();
        let bumped_rev = session.doc().unwrap().revision;
        let polled = runner.handle(&mut session, AiRequest::Poll);
        assert_eq!(polled.batch.as_ref().unwrap().document_revision, Some(bumped_rev));
        assert_eq!(std::fs::read_to_string(&checkpoint_file).unwrap(), saved_before, "selection-only bump must not rewrite batch-checkpoint.json");
    }

    #[test]
    fn ai33_auto_continue_pause_resume_and_blank_planner_continuation() {
        let _serial = worker::LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _memory = crate::resources::TestAdmissionGuard::healthy();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicU32::new(0));
        let server_calls = calls.clone();
        let slow_first = Arc::new(AtomicBool::new(false));
        let server_slow = slow_first.clone();
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
                        return Err("Mock request exceeded bounds".into());
                    }
                    let count = socket.read(&mut chunk).map_err(|e| e.to_string())?;
                    if count == 0 {
                        return Err("Mock request ended early".into());
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(body) = complete_http_body(&bytes)? {
                        break serde_json::from_slice::<Value>(body).map_err(|e| e.to_string())?;
                    }
                };
                if server_slow.swap(false, Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(120));
                }
                let page = server_calls.fetch_add(1, Ordering::Relaxed);
                let _ = request;
                let plan = json!({
                    "title": format!("Auto page {}", page + 1),
                    "commands": [
                        {"command": "frame.create", "params": {"spread": page, "rect": [48, 56, 200, 160], "content": "unassigned"}},
                        {"command": "object.label", "params": {"ids": ["$0.id"], "label": format!("auto-page-{}", page + 1)}}
                    ],
                    "images": []
                });
                let reply = json!({
                    "done": true,
                    "message": {"content": plan.to_string()},
                    "prompt_eval_count": 40,
                    "eval_count": 20
                })
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

        // 1. ExecutionPacing::AutoContinue completes all 3 batches without manual ApproveBatch,
        //    saving rolling latest-checkpoint.designcraft on batches 1 & 2 and full PDF+native proofs on batch 3.
        let dir = tempfile::tempdir().unwrap();
        let mut runner = ProductionRunner::open(dir.path()).unwrap();
        let mut session = Session::new();
        let mut auto_req = task(endpoint.clone(), 6);
        auto_req.target = TaskTarget::NewA4 { pages: 3 };
        auto_req.settings.execution_pacing = designcraft_ai_types::ExecutionPacing::AutoContinue;
        let started = runner.handle(&mut session, AiRequest::Start(auto_req));
        assert!(started.busy, "{}", started.message);
        let completed = wait(&mut runner, &mut session);
        assert_eq!(completed.stage, "completed", "{}", completed.message);
        let batch = completed.batch.as_ref().unwrap();
        assert_eq!(batch.status, "completed");
        assert_eq!(batch.completed_pages, 3);
        assert_eq!(batch.batches.len(), 3);
        assert!(dir.path().join("runs").join(&batch.job_id).join("latest-checkpoint.designcraft").is_file());
        assert_eq!(batch.batches[0].proof_paths.len(), 1);
        assert!(!batch.batches[0].proof_paths.iter().any(|p| p.ends_with("proof.pdf")));
        assert!(batch.batches[2].proof_paths.iter().any(|p| p.ends_with("proof.pdf")));
        assert!(batch.batches[2].proof_paths.iter().any(|p| p.ends_with("document.designcraft")));
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // 2. PauseBatch stops cleanly after the in-flight batch, and ResumeBatch finishes the remaining batches.
        calls.store(0, Ordering::Relaxed);
        slow_first.store(true, Ordering::Relaxed);
        let mut pause_req = task(endpoint.clone(), 6);
        pause_req.prompt = "Make one distinct native rectangle per page for pause-resume test, with exact 15 mm margins.".into();
        pause_req.target = TaskTarget::NewA4 { pages: 3 };
        pause_req.settings.execution_pacing = designcraft_ai_types::ExecutionPacing::AutoContinue;
        let started_pause = runner.handle(&mut session, AiRequest::Start(pause_req));
        assert!(started_pause.busy, "{}", started_pause.message);
        let job_id = started_pause.batch.as_ref().unwrap().job_id.clone();
        let paused_snap = runner.handle(&mut session, AiRequest::PauseBatch { job_id: job_id.clone() });
        assert!(paused_snap.batch.as_ref().unwrap().paused);
        let stopped_after_one = wait(&mut runner, &mut session);
        assert_eq!(stopped_after_one.stage, "awaiting_human_proof", "{}", stopped_after_one.message);
        assert_eq!(stopped_after_one.batch.as_ref().unwrap().completed_pages, 1);
        assert!(stopped_after_one.batch.as_ref().unwrap().paused);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let resumed = runner.handle(&mut session, AiRequest::ResumeBatch { job_id });
        assert!(resumed.busy, "{}", resumed.message);
        let finished_after_resume = wait(&mut runner, &mut session);
        assert_eq!(finished_after_resume.stage, "completed", "{}", finished_after_resume.message);
        assert_eq!(finished_after_resume.batch.as_ref().unwrap().completed_pages, 3);
        assert!(!finished_after_resume.batch.as_ref().unwrap().paused);
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // 3. ContinueBook on a 1-page blank A4 open document ("بلانر") auto-expands to 4 pages and A5 trim.
        calls.store(0, Ordering::Relaxed);
        session.execute("file.new", &new_a4(1)).unwrap();
        assert_eq!(session.doc().unwrap().doc.page_count(), 1);
        let mut cont_req = task(endpoint, 6);
        cont_req.target = TaskTarget::ContinueBook;
        cont_req.prompt = "انشاء بلانر متكامل وشامل من 4 صفحات\nTrim Size: 148 x 210 mm".into();
        cont_req.settings.execution_pacing = designcraft_ai_types::ExecutionPacing::AutoContinue;
        let started_cont = runner.handle(&mut session, AiRequest::Start(cont_req));
        assert!(started_cont.busy, "{}", started_cont.message);
        let finished_cont = wait(&mut runner, &mut session);
        assert_eq!(finished_cont.stage, "completed", "{}", finished_cont.message);
        let final_doc = &session.doc().unwrap().doc;
        assert_eq!(final_doc.page_count(), 4);
        assert!((final_doc.settings.page_width - 148.0 * 72.0 / 25.4).abs() < 1.0, "width={}", final_doc.settings.page_width);
        assert!((final_doc.settings.page_height - 210.0 * 72.0 / 25.4).abs() < 1.0, "height={}", final_doc.settings.page_height);
        assert!(final_doc.spreads.iter().all(|s| !s.items.is_empty()));

        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    }
}
