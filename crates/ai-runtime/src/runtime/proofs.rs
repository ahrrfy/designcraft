use super::{
    AiSettings, ApprovalEvidence, BTreeSet, DocState, MAX_BRIEF_BYTES, Path, ProductionRunner, RevisionStatus, SampleEvidence, Session, SkillCard,
    SkillRegistry, SkillReview, SkillSnapshot, TaskRequest, TaskTarget, Value, file_access, is_trusted_catalog_provenance, json, worker,
    write_profile,
};

impl ProductionRunner {
    pub(super) fn choose_skills(&self, prompt: &str) -> Vec<String> {
        choose_compatible_skills(prompt, self.registry.catalog(""), &supported_commands())
    }

    #[cfg(test)]
    pub(super) fn pinned_document_params(&self, pins: &[(String, String)], pages: u32) -> Result<Value, String> {
        self.pinned_document_params_for_brief(pins, pages, &AiSettings::default(), "")
    }
    pub(super) fn pinned_design_profile(&self, pins: &[(String, String)]) -> Result<Option<designcraft_ai_types::DesignProfile>, String> {
        let mut profile = None;
        for (id, revision) in pins {
            let record = self.registry.revision(id, revision).map_err(|e| e.to_string())?;
            if let Some(selected) = record.manifest.design_profile.as_ref() {
                if profile.is_some() {
                    return Err("New document has conflicting pinned design profiles".into());
                }
                selected.validate()?;
                profile = Some(selected.clone());
            }
        }
        Ok(profile)
    }

    #[cfg(test)]
    pub(super) fn pinned_document_params_for_brief(
        &self,
        pins: &[(String, String)],
        pages: u32,
        settings: &AiSettings,
        brief: &str,
    ) -> Result<Value, String> {
        let profile = self.pinned_design_profile(pins)?;
        production_document_params(profile.as_ref(), pages, settings, brief)
    }

    pub(super) fn prepare_automatic_skills(&mut self, session: &Session, prompt: &str) -> Result<Vec<String>, String> {
        self.prepare_skills_for_style(session, prompt, None)
    }

    pub(super) fn prepare_skills_for_style(&mut self, session: &Session, prompt: &str, style_id: Option<&str>) -> Result<Vec<String>, String> {
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
                    if (entry.skill_id.starts_with("kg-") || entry.skill_id.starts_with("early-years-"))
                        && !has_educational_kg_cues(prompt)
                        && !prompt_names_skill_id(prompt, &entry.skill_id)
                    {
                        return None;
                    }
                    let profile = revision.manifest.design_profile.as_ref()?;
                    let lower = normalize_skill_search(prompt);
                    let hits = profile.keywords.iter().filter(|word| lower.contains(&normalize_skill_search(word))).count();
                    let is_planner = crate::template_plan::is_stationery_or_planner_prompt(prompt);
                    let bonus = if is_planner {
                        usize::from(hits > 0 && entry.skill_id == "planner-journal-studio-style") * 24
                    } else {
                        usize::from(hits > 0 && entry.skill_id == "kg-oxford-cambridge-style") * 16
                    };
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

    pub(super) fn prepare_pinned_skills(&mut self, session: &Session, pinned: &[String]) -> Result<Vec<String>, String> {
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

    pub(super) fn prepare_style(&mut self, session: &Session, id: &str) -> Result<(), String> {
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

    pub(super) fn style_use(&self, pins: &[(String, String)], target: Option<&TaskTarget>) -> Option<designcraft_ai_types::DesignStyleUse> {
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
    pub(super) fn refresh_catalog(&mut self) {
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
    pub(super) fn inspect_skill(&mut self, id: &str, revision: &str) -> Result<(), String> {
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
    pub(super) fn run_sample(&mut self, session: &Session, id: &str, revision: &str) -> Result<(), String> {
        self.run_design_template(session, id, revision, None)
    }

    pub(super) fn run_design_template(&mut self, session: &Session, id: &str, revision: &str, template: Option<&str>) -> Result<(), String> {
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

    pub(super) fn prepare_selected_skills(&mut self, session: &Session, skill_ids: &[String], style_id: Option<&str>) -> Result<Vec<String>, String> {
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

    pub(super) fn default_preview_style_id(&self) -> Option<String> {
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

/// Preserve immutable guidance and executable design profiles within the task
/// budget. Command planners can omit optional sample templates, largest first;
/// the content compiler never needs non-style command examples.
pub(super) fn bounded_task_skills(mut skills: Vec<SkillSnapshot>, target: &TaskTarget) -> Result<Vec<SkillSnapshot>, String> {
    if matches!(target, TaskTarget::CurrentDocument | TaskTarget::NewDocument { .. }) {
        // The opened or custom-geometry document supplies its actual geometry.
        // Fixed A4 page-template commands are cleared while instructions, references
        // and profile identity remain pinned guidance for its command planner.
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

pub(super) fn merge_pinned_and_automatic_skills(pinned: &[String], auto_ids: Vec<String>, style_id: Option<&str>) -> Vec<String> {
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

pub(super) fn task_skill_context_bytes(skills: &[SkillSnapshot]) -> Result<usize, String> {
    let mut bytes = 2usize;
    for (index, skill) in skills.iter().enumerate() {
        let size = serde_json::to_vec(skill).map_err(|e| e.to_string())?.len();
        bytes = bytes.checked_add(size).and_then(|size| size.checked_add(usize::from(index > 0))).ok_or("Selected skill context size overflow")?;
    }
    Ok(bytes)
}

pub(super) fn design_revision(entry: &designcraft_ai_skills::CatalogEntry) -> Option<&designcraft_ai_skills::SkillRevision> {
    let latest = entry.revisions.last()?;
    if latest.status == RevisionStatus::Draft {
        Some(latest)
    } else {
        entry.active_revision.as_ref().and_then(|id| entry.revisions.iter().find(|r| &r.revision == id)).or(Some(latest))
    }
}

pub(super) fn active_design_revision(entry: &designcraft_ai_skills::CatalogEntry) -> Option<&designcraft_ai_skills::SkillRevision> {
    entry.active_revision.as_ref().and_then(|id| {
        entry.revisions.iter().find(|r| &r.revision == id && r.status == RevisionStatus::Approved && r.manifest.design_profile.is_some())
    })
}

/// Revalidate every exact revision template immediately before approval. Each
/// variant gets a fresh scratch document; no live document, history or selection
/// is exposed to template execution, including failed result references.
pub(super) fn validate_skill_templates(registry: &SkillRegistry, session: &Session, id: &str, revision: &str) -> Result<(), String> {
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

pub(super) fn skill_is_compatible(revision: &designcraft_ai_skills::SkillRevision, supported: &BTreeSet<String>) -> bool {
    revision.manifest.external_tools.is_empty()
        && !revision.manifest.required_commands.is_empty()
        && revision.manifest.required_commands.iter().all(|command| supported.contains(command))
}

pub(super) fn selected_skill_references(revision: &designcraft_ai_skills::SkillRevision) -> Vec<String> {
    revision.manifest.references.iter().filter(|path| designcraft_ai_skills::is_supported_text_reference(path)).cloned().collect()
}

pub(super) fn choose_compatible_skills(prompt: &str, entries: Vec<designcraft_ai_skills::CatalogEntry>, supported: &BTreeSet<String>) -> Vec<String> {
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

pub(super) fn prompt_names_skill_id(prompt: &str, skill_id: &str) -> bool {
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

pub(crate) fn has_educational_kg_cues(raw_prompt: &str) -> bool {
    let prompt = normalize_skill_search(raw_prompt);
    let words: Vec<&str> = prompt.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let word_matches =
        |w: &str, term: &str| w == term || ["ال", "وال", "بال", "لل", "كال", "فال", "و"].iter().any(|pref| w.strip_prefix(pref) == Some(term));
    let mentions =
        |terms: &[&str]| {
            terms.iter().any(|term| {
                if term.contains(' ') || term.contains('-') { prompt.contains(term) } else { words.iter().any(|w| word_matches(w, term)) }
            })
        };
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
    if upper_stage {
        return false;
    }
    mentions(&[
        "أطفال",
        "اطفال",
        "طفل",
        "روضة",
        "روضه",
        "تمهيدي",
        "للصغار",
        "ابتدائي",
        "تلاميذ",
        "مدرسة",
        "مدرسه",
        "مدرسي",
        "مدرسية",
        "منهج",
        "مناهج",
        "مقرر",
        "مادة",
        "ماده",
        "درس",
        "دروس",
        "كراس",
        "كتيب علوم",
        "كتيب رياضيات",
        "كتيب حاسوب",
        "كتاب علوم",
        "كتاب رياضيات",
        "أكسفورد",
        "اكسفورد",
        "كامبريدج",
        "كامبردج",
        "فاملي اند فريندز",
        "فرست فريندز",
        "كارنيت",
        "جارنيت",
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
        "kids",
        "curriculum",
        "workbook",
        "coursebook",
        "textbook",
        "worksheet",
        "syllabus",
        "scope and sequence",
        "oxford",
        "cambridge",
        "family and friends",
        "first friends",
        "garnet",
        "phonics",
        "handwriting",
    ])
}

pub(super) fn skill_match_score(prompt: &str, entry: &designcraft_ai_skills::CatalogEntry) -> usize {
    let exact_id_boost = if prompt_names_skill_id(prompt, &entry.skill_id) { 80 } else { 0 };
    if (entry.skill_id.starts_with("kg-") || entry.skill_id.starts_with("early-years-")) && exact_id_boost == 0 && !has_educational_kg_cues(prompt) {
        return 0;
    }
    let raw_prompt = prompt;
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
    let kg = !upper_stage && has_educational_kg_cues(raw_prompt) && (children || intl || computing || book || visual);
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
pub(super) fn normalize_skill_search(text: &str) -> String {
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

pub(super) fn supported_commands() -> BTreeSet<String> {
    designcraft_engine::cmd::production::catalog()
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("command").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub(super) fn task_document_context(
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
            crate::template_plan::initialize_relational_book_master(&mut scratch, profile, settings, brief)?;
            if settings.facing_pages {
                initialize_facing_document(&mut scratch, settings, brief)?;
                let state = crate::facing_pages::canonicalize(scratch.doc().map_err(|e| e.to_string())?, &scratch.prefs)?;
                scratch.add_document(state);
            }
            let context = compact_context(&scratch)?;
            Ok((scratch.doc().map_err(|e| e.to_string())?.clone(), context))
        }
        TaskTarget::NewDocument { pages, geometry } => {
            crate::intent_geometry::validate_geometry_spec(geometry)?;
            if let Some(profile) = profile {
                profile.validate()?;
            }
            let mut scratch = Session::new();
            scratch.prefs = session.prefs.clone();
            let params = crate::intent_geometry::geometry_to_file_new_params(geometry, *pages, settings, brief)?;
            scratch.execute("file.new", &params).map_err(|e| e.to_string())?;
            if geometry.facing_pages.unwrap_or(false) {
                let state = crate::facing_pages::canonicalize(scratch.doc().map_err(|e| e.to_string())?, &scratch.prefs)?;
                scratch.add_document(state);
            }
            let context = compact_context(&scratch)?;
            Ok((scratch.doc().map_err(|e| e.to_string())?.clone(), context))
        }
    }
}
pub(super) fn production_document_params(
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
pub(super) fn initialize_print_document(session: &mut Session, brief: &str) -> Result<(), String> {
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
pub(super) fn initialize_facing_document(session: &mut Session, settings: &AiSettings, brief: &str) -> Result<(), String> {
    if settings.facing_pages {
        session
            .execute("layout.documentSetup", &json!({"facingPages":true,"binding":if book_binding(settings,brief){"rightToLeft"}else{"leftToRight"}}))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[cfg(test)]
pub(super) fn new_a4(pages: u32) -> Value {
    json!({"preset":"A4","pages":pages,"facingPages":false,"margins":15.0*72.0/25.4,"bleed":0})
}
pub(super) fn print_params(printer: Option<String>, grayscale: bool, duplex: String) -> Value {
    let mut params = json!({"grayscale":grayscale,"duplex":duplex,"scaling":"exact"});
    if let Some(printer) = printer {
        params["printer"] = json!(printer);
    }
    params
}
pub(super) fn ensure_preflight(session: &mut Session) -> Result<Value, String> {
    ensure_preflight_at(session, 150.0)
}
pub(super) fn ensure_preflight_at(session: &mut Session, min_ppi: f64) -> Result<Value, String> {
    let report = session.execute("preflight.run", &json!({"minPpi":min_ppi})).map_err(|e| e.to_string())?;
    if report.get("errors").and_then(Value::as_u64).is_none_or(|n| n > 0) {
        return Err(format!("Preflight errors require correction before output: {report}"));
    }
    Ok(report)
}
/// Check before file.new or evidence writes: normal command prologues can commit IME
/// composition and restore previews, so the later production.apply guard is insufficient.
pub(super) fn is_publication_temporarily_busy(session: &Session, target_uid: Option<u64>) -> bool {
    if ensure_publication_idle(session).is_err() {
        return true;
    }
    if let Some(uid) = target_uid
        && let Some(target) = session.documents().iter().find(|doc| doc.uid == uid)
        && (target.interaction.is_some() || target.preview_stash.is_some())
    {
        return true;
    }
    false
}

pub(super) fn ensure_publication_idle(session: &Session) -> Result<(), String> {
    if session.tool_composing() || session.active().is_some_and(|state| state.interaction.is_some() || state.preview_stash.is_some()) {
        return Err("Finish the current interaction, IME composition or preview before publishing AI production".into());
    }
    Ok(())
}
/// Task environment is rebuilt for every explicit start, retry and approved
/// group. It is host data, never authority recovered from a model response.
pub(super) fn enrich_production_context(context: &mut Value, root: &Path, base: &DocState, task: &TaskRequest) -> Result<(), String> {
    enrich_production_context_with_memory(context, root, base, task, None)
}

pub(super) fn enrich_production_context_with_memory(
    context: &mut Value,
    root: &Path,
    base: &DocState,
    task: &TaskRequest,
    memory: Option<&crate::brief_planner::SessionDesignMemory>,
) -> Result<(), String> {
    let context = context.as_object_mut().ok_or("Invalid native production context")?;
    context.insert("_production_cache_root".into(), json!(root.join("production-cache-v1")));
    context.insert("_ai_profile_root".into(), json!(root));
    let art_blueprint = crate::brief_planner::expand_brief_blueprint(task, Some(&base.doc));
    if let Ok(bp_val) = serde_json::to_value(&art_blueprint) {
        context.insert("brief_art_direction".into(), bp_val);
    }
    if let Some(mem) = memory {
        let summary = mem.format_context_summary(task.selection_context.as_ref());
        if !summary.is_empty() {
            context.insert("session_design_memory".into(), json!(summary));
        }
    }
    if let Some(sel) = &task.selection_context
        && let Ok(sel_val) = serde_json::to_value(sel)
    {
        context.insert("selection_context".into(), sel_val);
        context.insert(
            "selection_edit_directive".into(),
            json!("Modify ONLY the targeted page/frame(s) in selection_context unless the brief explicitly requests broader layout changes."),
        );
    }
    let brand_palette = crate::native_layout::extract_brand_palette_from_attachments(&task.attachments);
    if !brand_palette.is_empty() {
        context.insert("extracted_brand_palette".into(), json!(brand_palette));
    }
    let mut pack_ids = task.knowledge_pack_ids.clone();
    for id in &task.settings.active_knowledge_packs {
        if !pack_ids.contains(id) {
            pack_ids.push(id.clone());
        }
    }
    if !task.attachments.is_empty() || !pack_ids.is_empty() {
        let profile_roots = file_access::AutomationRoots::new(&[root], &[root]).ok().flatten();
        let (rag_idx, scanned_images, _) =
            crate::rag::build_task_knowledge_index(profile_roots.as_deref(), Some(root), &task.attachments, &pack_ids, &[]);
        let hits = rag_idx.search(&task.prompt, 6);
        if !hits.is_empty()
            && let Ok(hits_val) = serde_json::to_value(&hits)
        {
            context.insert("rag_grounding_hits".into(), hits_val);
        }
        if !scanned_images.is_empty() && designcraft_ai_providers::supports_agent_vision(&task.settings.text_model) {
            use base64::Engine as _;
            let mut encoded_imgs = Vec::new();
            let mut total_bytes = 0usize;
            for img in scanned_images.into_iter().take(4) {
                if total_bytes.saturating_add(img.bytes.len()) > 1024 * 1024 {
                    continue;
                }
                total_bytes = total_bytes.saturating_add(img.bytes.len());
                encoded_imgs.push(json!({
                    "mediaType": img.mime_type,
                    "base64": base64::engine::general_purpose::STANDARD.encode(&img.bytes),
                }));
            }
            if !encoded_imgs.is_empty() {
                context.insert("_scanned_attachment_images".into(), Value::Array(encoded_imgs));
                context.insert(
                    "scanned_attachment_notice".into(),
                    json!(
                        "Attached scanned document page image(s) are included in your multimodal visual input; read and ground your output in them."
                    ),
                );
            }
        }
    }
    let uses_facing =
        task.settings.facing_pages || matches!(&task.target, TaskTarget::NewDocument { geometry, .. } if geometry.facing_pages.unwrap_or(false));
    if uses_facing {
        context.insert("facing_editorial_direction".into(), json!({
            "enabled":true,"rightToLeft":book_binding(&task.settings,&task.prompt),
            "instruction":"Plan occasional visually connected facing pairs, like an educational magazine: shared thematic scene, aligned sections, related activities and a continuous restrained identity. Use chapter openers and selected learning sequences, not one repeated spread everywhere. Preserve each page's supplied lesson and printable copy. Keep critical text/answer areas clear of the binding gutter; headers/folios follow left/right positions. Coordinate with the adjacent supplied PAGE block; both pages retain independently editable native elements. Cover page 1 stands alone; reader pairs are pages 2–3, 4–5 and so on."
        }));
    }
    if let Some(contract) = crate::print_contract::PrintContract::from_prompt(&task.prompt)? {
        context.insert("production_print_contract".into(), serde_json::to_value(&contract).map_err(|e| e.to_string())?);
        if !matches!(task.target, TaskTarget::NewA4 { .. } | TaskTarget::NewDocument { .. })
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

pub(super) fn compact_context(session: &Session) -> Result<Value, String> {
    let state = session.doc().map_err(|e| e.to_string())?;
    let doc = &state.doc;
    // A small, measured font inventory prevents planning from assuming installed glyphs.
    let fonts = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
    let arabic_families: Vec<String> = fonts.available_arabic_families().into_iter().take(6).collect();
    let mut families: Vec<String> = arabic_families.iter().take(2).cloned().collect();
    for fallback in ["Tahoma", "Arial", "Noto Sans", "Noto Naskh Arabic", "DejaVu Sans"] {
        if families.len() >= 3 {
            break;
        }
        if fonts.has_family(fallback) && !families.iter().any(|f| f.eq_ignore_ascii_case(fallback)) {
            families.push(fallback.to_owned());
        }
    }
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
    let max_summary_pages = doc.page_count().min(128);
    let mut page_summaries: Vec<(Option<String>, Option<String>, Option<String>, usize)> = vec![(None, None, None, 0); max_summary_pages];
    for (spread_index, spread) in doc.spreads.iter().enumerate() {
        let first_page = doc.first_page_of_spread(spread_index);
        let mut stack: Vec<(&designcraft_engine::doc::Item, designcraft_geom::Affine, usize)> =
            spread.items.iter().rev().map(|it| (it.as_ref(), designcraft_geom::Affine::IDENTITY, 0usize)).collect();
        while let Some((item, parent_xf, depth)) = stack.pop() {
            let bounds = parent_xf.transform_rect_bbox(item.bounds());
            let page_offset = spread.page_at_x(bounds.center().x).unwrap_or(0);
            let p_idx = first_page.saturating_add(page_offset);
            if let Some(slot) = page_summaries.get_mut(p_idx) {
                slot.3 = slot.3.saturating_add(1);
                if let Some(tf) = item.text_frame()
                    && let Some(st) = doc.stories.get(&tf.story)
                {
                    let resolved_text = if st.text.contains('\u{E000}') || st.text.contains('\u{E001}') {
                        let sec_marker = doc.sections.iter().rev().find(|s| s.start <= p_idx).map(|s| s.marker.as_str()).unwrap_or("");
                        st.text.replace('\u{E000}', &doc.page_name(p_idx)).replace('\u{E001}', sec_marker)
                    } else {
                        st.text.clone()
                    };
                    let snippet: String = resolved_text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(64).collect();
                    if !snippet.is_empty() {
                        if item.label == "part.ui.header.unit-num" {
                            slot.0 = Some(snippet);
                        } else if item.label == "part.ui.footer.page" {
                            slot.2 = Some(snippet);
                        } else if slot.1.is_none() && (item.label.contains("title-main") || bounds.y0 < 160.0) {
                            slot.1 = Some(snippet);
                        }
                    }
                }
            }
            if depth < 6 && !item.children().is_empty() {
                let child_xf = parent_xf * item.xf;
                for child in item.children().iter().rev() {
                    stack.push((child.as_ref(), child_xf, depth + 1));
                }
            }
        }
    }
    let has_master_footer = crate::template_plan::has_relational_master_footer(state);
    let page_index: Vec<Value> = page_summaries
        .into_iter()
        .enumerate()
        .map(|(idx, (unit_badge, heading, mut footer_page, items))| {
            if footer_page.is_none() && items > 0 && has_master_footer && doc.page(idx).is_some_and(|pg| pg.show_parent_items && pg.parent.is_some())
            {
                footer_page = Some(doc.page_name(idx));
            }
            json!({
                "page": idx + 1,
                "unitBadge": unit_badge,
                "heading": heading,
                "footerPage": footer_page,
                "itemCount": items,
            })
        })
        .collect();
    Ok(
        json!({"documentUid":state.uid,"revision":state.revision,"title":doc.title,"pageCount":doc.page_count(),"pageWidth":doc.settings.page_width,"pageHeight":doc.settings.page_height,"facingPages":doc.settings.facing_pages,"rightToLeftBinding":doc.settings.right_to_left_binding,"selection":state.selection,"pageIndex":page_index,"objects":objects,"objectsTruncated":objects_truncated,"availableArabicFonts":arabic_families,"fontFaces":font_faces,"swatchNames":doc.swatches.iter().take(64).map(|s|s.name.chars().take(160).collect::<String>()).collect::<Vec<_>>(),"units":"points, 72 per inch; rect=[left,top,right,bottom]"}),
    )
}

pub(super) fn corrected_design_context(session: &Session) -> Result<Value, String> {
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
