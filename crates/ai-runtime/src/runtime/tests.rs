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
    runner.publish(&mut session, &Some(task), None, None, worker::Product::Plan(plan), &mut report).unwrap();
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
            ("templates/oversized.json".into(), vec![EngineAction { command: "frame.create".into(), params: json!({"text":"x".repeat(111_399)}) }]),
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
    assert!(bounded_task_skills(vec![guidance], &TaskTarget::CurrentDocument).is_err(), "Actual reference guidance must never be silently truncated");

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
        ("SKILL.md".into(), b"---\nname: automatic-fixture\ndescription: fixture automatic layout\n---\nUse editable native elements.\n".to_vec()),
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
    let error = runner.publish(&mut session, &Some(task), None, None, worker::Product::Plan(plan), &mut report).unwrap_err();
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
        let output =
            std::process::Command::new("cmd").creation_flags(0x08000000).args(["/D", "/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
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
    let error = runner.publish(&mut session, &Some(task), None, None, worker::Product::Plan(plan), &mut report).unwrap_err();
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
    file_access::confine(Some(&roots), || runner.launch(work, Some(task), Some((before.uid, before.revision)), vec![], Default::default())).unwrap();
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
            if let Some(sep) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..sep]).map_err(|error| error.to_string())?;
                let length = headers
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok()))
                    .ok_or("Local mock request needs Content-Length")?;
                if bytes.len().saturating_sub(sep + 4) >= length {
                    break;
                }
            }
        }
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len())
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
    let other_endpoint = designcraft_ai_types::ModelSelection { endpoint: Some("https://other-credential-review.invalid/v1/".into()), ..selection };
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
    let custom = designcraft_ai_types::ModelSelection { endpoint: Some("https://authorized-session-review.invalid/v1/".into()), ..canonical.clone() };
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
        ("kg-international-curriculum-architect", "International curriculum authoring, units, chapters, scope and sequence, and spiral mental chain"),
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
        expected_doc: None,
        expected_prefs: None,
        task: None,
        skills: vec![],
        settings: Default::default(),
        deferred_outcome: None,
        deferred_batch: None,
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
        sender.send(worker::Message::Diagnostic(json!({"kind":"nativePlanningValidation","attempt":attempt,"message":"bounded evidence"}))).unwrap();
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
    let error = runner.publish(&mut session, &Some(task.clone()), None, None, worker::Product::Plan(plan), &mut report).unwrap_err();
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
    runner.publish(&mut session, &Some(task), None, None, worker::Product::Plan(valid), &mut report).unwrap();
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
    assert!(runner.publish(&mut session, &Some(task), Some((uid, revision)), None, worker::Product::Plan(plan), &mut report).is_err());
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
        expected_doc: Some(before.doc.clone()),
        expected_prefs: Some(original_prefs),
        skills: vec![],
        settings,
        deferred_outcome: None,
        deferred_batch: None,
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
        expected_doc: Some(session.doc().unwrap().doc.clone()),
        expected_prefs: Some(session.prefs.clone()),
        task: Some(task),
        skills: Vec::new(),
        settings: Default::default(),
        deferred_outcome: None,
        deferred_batch: None,
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
    let snapshot = runner
        .handle(&mut session, AiRequest::PrintDocument { printer: None, grayscale: true, duplex: "off".into(), request_id: "already-sent".into() });
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
    let g1 =
        session.execute("frame.create", &json!({"spread":0,"rect":[300.0,36.0,420.0,140.0],"content":"graphic"})).unwrap()["id"].as_u64().unwrap();
    let g2 =
        session.execute("frame.create", &json!({"spread":0,"rect":[430.0,36.0,520.0,140.0],"content":"unassigned"})).unwrap()["id"].as_u64().unwrap();
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
    legacy_draft.request.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
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
        expected_doc: None,
        expected_prefs: None,
        skills: vec![],
        settings: request.settings,
        deferred_outcome: None,
        deferred_batch: None,
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

#[test]
fn ai31_selection_revision_bump_and_transient_interaction_preserve_current_document_publication() {
    let dir = tempfile::tempdir().unwrap();
    let mut runner = ProductionRunner::open(dir.path()).unwrap();
    let mut session = Session::new();
    session.execute("file.new", &new_a4(2)).unwrap();
    let created = session.execute("frame.create", &json!({"spread":0,"rect":[50,50,200,120]})).unwrap();
    let item_id = created["id"].as_u64().unwrap();
    let before = session.doc().unwrap().clone();

    let task = TaskRequest {
        prompt: "Add second frame".into(),
        settings: AiSettings::default(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let plan = ProductionPlan {
        title: "Added frame".into(),
        commands: vec![EngineAction { command: "frame.create".into(), params: json!({"spread":1,"rect":[60,60,220,140]}) }],
        images: vec![],
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    sender.send(worker::Message::Finished(worker::Outcome { result: Ok(worker::Product::Plan(plan)), usage: Default::default() })).unwrap();
    runner.pending = Some(Pending {
        roots: None,
        worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
        id: "ai31-ptr-eq".into(),
        started: Instant::now(),
        task: Some(task),
        expected: Some((before.uid, before.revision)),
        expected_doc: Some(before.doc.clone()),
        expected_prefs: Some(session.prefs.clone()),
        skills: vec![],
        settings: Default::default(),
        deferred_outcome: None,
        deferred_batch: None,
    });
    runner.snapshot.busy = true;

    // 1. User clicks/selects items while inspecting pages (which bumps `revision` without mutating `Arc<Document>`),
    // AND starts a transient canvas drag (`interaction = Some(...)`) right when `Finished` arrives:
    session.execute("selection.set", &json!({"ids":[item_id]})).unwrap();
    session.execute("selection.set", &json!({"ids":[]})).unwrap();
    assert_ne!(session.doc().unwrap().revision, before.revision, "selection commands bump revision");
    assert!(std::sync::Arc::ptr_eq(&session.doc().unwrap().doc, &before.doc));
    {
        let st = session.doc_mut().unwrap();
        st.interaction = Some(designcraft_engine::Interaction {
            label: "pan-drag".into(),
            doc: st.doc.clone(),
            selection: st.selection.clone(),
            preview: None,
            revision: st.revision,
        });
    }

    // Poll while interaction is active must defer instead of discarding the finished result!
    let waiting_snap = runner.handle(&mut session, AiRequest::Poll);
    assert_eq!(waiting_snap.stage, "waiting_for_idle");
    assert!(runner.pending.is_some(), "finished outcome must be held until canvas interaction ends");

    // Release interaction -> next Poll publishes cleanly despite bumped revision!
    session.doc_mut().unwrap().interaction = None;
    let done_snap = runner.handle(&mut session, AiRequest::Poll);
    assert_eq!(done_snap.stage, "completed", "msg={}", done_snap.message);
    assert_eq!(session.doc().unwrap().doc.spreads[1].items.len(), 1);
}

#[test]
fn agentic_publish_applies_multiple_chunks_in_single_undo_and_gates_deferred_images() {
    let dir = tempfile::tempdir().unwrap();
    let mut runner = ProductionRunner::open(dir.path()).unwrap();
    let mut session = Session::new();
    session.execute("file.new", &json!({"width": 595.0, "height": 842.0, "pages": 2, "facingPages": false})).unwrap();
    {
        let st = session.doc_mut().unwrap();
        st.history.undo.clear();
        st.history.redo.clear();
    }
    let before = session.doc().unwrap().clone();
    let settings = AiSettings {
        text_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None },
        image_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        image_rates: Some(TokenRates {
            input_per_million: 0.0,
            cached_input_per_million: 0.0,
            output_per_million: 0.0,
            cache_write_per_million: None,
            image_per_request: Some(0.04),
            image_reserve_usd: None,
        }),
        output_mode: designcraft_ai_types::OutputMode::NativeOnly,
        ..Default::default()
    };
    runner.snapshot.settings = settings.clone();

    let task = TaskRequest {
        prompt: "أصلح تنسيق الصفحتين واضبط الهوامش".into(),
        target: TaskTarget::CurrentDocument,
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        export_directory: None,
    };
    let (tx, receiver) = std::sync::mpsc::channel();
    tx.send(worker::Message::Finished(worker::Outcome {
        result: Ok(worker::Product::Agentic {
            title: "Polish Open Document".into(),
            summary: "Applied multi-chunk typography & layout fixes".into(),
            chunks: vec![
                vec![EngineAction {
                    command: "frame.create".into(),
                    params: json!({"spread": 0, "rect": [72.0, 72.0, 292.0, 132.0], "content": "text", "text": "Page 1 Header"}),
                }],
                vec![EngineAction {
                    command: "frame.create".into(),
                    params: json!({"spread": 1, "rect": [72.0, 72.0, 292.0, 132.0], "content": "text", "text": "Page 2 Header"}),
                }],
            ],
            deferred_images: vec![crate::doc_tools::DeferredImagePlan {
                companion_commands: vec![],
                images: vec![
                    ImagePlacement {
                        prompt: "Clean single pomegranate".into(),
                        page: 1,
                        rect: [72.0, 180.0, 232.0, 340.0],
                        label: "Pomegranate".into(),
                        family_id: None,
                        slot_id: None,
                        repeat_rects: vec![],
                        repeat_groups: vec![],
                    },
                    ImagePlacement {
                        prompt: "Clean single olive branch".into(),
                        page: 2,
                        rect: [72.0, 180.0, 232.0, 340.0],
                        label: "Olive Branch".into(),
                        family_id: None,
                        slot_id: None,
                        repeat_rects: vec![],
                        repeat_groups: vec![],
                    },
                ],
                summary: "2 composite illustrations queued for clean single-subject replacement".into(),
            }],
        }),
        usage: UsageSummary::default(),
    }))
    .unwrap();
    runner.pending = Some(Pending {
        roots: None,
        worker: worker::Worker { receiver, cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) },
        id: "agentic-job-1".into(),
        started: Instant::now(),
        task: Some(task),
        expected: Some((before.uid, before.revision)),
        expected_doc: Some(before.doc.clone()),
        expected_prefs: Some(session.prefs.clone()),
        skills: vec![],
        settings,
        deferred_outcome: None,
        deferred_batch: None,
    });
    runner.snapshot.busy = true;

    let snap = runner.handle(&mut session, AiRequest::Poll);
    assert_eq!(snap.stage, "completed", "msg={}", snap.message);
    // Both chunks were applied in ONE single Undo step!
    assert_eq!(session.doc().unwrap().history.undo.len(), 1);
    assert_eq!(session.doc().unwrap().doc.spreads[0].items.len(), 1);
    assert_eq!(session.doc().unwrap().doc.spreads[1].items.len(), 1);

    // Deferred paid cloud images are staged in PendingImageApproval with exact count & cost:
    let approval = snap.pending_image_approval.as_ref().expect("deferred image approval must be staged");
    assert_eq!(approval.job_id, "agentic-job-1");
    assert_eq!(approval.count, 2);
    assert_eq!(approval.provider, ProviderKind::Gemini);
    assert!((approval.estimated_cost_usd.unwrap() - 0.08).abs() < 1e-9);

    // Dismissing keeps the layout edits and clears the pending approval cleanly:
    let dismissed = runner.handle(&mut session, AiRequest::DismissImageRegeneration { job_id: "agentic-job-1".into() });
    assert!(dismissed.pending_image_approval.is_none());
    assert_eq!(session.doc().unwrap().doc.spreads[0].items.len(), 1);
    assert_eq!(session.doc().unwrap().doc.spreads[1].items.len(), 1);

    // Single Undo reverts BOTH chunks at once!
    session.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(session.doc().unwrap().doc.spreads[0].items.len(), 0);
    assert_eq!(session.doc().unwrap().doc.spreads[1].items.len(), 0);
}

#[test]
fn confirm_image_regeneration_promotes_native_only_non_gemini_and_sizes_request_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut runner = ProductionRunner::open(dir.path()).unwrap();
    let mut session = Session::new();
    session.execute("file.new", &json!({"width": 595.0, "height": 842.0, "pages": 1, "facingPages": false})).unwrap();
    let current = session.doc().unwrap().clone();

    // Bind a closed ephemeral loopback port so ComfyUI needs zero OS vault keys and never touches the internet.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let settings = AiSettings {
        output_mode: designcraft_ai_types::OutputMode::NativeOnly,
        verify_generated_images: true,
        text_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None },
        image_model: designcraft_ai_types::ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: Some(endpoint) },
        comfy_workflow: Some(json!({"1": {"class_type": "KSampler", "inputs": {"text": "{{PROMPT}}"}}})),
        budget: BudgetSpec { max_requests: 4, ..Default::default() },
        ..Default::default()
    };
    runner.snapshot.settings = settings.clone();
    let images: Vec<ImagePlacement> = (0..6)
        .map(|i| ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: vec![],
            repeat_groups: vec![],
            prompt: format!("Card {i}"),
            page: 0,
            rect: [72.0, 72.0 + (i as f64) * 30.0, 172.0, 96.0 + (i as f64) * 30.0],
            label: format!("Card {i}"),
        })
        .collect();
    let approval = PendingImageApproval {
        job_id: "job-confirm-6".into(),
        document_uid: current.uid,
        count: 6,
        provider: ProviderKind::ComfyUi,
        model: String::new(),
        estimated_cost_usd: Some(0.0),
        summary: "6 cards queued".into(),
    };
    runner.pending_image_regen = Some(PendingImageRegen {
        approval: approval.clone(),
        task: TaskRequest {
            prompt: "افحص الكتاب".into(),
            target: TaskTarget::CurrentDocument,
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            export_directory: None,
        },
        skills: vec![],
        style_profile: None,
        batches: vec![crate::doc_tools::DeferredImagePlan { companion_commands: vec![], images, summary: "6 cards".into() }],
        min_ppi: 150.0,
        expected_doc: current.doc.clone(),
    });
    runner.snapshot.pending_image_approval = Some(approval);

    // Confirming must promote NativeOnly -> NativeWithImages, clear incompatible verify_generated_images for OpenAI text model,
    // and raise max_requests from 4 to 6 so admission succeeds before worker dispatch!
    let confirmed = runner.handle(&mut session, AiRequest::ConfirmImageRegeneration { job_id: "job-confirm-6".into() });
    assert!(confirmed.busy, "confirm_image_regeneration must launch worker cleanly: msg={}", confirmed.message);
    let launched_settings = &runner.pending.as_ref().expect("worker launched").settings;
    assert_eq!(launched_settings.output_mode, designcraft_ai_types::OutputMode::NativeWithImages);
    assert!(!launched_settings.verify_generated_images, "OpenAI text model cannot run Gemini image review");
    assert_eq!(launched_settings.budget.max_requests, 6, "max_requests raised from 4 to cover all 6 confirmed images");
    let _ = runner.handle(&mut session, AiRequest::Cancel);
}
