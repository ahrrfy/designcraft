use super::panel_production::{
    batch_review, budget_targets, export_print, image_inspection_setting, render_diagnostic_alert_banner, saved_book_document,
};
use super::state::{BatchReviewKey, classify_diagnostic_alert};
use super::*;
use crate::Services;
use designcraft_engine::Session;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

fn harness_builder<State>() -> egui_kittest::HarnessBuilder<State> {
    egui_kittest::HarnessBuilder::default().with_context_setup(|ctx| {
        crate::theme::install_fonts(ctx, "");
    })
}

#[test]
fn ai23_remaining_book_allowance_is_explicit_and_does_not_change_review_size() {
    let mut state = AiUiState { prompt: "Create a science book".into(), pages: 48, ..Default::default() };
    state.settings.text_model.model = "fixture".into();
    state.settings.pages_per_batch = 2;
    state.settings.output_mode = OutputMode::NativeWithImages;
    let mut panel = harness_builder()
        .with_size(egui::vec2(1200.0, 1600.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 4);
    panel.get_by_label("Set request allowance for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 96);
    assert_eq!(panel.state().settings.pages_per_batch, 2);
    assert_eq!(panel.state().pages, 48);
}

#[test]
fn ai23_curriculum_preview_uses_the_injected_host_without_starting_production() {
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = calls.clone();
    let mut app = DesignApp::new(
        Session::new(),
        Services {
            ai: Some(Box::new(move |_, _| {
                seen.set(seen.get() + 1);
                AiSnapshot { available: true, message: "local preview".into(), ..Default::default() }
            })),
            ..Default::default()
        },
    );
    let request = TaskRequest {
        prompt: "Science book".into(),
        settings: AiSettings::default(),
        target: TaskTarget::NewA4 { pages: 48 },
        skill_ids: vec![],
        auto_select_skills: true,
        export_directory: None,
    };
    let result = request_command(&mut app, &json!({"method":"preview_production","request":request}));
    assert!(result.is_ok(), "Local curriculum preview should be available: {result:?}");
    assert_eq!(calls.get(), 1);
}

#[test]
fn ai23_proof_page_strip_navigates_without_reviewing_or_approving() {
    let fixture = ProofFixture::new();
    let calls = fixture.calls.clone();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    panel.get_by_label("Review page 2").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    assert_eq!(calls.get(), 0);
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
}

#[test]
fn ai23_spread_confirmation_requires_both_current_previews_and_separate_approval() {
    let mut fixture = ProofFixture::new();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.total_pages = 6;
    batch.pages_per_batch = 3;
    batch.completed_pages = 3;
    let report = &mut batch.batches[0];
    report.page_count = 3;
    report.proof_paths.push("page-3.png".into());
    report.proof_sha256.push("4".repeat(64));
    fixture.app.ui.ai.accept(fixture.checkpoint.clone());
    fixture.app.ui.ai.settings.facing_pages = true;
    let calls = fixture.calls.clone();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert!(panel.get_by_label("I reviewed both displayed pages; next preview").accesskit_node().is_disabled());
    panel.get_by_label("100% image preview").click();
    panel.run_steps(3);
    panel.get_by_label("I reviewed both displayed pages; next preview").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-1.png"));
    assert_eq!(calls.get(), 0, "spread review never launches the next group");
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
}

#[test]
fn ai20_ui_completed_discovery_stays_selectable_for_its_provider_and_endpoint() {
    let mut state = AiUiState { initialized: true, ..Default::default() };
    state.settings.text_model =
        ModelSelection { provider: ProviderKind::Gemini, model: "original".into(), endpoint: Some("https://models.example".into()) };
    let selection = state.settings.text_model.clone();
    state.pending_models = Some(selection.clone());
    state.accept(AiSnapshot { available: true, busy: true, stage: "discovering".into(), ..Default::default() });
    state.accept(AiSnapshot { available: true, stage: "completed".into(), models: vec!["text-a".into(), "text-b".into()], ..Default::default() });
    let mut panel = harness_builder()
        .with_size(egui::vec2(1200.0, 1200.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
    panel.run_steps(2);
    panel.get_by_label("text-a").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.text_model.model, "text-a");
    panel.state_mut().accept(AiSnapshot { available: true, stage: "completed".into(), ..Default::default() });
    panel.run_steps(3);
    panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
    panel.run_steps(2);
    panel.get_by_label("text-b").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.text_model.model, "text-b");
    panel.state_mut().settings.text_model.endpoint = Some("https://other.example".into());
    panel.run_steps(3);
    assert!(panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none());
    panel.state_mut().settings.text_model.endpoint = selection.endpoint;
    panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
    panel.run_steps(3);
    assert!(panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none());
}

#[test]
fn ai20_ui_natural_book_estimates_preserve_review_size_remaining_pages_and_explicit_budget() {
    let mut state = AiUiState { prompt: "Create a clear science book for children.".into(), pages: 12, ..Default::default() };
    state.settings.text_model.model = "fixture".into();
    state.settings.pages_per_batch = 6;
    let estimate = state.request_estimate(&[], None).expect("natural book briefs need a conservative planning allowance");
    assert_eq!(estimate.review_pages, 6);
    assert_eq!(estimate.allowance(), 6);
    let original_tokens = state.settings.budget.token_limit;
    let mut panel = harness_builder()
        .with_size(egui::vec2(1200.0, 1200.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 4);
    assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_none());
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 6);
    assert_eq!(panel.state().settings.pages_per_batch, 6);
    assert_eq!(panel.state().pages, 12);
    assert_eq!(panel.state().settings.budget.token_limit, original_tokens);

    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":48,"facingPages":false})).unwrap();
    for spread in 0..12 {
        session.execute("frame.create", &json!({"spread":spread,"rect":[20,20,100,80]})).unwrap();
    }
    let document = OpenDocument::from_document(3, session.active().unwrap());
    let mut continuing =
        AiUiState { prompt: "Finish the remaining science lessons.".into(), new_document: false, continue_book: true, ..Default::default() };
    continuing.settings.pages_per_batch = 40;
    continuing.settings.output_mode = OutputMode::NativeWithImages;
    continuing.settings.verify_generated_images = true;
    let estimate = continuing.request_estimate(&[document], Some(3)).unwrap();
    assert_eq!(estimate.review_pages, 36);
    assert_eq!(estimate.image_allowance, 36);
    assert_eq!(estimate.vision_allowance, 36);
    assert_eq!(estimate.allowance(), 108);
    assert_eq!(continuing.settings.pages_per_batch, 40);
    assert_eq!(continuing.settings.budget.max_requests, 4);
    continuing.continue_book = false;
    assert!(continuing.request_estimate(&[], None).is_none(), "ordinary edits do not use book planning estimates");
}

#[test]
fn ai20_ui_binding_choices_are_visible_and_existing_book_binding_is_authoritative() {
    let mut state = AiUiState { prompt: "Create a bilingual facing-page book.".into(), ..Default::default() };
    state.settings.text_model.model = "fixture".into();
    state.settings.facing_pages = true;
    state.settings.right_to_left_binding = Some(false);
    let mut panel = harness_builder()
        .with_size(egui::vec2(1300.0, 1400.0))
        .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
    panel.run_steps(3);
    assert!(panel.query_by_label("Left to right (LTR)").is_some());
    assert!(panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_none());
    for (label, expected) in [("Automatic (content language)", None), ("Right to left (RTL)", Some(true)), ("Left to right (LTR)", Some(false))] {
        panel.get_by_label(label).click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.right_to_left_binding, expected);
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert!(
            matches!(panel.state().1.last(), Some(AiRequest::Start(request)) if request.settings.right_to_left_binding == expected && request.settings.facing_pages)
        );
    }

    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":4,"facingPages":true})).unwrap();
    session.execute("layout.documentSetup", &json!({"binding":"rightToLeft"})).unwrap();
    session.execute("frame.create", &json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
    let documents = vec![OpenDocument::from_document(0, session.active().unwrap())];
    let mut state = AiUiState { prompt: "Continue this existing book.".into(), new_document: false, continue_book: true, ..Default::default() };
    state.settings.right_to_left_binding = Some(false);
    let mut existing = harness_builder()
        .with_size(egui::vec2(1300.0, 1400.0))
        .build_ui_state(move |ui, state| production(ui, state, false, true, &documents, &mut Some(0), &mut Vec::new()), state);
    existing.run_steps(3);
    assert!(existing.query_by_label("Existing book binding: Right to left (RTL)").is_some());
    assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
    assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
    assert!(existing.query_by_label("Automatic (content language)").is_none());
    assert_eq!(existing.state().settings.right_to_left_binding, Some(false), "viewing the existing binding does not rewrite new-book preferences");
    assert!(!existing.state().settings.facing_pages, "displaying preserved geometry does not rewrite new-book preferences");
    existing.state_mut().continue_book = false;
    existing.run_steps(3);
    assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
    assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
}

#[test]
fn ai20_ui_document_and_skill_export_destinations_are_independent_and_session_only() {
    let mut state = AiUiState { export_path: "book.pdf".into(), ..Default::default() };
    state.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
        id: "fixture-skill".into(),
        revision: "revision-one".into(),
        skill_md: String::new(),
        manifest_json: String::new(),
        sample_path: None,
        sample_valid: false,
        files: Vec::new(),
    });
    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1800.0)).build_ui_state(
        |ui, (state, requests, skill_tab)| {
            if *skill_tab {
                skills(ui, state, false, true, false, requests);
            } else {
                export_print(ui, state, false, true, requests);
            }
        },
        (state, Vec::new(), true),
    );
    panel.run_steps(3);
    panel.get_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("skill-package.zip")).focus();
    panel.run_steps(1);
    panel.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    panel.event(egui::Event::Text("portable.zip".into()));
    panel.run_steps(3);
    assert_eq!(panel.state().0.export_path, "book.pdf");
    panel.get_by_label("Export skill").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportSkill { path, .. }) if path == "portable.zip"));
    panel.state_mut().2 = false;
    panel.run_steps(3);
    panel.get_by_label("Export IDML").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportIdml { path }) if path == "book.idml"));
    panel.state_mut().2 = true;
    panel.run_steps(3);
    assert!(panel.query_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("portable.zip")).is_some());
    let ui = crate::UiState { ai: panel.state().0.clone(), ..Default::default() };
    let saved = serde_json::to_string(&ui).unwrap();
    assert!(!saved.contains("portable.zip") && !saved.contains("book.idml"));
}

fn continuation_request(target: TaskTarget) -> TaskRequest {
    let mut settings = AiSettings::default();
    settings.text_model.model = "continuation-fixture".into();
    settings.pages_per_batch = 3;
    TaskRequest {
        prompt: "Keep the approved opening pages and finish the original book.".into(),
        settings,
        skill_ids: vec!["original-book-style".into()],
        auto_select_skills: false,
        target,
        export_directory: Some("book-output".into()),
    }
}

fn saved_production_snapshot(request: &TaskRequest, document: &designcraft_engine::DocState) -> AiSnapshot {
    let mut snapshot = serde_json::to_value(AiSnapshot::default()).unwrap();
    snapshot["available"] = json!(true);
    snapshot["production_draft"] = json!({
        "id": "saved-book", "request": request, "document_uid": document.uid,
        "document_path": document.path, "document_title": document.title()
    });
    serde_json::from_value(snapshot).unwrap()
}

#[test]
fn continuation_ui_control_start_populates_the_composer_without_serializing_secrets() {
    let mut app = DesignApp::new(
        Session::new(),
        Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
    );
    let request = continuation_request(TaskTarget::ContinueBook);
    app.ui.ai.api_key.0 = "private-key-fixture".into();
    app.run("ai.request", json!({"method":"start", "request": request})).unwrap();
    assert_eq!(app.ui.ai.prompt, request.prompt);
    assert!(!app.ui.ai.new_document);
    assert!(app.ui.ai.continue_book);
    assert_eq!(app.ui.ai.settings.pages_per_batch, 3);
    assert!(!app.ui.ai.automatic_skills);
    assert!(app.ui.ai.selected_skills.contains("original-book-style"));
    assert_eq!(app.ui.ai.directory, "book-output");
    let saved_ui = serde_json::to_string(&app.ui).unwrap();
    assert!(!saved_ui.contains("private-key-fixture"));
    assert!(!saved_ui.contains(&request.prompt));
    assert!(app.ui.ai.reviewed_proofs.is_empty());
    assert!(app.ui.ai.batch_approval_gesture.is_none());
}

#[test]
fn continuation_ui_saved_request_hydrates_once_and_keeps_subsequent_edits() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":4})).unwrap();
    let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
    let snapshot = saved_production_snapshot(&request, session.active().unwrap());
    let mut state = AiUiState::default();
    state.accept(snapshot.clone());
    assert_eq!(state.prompt, request.prompt);
    assert_eq!(state.settings.pages_per_batch, 3);
    assert_eq!(state.pages, 4);
    state.prompt = "A manual revision awaiting Produce".into();
    state.accept(snapshot);
    assert_eq!(state.prompt, "A manual revision awaiting Produce");
    assert!(state.reviewed_proofs.is_empty());
    assert!(state.batch_approval_gesture.is_none());
}

#[test]
fn continuation_ui_explicit_action_keeps_current_document_edits_separate() {
    let mut state = AiUiState { new_document: false, ..Default::default() };
    state.prompt = "Edit the selected document".into();
    state.settings.text_model.model = "fixture".into();
    let documents = vec![OpenDocument {
        index: 0,
        uid: 1,
        path: None,
        title: "Book".into(),
        facing_pages: false,
        right_to_left_binding: false,
        pages: 4,
        remaining_pages: 3,
        continuable: true,
        filled_ranges: vec![[1, 1]],
        blank_ranges: vec![[2, 4]],
    }];
    let mut panel = harness_builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
        (state, Vec::new(), Some(0)),
    );
    panel.run_steps(3);
    panel.get_by_label("Continue remaining 3 pages").click();
    panel.run_steps(3);
    assert!(panel.state().1.is_empty(), "choosing continuation is not permission to start production");
    assert_eq!(panel.state().2, Some(0));
    assert_eq!(panel.state().0.prompt, "Edit the selected document");
    panel.get_by_label("Produce").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::ContinueBook, .. }))));
    panel.get_by_label("Edit current document").click();
    panel.run_steps(3);
    panel.get_by_label("Produce").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::CurrentDocument, .. }))));
}

#[test]
fn continuation_ui_busy_one_shot_keeps_failed_book_in_previous_task() {
    let state = AiUiState {
        snapshot: AiSnapshot { busy: true, batch: Some(BatchSnapshot { status: "failed".into(), ..Default::default() }), ..Default::default() },
        ..Default::default()
    };
    let mut panel = harness_builder()
        .with_size(egui::vec2(1200.0, 1200.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    assert!(panel.query_by_label("Previous task").is_some());
    assert!(panel.query_by_label("Page proof review").is_none(), "a failed old book is not the running one-shot task");
}

#[test]
fn continuation_ui_saved_book_preparation_requires_a_fresh_produce() {
    let mut app = DesignApp::new(Session::new(), Services::default());
    app.run("file.new", json!({"pages":4,"facingPages":false})).unwrap();
    app.run("frame.create", json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
    let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
    app.ui.ai.accept(saved_production_snapshot(&request, app.session.active().unwrap()));
    app.ui.ai.open = true;
    let starts = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let host_starts = starts.clone();
    let snapshot = app.ui.ai.snapshot.clone();
    app.services.ai = Some(Box::new(move |_, request| {
        if matches!(request, AiRequest::Start(_)) {
            host_starts.set(host_starts.get() + 1);
        }
        snapshot.clone()
    }));
    let mut panel = harness_builder().with_size(egui::vec2(1400.0, 1200.0)).build_ui_state(|ui, app| show(app, ui.ctx()), app);
    panel.run_steps(3);
    panel.get_by_label("Prepare saved book continuation").click();
    panel.run_steps(3);
    assert_eq!(starts.get(), 0);
    assert!(panel.state().ui.ai.continue_book);
    assert!(!panel.state().ui.ai.new_document);
    assert_eq!(panel.state().ui.ai.prompt, request.prompt);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    panel.get_by_label("Produce").click();
    panel.run_steps(3);
    assert_eq!(starts.get(), 1);
}

#[test]
fn continuation_ui_proof_document_navigation_does_not_review_or_approve() {
    let mut fixture = ProofFixture::new();
    let proof_uid = fixture.app.session.active().unwrap().uid;
    fixture.app.run("file.new", json!({"pages":2})).unwrap();
    assert_ne!(fixture.app.session.active().unwrap().uid, proof_uid);
    let calls = fixture.calls.clone();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    panel.get_by_label("Open proof document").click();
    panel.run_steps(3);
    assert_eq!(panel.state().session.active().unwrap().uid, proof_uid);
    assert!(panel.state().ui.ai.proof_document_matches);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    assert!(!panel.state().ui.ai.can_approve_batch());
    assert_eq!(calls.get(), 0, "document navigation must not call the AI host");
}

#[test]
fn continuation_ui_saved_path_precedes_reused_uid_and_restored_targets_require_choice() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":4})).unwrap();
    let request = continuation_request(TaskTarget::ContinueBook);
    let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
    let mut value = serde_json::to_value(&snapshot).unwrap();
    value["production_draft"]["document_path"] = json!("original-book.designcraft");
    snapshot = serde_json::from_value(value).unwrap();
    let mut state = AiUiState::default();
    state.accept(snapshot);
    let mut wrong = OpenDocument::from_document(0, session.active().unwrap());
    wrong.path = Some("different-book.designcraft".into());
    wrong.continuable = true;
    let documents = [wrong];
    assert!(saved_book_document(state.snapshot.production_draft.as_ref().unwrap(), &documents, Some(0)).is_none());
    assert_eq!(
        state.production_issue(true, true, false),
        Some("Choose the target book or prepare its remaining pages before producing the saved request.")
    );
}

#[test]
fn continuation_ui_retry_requires_live_retryability_and_exact_selected_document() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":4})).unwrap();
    let document = session.active().unwrap();
    let request = continuation_request(TaskTarget::ContinueBook);
    let mut snapshot = saved_production_snapshot(&request, document);
    snapshot.batch = Some(BatchSnapshot {
        job_id: "saved-book".into(),
        status: "failed".into(),
        retryable: true,
        document_uid: Some(document.uid),
        document_revision: Some(document.revision),
        ..Default::default()
    });
    let mut state = AiUiState::default();
    state.accept(snapshot);
    let documents = vec![OpenDocument::from_document(0, document)];
    let mut panel = harness_builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
        |ui, (state, requests, selected, documents)| production(ui, state, false, true, documents, selected, requests),
        (state, Vec::new(), Some(0), documents),
    );
    panel.run_steps(3);
    assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
    panel.get_by_label("Retry failed review group").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
    let batch = panel.state_mut().0.snapshot.batch.as_mut().unwrap();
    batch.document_revision = batch.document_revision.map(|revision| revision + 1);
    panel.run_steps(3);
    assert!(
        !panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
        "view revisions do not change the selected book's identity"
    );
    panel.state_mut().3.first_mut().unwrap().uid += 1;
    panel.run_steps(3);
    assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "a different book cannot retry this task");
    panel.state_mut().2 = None;
    panel.run_steps(3);
    assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().retryable = false;
    panel.run_steps(3);
    assert!(panel.query_by_label("Retry failed review group").is_none(), "a persisted failure never grants retry authority");
    assert!(panel.state().0.reviewed_proofs.is_empty());
    assert!(panel.state().0.batch_approval_gesture.is_none());
}

#[test]
fn continuation_ui_unpublished_new_book_retry_does_not_require_an_open_document() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":4})).unwrap();
    let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
    let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
    snapshot.production_draft.as_mut().unwrap().document_uid = None;
    snapshot.batch = Some(BatchSnapshot { job_id: "saved-book".into(), status: "failed".into(), retryable: true, ..Default::default() });
    let mut state = AiUiState::default();
    state.accept(snapshot);
    let mut panel = harness_builder()
        .with_size(egui::vec2(1200.0, 1200.0))
        .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
    panel.run_steps(3);
    assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
    panel.get_by_label("Retry failed review group").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
    panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::ContinueBook;
    panel.run_steps(3);
    assert!(
        panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
        "an existing book still needs an exact selected document"
    );
    panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::NewA4 { pages: 4 };
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().completed_pages = 1;
    panel.run_steps(3);
    assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "published pages cannot use document-free retry");
}

#[test]
fn continuation_ui_view_only_revision_requests_one_repin_and_preserves_exact_proof_review() {
    let mut fixture = ProofFixture::new();
    fixture.confirm_all();
    let original = fixture.app.session.active().unwrap().doc.clone();
    let pinned = original.clone();
    let mut checkpoint = fixture.checkpoint.clone();
    let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let host_polls = polls.clone();
    fixture.app.services.ai = Some(Box::new(move |session, request| {
        assert!(matches!(request, AiRequest::Poll), "revision validation cannot approve or start work");
        host_polls.set(host_polls.get() + 1);
        let current = session.active().unwrap();
        if std::sync::Arc::ptr_eq(&current.doc, &pinned) {
            checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
        }
        checkpoint.clone()
    }));
    let mut panel = fixture.panel();
    panel.run_steps(3);
    let reviewed = panel.state().ui.ai.reviewed_proofs.clone();
    let proof_path = panel.state().ui.ai.proof_path.clone();
    assert_eq!(reviewed.len(), 2);
    for (expected_polls, command, params) in [(1, "selection.set", json!({"ids":[]})), (2, "layout.selectPage", json!({"page":1}))] {
        let before = panel.state().session.active().unwrap().revision;
        panel.state_mut().run(command, params).unwrap();
        assert_ne!(panel.state().session.active().unwrap().revision, before);
        assert!(std::sync::Arc::ptr_eq(&panel.state().session.active().unwrap().doc, &original));
        panel.run_steps(5);
        assert_eq!(polls.get(), expected_polls, "view-only revision changes request one local validation");
        assert_eq!(panel.state().ui.ai.reviewed_proofs, reviewed);
        assert_eq!(panel.state().ui.ai.proof_path, proof_path);
        assert!(panel.state().ui.ai.can_approve_batch());
        assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
    }
    panel.state_mut().run("frame.create", json!({"spread":0,"rect":[20,20,80,80]})).unwrap();
    panel.run_steps(5);
    assert_eq!(polls.get(), 3);
    assert!(!panel.state().ui.ai.proof_document_matches);
    assert!(!panel.state().ui.ai.can_approve_batch(), "an actual content edit cannot be repinned by the host");
    panel.run_steps(5);
    assert_eq!(polls.get(), 3, "a rejected content revision must not trigger repeated validation polls");
    assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
}

#[test]
fn continuation_ui_changed_view_can_return_to_its_book_before_revision_validation() {
    let mut fixture = ProofFixture::new();
    let original = fixture.app.session.active().unwrap().clone();
    let uid = original.uid;
    let mut checkpoint = fixture.checkpoint.clone();
    let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let host_polls = polls.clone();
    fixture.app.services.ai = Some(Box::new(move |session, request| {
        assert!(matches!(request, AiRequest::Poll));
        host_polls.set(host_polls.get() + 1);
        let current = session.active().unwrap();
        if current.uid == uid && std::sync::Arc::ptr_eq(&current.doc, &original.doc) {
            checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
        }
        checkpoint.clone()
    }));
    fixture.app.run("selection.set", json!({"ids":[]})).unwrap();
    fixture.app.run("file.new", json!({"pages":2})).unwrap();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    assert_eq!(polls.get(), 0);
    panel.get_by_label("Open proof document").click();
    panel.run_steps(5);
    assert_eq!(panel.state().session.active().unwrap().uid, uid);
    assert_eq!(polls.get(), 1);
    assert!(panel.state().ui.ai.proof_document_matches);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    assert!(!panel.state().ui.ai.can_approve_batch(), "returning to the book never reviews its pages");
    assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
}
#[test]
fn production_composer_exposes_model_prompt_and_action_without_advanced_settings() {
    let mut app = DesignApp::new(Session::new(), Services::default());
    app.ui.ai.open = true;
    app.ui.ai.snapshot.available = true;
    app.ui.ai.initialized = true;
    app.ui.ai.prompt = "A long request\n".repeat(200);
    let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
    harness.run_steps(3);
    assert!(harness.query_by_label("Model or agent").is_some());
    assert!(harness.query_by_label("What would you like to create?").is_some());
    assert!(harness.query_by_label("Produce").is_some());
    assert!(harness.get_by_label("Produce").rect().max.y < 650.0, "long briefs must not push the action off screen");
    assert!(harness.query_by_label("Input").is_none(), "pricing belongs in advanced settings");
}
#[test]
fn disabled_production_button_explains_the_missing_model() {
    let mut app = DesignApp::new(
        Session::new(),
        Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
    );
    app.run("ai.production", json!({})).unwrap();
    app.ui.ai.prompt = "Create an editable activity".into();
    let mut harness = crate::test_window::open(app, egui::vec2(1500.0, 1100.0));
    harness.run_steps(3);
    assert!(harness.get_by_label("Produce").accesskit_node().is_disabled());
    assert!(harness.query_by_label("Choose a text model above.").is_some(), "a disabled button must explain what to fix");
    harness.state_mut().app.ui.ai.settings.text_model.model = "fixture-model".into();
    harness.run_steps(3);
    assert!(!harness.get_by_label("Produce").accesskit_node().is_disabled());
    assert!(harness.query_by_label("Choose a text model above.").is_none());
}

#[test]
fn production_readiness_preserves_workflow_budget_and_human_review_guards() {
    let mut state = AiUiState { prompt: "Native activity".into(), ..Default::default() };
    state.settings.text_model.model = "text-model".into();
    state.settings.output_mode = OutputMode::NativeWithImages;
    state.settings.image_model.provider = ProviderKind::Gemini;
    assert_eq!(state.production_issue(true, false, false), Some("Choose an image model in Providers and keys."));
    state.settings.image_model.model = "image-model".into();
    state.settings.budget.max_cost_usd = Some(1.8);
    assert_eq!(state.production_issue(true, false, true), Some("أدخل أسعار المزود وأكّدها من الإعدادات المتقدمة."));
    state.rates_confirmed = true;
    assert_eq!(state.production_issue(true, false, false), None);
    state.settings.image_model.provider = ProviderKind::ComfyUi;
    state.settings.image_model.model.clear();
    assert_eq!(state.production_issue(true, false, false), Some("Complete a valid ComfyUI API workflow in Providers and keys."));
    state.workflow_json = r#"{"node":{"text":"{{prompt}}"}}"#.into();
    state.edit_workflow();
    assert_eq!(state.production_issue(true, false, false), None, "a workflow can specify its checkpoint without a model placeholder");
    state.snapshot.batch = Some(BatchSnapshot { status: "awaiting_human_proof".into(), ..Default::default() });
    assert_eq!(state.production_issue(true, false, false), Some("Review and approve the current batch, or cancel the task before starting another."));
    assert!(!state.can_approve_batch());
}

#[test]
fn explicit_page_review_estimate_changes_request_allowance_only_after_the_button() {
    let mut state = AiUiState { prompt: (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect(), ..Default::default() };
    state.pages = 8;
    state.settings.pages_per_batch = 6;
    state.settings.text_model.model = "fixture-model".into();
    let token_limit = state.settings.budget.token_limit;
    let mut panel = harness_builder()
        .with_size(egui::vec2(1000.0, 1100.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 0 images = 6").is_some());
    assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_some());
    assert_eq!(panel.state().settings.budget.max_requests, 4, "an estimate never raises the spending allowance itself");
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 6);
    assert_eq!(panel.state().settings.pages_per_batch, 6);
    assert_eq!(panel.state().pages, 8);
    assert_eq!(panel.state().settings.budget.token_limit, token_limit);

    panel.state_mut().settings.output_mode = OutputMode::NativeWithImages;
    panel.run_steps(3);
    assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images = 12").is_some());
    assert_eq!(panel.state().settings.budget.max_requests, 6, "provisional image allowance requires another explicit choice");
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 12);
    assert_eq!(panel.state().settings.pages_per_batch, 6);
    panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
    panel.state_mut().settings.verify_generated_images = true;
    panel.run_steps(3);
    assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images + 6 vision checks = 18").is_some());
    assert_eq!(panel.state().settings.budget.max_requests, 12, "opting into inspection still does not raise request allowance implicitly");
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().settings.budget.max_requests, 18);
    assert_eq!(panel.state().settings.pages_per_batch, 6);
    assert_eq!(panel.state().settings.budget.token_limit, token_limit);
}

#[test]
fn image_inspection_requires_explicit_gemini_choice_and_can_be_disabled_after_provider_change() {
    let mut panel = harness_builder()
        .with_size(egui::vec2(900.0, 400.0))
        .build_ui_state(|ui, state| image_inspection_setting(ui, state, false), AiUiState::default());
    let label = "Inspect generated images with Gemini (extra billed vision requests)";
    panel.run_steps(3);
    assert!(panel.get_by_label(label).accesskit_node().is_disabled());
    assert!(!panel.state().settings.verify_generated_images);
    panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
    panel.run_steps(3);
    assert!(!panel.get_by_label(label).accesskit_node().is_disabled());
    panel.get_by_label(label).click();
    panel.run_steps(3);
    assert!(panel.state().settings.verify_generated_images);
    assert_eq!(panel.state().settings.budget.max_requests, 4);
    assert_eq!(panel.state().settings.pages_per_batch, 2);
    panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
    panel.run_steps(3);
    assert!(panel.get_by_label(label).accesskit_node().is_disabled());
    assert!(panel.state().settings.verify_generated_images, "provider changes do not silently change an explicit inspection choice");
    panel.get_by_label("Turn off image inspection").click();
    panel.run_steps(3);
    assert!(!panel.state().settings.verify_generated_images);
    assert_eq!(panel.state().settings.budget.max_requests, 4);
    assert_eq!(panel.state().settings.pages_per_batch, 2);
}

#[test]
fn continuation_request_estimate_counts_only_remaining_pages_and_ignores_fenced_examples() {
    let prompt = format!(
        "```markdown\n### PAGE 99 — Example only\n```\n{}",
        (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect::<String>()
    );
    let mut state = AiUiState { prompt, new_document: false, continue_book: true, ..Default::default() };
    state.settings.pages_per_batch = 6;
    state.settings.output_mode = OutputMode::NativeWithImages;
    let document = OpenDocument {
        index: 3,
        uid: 1,
        path: None,
        title: "Book".into(),
        facing_pages: false,
        right_to_left_binding: false,
        pages: 8,
        remaining_pages: 2,
        continuable: true,
        filled_ranges: vec![[1, 6]],
        blank_ranges: vec![[7, 8]],
    };
    let estimate = state.request_estimate(&[document], Some(3)).unwrap();
    assert_eq!(estimate.review_pages, 2);
    assert_eq!(estimate.image_allowance, 2);
    assert_eq!(state.settings.budget.max_requests, 4);
}

#[test]
fn facing_page_choice_preserves_automatic_binding_without_another_direction_prompt() {
    let state = AiUiState::default();
    let mut panel = harness_builder()
        .with_size(egui::vec2(1000.0, 1100.0))
        .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
    panel.run_steps(3);
    assert!(!panel.state().settings.facing_pages);
    panel.get_by_label("Facing pages").click();
    panel.run_steps(3);
    assert!(panel.state().settings.facing_pages);
    assert_eq!(panel.state().settings.right_to_left_binding, None);
    assert!(panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_some());
}
#[test]
fn facing_book_continuation_counts_blank_pages_inside_a_partly_filled_pair() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":360,"height":480,"pages":5,"facingPages":true})).unwrap();
    let add_page_item = |session: &mut Session, absolute| {
        let document = session.doc().unwrap();
        let (spread, page) = document.doc.page_loc(absolute).unwrap();
        let x = document.doc.spreads[spread].pages[page].x;
        session.execute("frame.create", &json!({"spread":spread,"rect":[x+20.0,20.0,x+100.0,80.0]})).unwrap();
    };
    add_page_item(&mut session, 0);
    add_page_item(&mut session, 1);
    assert!(session.doc().unwrap().doc.spreads.iter().any(|spread| spread.pages.len() == 2));
    let book = OpenDocument::from_document(0, session.doc().unwrap());
    assert!(book.continuable);
    assert_eq!(book.pages, 5);
    assert_eq!(book.remaining_pages, 3, "the blank right page of the first pair is part of the suffix");
    add_page_item(&mut session, 3);
    assert!(!OpenDocument::from_document(0, session.doc().unwrap()).continuable, "a later filled page cannot bypass a blank gap");
}

#[test]
fn automatic_proof_binding_uses_print_content_metadata_before_instruction_language() {
    assert!(!automatic_proof_binding("صمّم كتاباً للأطفال\nContent Copy Language: English", true));
    assert!(automatic_proof_binding("Script Language: English\nContent Copy Language: English LTR + Vocalized Arabic RTL", false));
    assert!(!automatic_proof_binding("تعليمات عربية: محتوى الكتاب باللغة الإنجليزية", true));
    assert!(automatic_proof_binding("Binding: RTL\nContent Copy Language: English", false));
}

#[test]
fn long_bilingual_composer_keeps_the_full_brief_and_uses_the_shared_limit() {
    let mut state = AiUiState { prompt: "تعليمات المنهج Mathematics\n".repeat(4000), ..Default::default() };
    state.settings.text_model.model = "fixture-model".into();
    let original = state.prompt.clone();
    assert!(original.len() > 32768);
    assert_eq!(state.production_issue(true, false, false), None);
    assert_eq!(state.prompt, original);
    state.prompt = "x".repeat(designcraft_ai_types::protocol::MAX_BRIEF_BYTES + 1);
    assert!(state.production_issue(true, false, false).is_some());
}
#[test]
fn local_resource_preset_keeps_the_requested_human_review_size() {
    let mut app = DesignApp::new(
        Session::new(),
        Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
    );
    app.ui.ai.open = true;
    app.ui.ai.initialized = true;
    app.ui.ai.snapshot.available = true;
    app.ui.ai.tab = 4;
    app.ui.ai.settings.pages_per_batch = 12;
    let mut harness = crate::test_window::open(app, egui::vec2(1800.0, 1500.0));
    harness.run_steps(3);
    harness.get_by_label("Conservative CPU (Ollama)").click();
    harness.run_steps(3);
    assert_eq!(harness.state().app.ui.ai.settings.pages_per_batch, 12);
    assert_eq!(harness.state().app.ui.ai.settings.resources.cpu_threads, 2);
}
#[test]
fn opened_document_target_is_visible_without_creating_or_resizing_it() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":360,"height":480,"pages":3,"facingPages":false})).unwrap();
    let mut app = DesignApp::new(session, Services::default());
    app.ui.ai.open = true;
    app.ui.ai.initialized = true;
    app.ui.ai.new_document = false;
    let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
    harness.run_steps(3);
    assert!(harness.query_by_label("Target document").is_some());
    let state = harness.state().app.session.doc().unwrap();
    assert_eq!(state.doc.page_count(), 3);
    assert_eq!(state.doc.settings.page_width, 360.0);
    assert!(!state.is_dirty());
}

#[test]
fn requested_large_review_group_keeps_every_page_in_the_human_proof() {
    let fixture = ProofFixture::new();
    let mut batch = fixture.checkpoint.batch.clone().unwrap();
    batch.total_pages = 64;
    batch.pages_per_batch = 64;
    batch.total_batches = 1;
    batch.completed_pages = 64;
    let report = &mut batch.batches[0];
    report.page_count = 64;
    report.proof_paths = vec!["document.designcraft".into(), "proof-1.pdf".into(), "proof-2.pdf".into()];
    report.proof_paths.extend((1..=64).map(|page| format!("page-{page}.png")));
    report.proof_sha256 = vec!["a".repeat(64); report.proof_paths.len()];
    let key = BatchReviewKey::from_batch(&batch).expect("A complete 64-page review group must not be rejected by an internal 32-page limit");
    assert_eq!(key.pages().count(), 64);
    let report = &mut batch.batches[0];
    report.proof_paths.pop();
    report.proof_sha256.pop();
    assert!(BatchReviewKey::from_batch(&batch).is_none(), "Every requested page still needs its own proof");
}

#[test]
fn callback_dispatch_and_manual_settings_survive_poll() {
    let mut app = DesignApp::new(
        Session::new(),
        Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
    );
    app.run("ai.production", json!({})).unwrap();
    app.ui.ai.settings.text_model.model = "manual-model".into();
    app.run("ai.request", json!({"method":"poll"})).unwrap();
    assert_eq!(app.ui.ai.settings.text_model.model, "manual-model");
    assert!(app.ui.ai.open);
}
#[test]
fn session_key_drafts_and_prompts_never_serialize() {
    let mut ui = crate::UiState::default();
    ui.ai.api_key.0 = "secret-fixture".into();
    ui.ai.prompt = "private-prompt".into();
    let serialized = serde_json::to_string(&ui).unwrap();
    assert!(!serialized.contains("secret-fixture"));
    assert!(!serialized.contains("private-prompt"));
    assert!(!format!("{:?}", ui.ai).contains("secret-fixture"));
}
#[test]
fn approval_requires_valid_review_and_host_absence_is_an_error() {
    let mut app = DesignApp::new(Session::new(), Services::default());
    assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"x", "revision":"1"})).is_err());
    assert!(dispatch(&mut app, AiRequest::Poll).is_err());
}

#[test]
fn partial_workflow_draft_survives_poll_and_becomes_runnable_when_completed() {
    let mut state = AiUiState::default();
    state.accept(AiSnapshot { available: true, ..Default::default() });
    state.settings.output_mode = OutputMode::NativeWithImages;
    state.settings.comfy_workflow = Some(json!({"stale": true}));
    state.workflow_json = r#"{"node":{"text":"{{prompt}}""#.into();
    state.edit_workflow();
    let partial = state.workflow_json.clone();
    assert!(!state.workflow_error.is_empty());
    assert!(state.settings.comfy_workflow.is_none());
    assert!(!state.workflow_ready());
    state.accept(AiSnapshot { available: true, ..Default::default() });
    assert_eq!(state.workflow_json, partial);
    assert!(!state.workflow_error.is_empty());
    state.workflow_json.push_str(r#", "model":"{{model}}"}}"#);
    state.edit_workflow();
    assert!(state.workflow_error.is_empty());
    assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
    assert!(state.workflow_ready());
    state.accept(AiSnapshot { available: true, ..Default::default() });
    assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
}

#[test]
fn automation_cannot_approve_even_a_valid_review_or_call_the_host() {
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let host_calls = calls.clone();
    let mut app = DesignApp::new(
        Session::new(),
        Services {
            ai: Some(Box::new(move |_, _| {
                host_calls.set(host_calls.get() + 1);
                AiSnapshot { available: true, ..Default::default() }
            })),
            ..Default::default()
        },
    );
    app.ui.ai.snapshot.available = true;
    app.ui.ai.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
        id: "fixture".into(),
        revision: "1".into(),
        skill_md: "reviewed".into(),
        manifest_json: "{}".into(),
        sample_path: Some("sample.png".into()),
        sample_valid: true,
        files: vec![],
    });
    assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"fixture", "revision":"1"})).is_err());
    app.ui.ai.snapshot.batch = Some(designcraft_ai_types::protocol::BatchSnapshot {
        job_id: "batch-fixture".into(),
        status: "awaiting_human_proof".into(),
        checkpoint: 7,
        ..Default::default()
    });
    assert!(app.run("ai.request", json!({"method":"approve_batch", "job_id":"batch-fixture", "checkpoint":7})).is_err());
    assert!(app.run("ai.request", json!({"method":"remove_api_key", "selection":{"provider":"open_ai","model":"fixture","endpoint":null}})).is_err());
    assert!(app.run("ai.request", json!({"method":"refresh_credentials"})).is_err());
    app.ui.ai.snapshot.print_proof = Some(designcraft_ai_types::protocol::PrintProof {
        token: "frozen-fixture".into(),
        sha256: "fixture-hash".into(),
        bytes: 64,
        pages: 1,
        manifest: json!({}),
        pdf_path: Some("fixture.pdf".into()),
    });
    assert!(app.run("ai.request", json!({"method":"submit_prepared_print", "token":"frozen-fixture", "request_id":"fixture-print"})).is_err());
    assert!(app.run("ai.request", json!({"method":"open_print_proof", "token":"frozen-fixture"})).is_err());
    assert!(app.run("ai.request", json!({"method":"start_local_ollama", "executable":"C:/fixture/ollama.exe"})).is_err());
    assert!(app.run("ai.request", json!({"method":"stop_local_ollama"})).is_err());
    assert_eq!(calls.get(), 0);
}

struct ProofFixture {
    app: DesignApp,
    ctx: egui::Context,
    checkpoint: AiSnapshot,
    calls: std::rc::Rc<std::cell::Cell<u32>>,
}
#[test]
fn ai24_recovered_proofs_remain_visible_without_restoring_approval_authority() {
    let mut fixture = ProofFixture::new();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.status = "interrupted".into();
    batch.checkpoint = 0;
    batch.document_uid = None;
    batch.document_revision = None;
    fixture.app.ui.ai.accept(fixture.checkpoint.clone());
    assert_eq!(fixture.app.ui.ai.proof_path.as_deref(), Some("page-1.png"));
    assert!(fixture.app.ui.ai.proof_key.is_none());
    assert!(fixture.app.ui.ai.select_proof("page-2.png"));
    fixture.view("page-2.png");
    fixture.app.ui.ai.review_selected_proof();
    assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
    assert!(!fixture.app.ui.ai.can_review_selected_proof());
    assert_eq!(fixture.calls.get(), 0);
}
#[test]
fn ai24_saved_proof_navigation_and_return_to_composer_do_not_approve_or_generate() {
    let mut fixture = ProofFixture::new();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.status = "interrupted".into();
    batch.document_uid = None;
    batch.document_revision = None;
    fixture.app.ui.ai.accept(fixture.checkpoint.clone());
    let calls = fixture.calls.clone();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    assert!(panel.query_by_label("Selected page proof").is_some());
    assert!(panel.query_by_label("Approve and generate next review group").is_none());
    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    panel.get_by_label("Return to production settings").click();
    panel.run_steps(3);
    assert!(panel.query_by_label("What would you like to create?").is_some());
    assert_eq!(calls.get(), 0);
}

#[test]
fn ai24_failed_unpublished_group_explains_why_there_are_no_proofs_to_approve() {
    let mut fixture = ProofFixture::new();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.status = "failed".into();
    batch.batches[0].proof_paths.clear();
    batch.batches[0].proof_sha256.clear();
    fixture.app.ui.ai.accept(fixture.checkpoint);
    let mut panel = harness_builder().build_ui_state(|ui, state| batch_review(ui, state, false, &mut Vec::new()), fixture.app.ui.ai);
    panel.run_steps(3);
    assert!(panel.query_by_label("This group has no complete published page proofs. Resolve the reported error and retry the unfinished pages; there is nothing new to approve.").is_some());
    assert!(panel.query_by_label("Approve and generate next review group").is_none());
    assert!(panel.query_by_label("Selected page proof").is_none());
}
impl ProofFixture {
    fn new() -> Self {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_calls = calls.clone();
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255])));
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                read: Some(Box::new(move |_| Ok(png.clone()))),
                ai: Some(Box::new(move |_, _| {
                    host_calls.set(host_calls.get() + 1);
                    AiSnapshot { available: true, ..Default::default() }
                })),
                ..Default::default()
            },
        );
        app.run("file.new", json!({"facingPages":false})).unwrap();
        let document = app.session.active().map(|d| (d.uid, d.revision)).unwrap();
        let checkpoint = AiSnapshot {
            available: true,
            batch: Some(BatchSnapshot {
                job_id: "two-page-proof".into(),
                total_pages: 4,
                pages_per_batch: 2,
                current_batch: 1,
                total_batches: 2,
                completed_pages: 2,
                status: "awaiting_human_proof".into(),
                checkpoint: 1,
                document_uid: Some(document.0),
                document_revision: Some(document.1),
                batches: vec![designcraft_ai_types::protocol::BatchReport {
                    batch: 1,
                    page_count: 2,
                    proof_paths: vec!["book.designcraft".into(), "proof.pdf".into(), "page-1.png".into(), "page-2.png".into()],
                    proof_sha256: vec!["0".repeat(64), "1".repeat(64), "2".repeat(64), "3".repeat(64)],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        app.ui.ai.accept(checkpoint.clone());
        app.ui.ai.sync_proof_document(Some(document));
        app.ui.ai.review_input_is_human = true;
        Self { app, ctx: egui::Context::default(), checkpoint, calls }
    }

    fn view(&mut self, path: &str) {
        self.app.ui.ai.proof_path = Some(path.into());
        self.app.ui.ai.proof_loaded = None;
        load_proof_preview(&mut self.app, &self.ctx);
    }

    fn confirm_all(&mut self) {
        let pages: Vec<_> = self.app.ui.ai.proof_key.as_ref().unwrap().pages().map(|(path, _)| path.clone()).collect();
        for path in pages {
            self.view(&path);
            self.app.ui.ai.review_selected_proof();
        }
    }

    fn panel(mut self) -> egui_kittest::Harness<'static, DesignApp> {
        self.app.ui.ai.open = true;
        self.app.ui.ai.prompt = "A long production request\n".repeat(1000);
        harness_builder().with_size(egui::vec2(1024.0, 768.0)).build_ui_state(|ui, app| show(app, ui.ctx()), self.app)
    }
}

#[test]
fn production_review_keeps_actions_beside_the_proof_and_tracks_each_displayed_page() {
    let fixture = ProofFixture::new();
    let calls = fixture.calls.clone();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    let image = panel.get_by_label("Selected page proof").rect();
    let review = panel.get_by_label("I reviewed the displayed page").rect();
    let approval = panel.get_by_label("Approve and generate next review group").rect();
    assert!(review.min.x >= image.max.x, "page confirmation belongs beside its displayed preview");
    assert!(approval.min.x >= image.max.x, "group approval belongs beside the preview");
    assert!(image.contains(egui::pos2(image.center().x, review.center().y)));
    assert!(image.contains(egui::pos2(image.center().x, approval.center().y)));
    assert!(image.max.y < 768.0 && approval.max.y < 768.0, "long requests and file lists must not require scrolling to approve");
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    assert!(panel.get_by_label("Previous").accesskit_node().is_disabled());
    assert!(panel.query_by_label("Pages reviewed: 0 / 2").is_some());

    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert!(panel.query_by_label("Reviewed").is_some());
    assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-1.png".into(), "2".repeat(64))]));
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());

    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
    assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-2.png"));
    assert!(panel.query_by_label("Pages reviewed: 1 / 2").is_some());
    assert!(panel.get_by_label("Next").accesskit_node().is_disabled());
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1, "navigation and loading never review the next page");

    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    assert!(!panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    assert_eq!(calls.get(), 0, "per-page review never submits group approval");
}

#[test]
fn busy_production_shows_live_usage_and_keeps_full_diagnostics_in_details() {
    let mut fixture = ProofFixture::new();
    fixture.app.ui.ai.snapshot.busy = true;
    fixture.app.ui.ai.snapshot.current_usage = designcraft_ai_types::protocol::UsageSummary {
        requests: 3,
        input_tokens: 1042,
        output_tokens: 91,
        estimated_cost_usd: Some(0.0123),
        ..Default::default()
    };
    let issue = json!({"page":7,"field":"body","reason":"Native text exceeds frame","solution":"Increase frame height"});
    fixture.app.ui.ai.snapshot.current_issues = vec![issue.clone()];
    fixture.app.ui.ai.error = "Native text exceeds frame\nPage 7 body needs another line.".into();
    let mut panel = fixture.panel();
    panel.run_steps(3);
    assert!(panel.query_by_label("Live requests: 3 / 4 · Input 1042 · Output 91 · Estimated $0.012300").is_some());
    assert!(panel.query_by_label("Native text exceeds frame").is_some());
    assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_none());
    let diagnostic = serde_json::to_string_pretty(&issue).unwrap();
    assert!(panel.query_by_label(&diagnostic).is_none());
    panel.get_by_label("Details").click();
    panel.run_steps(3);
    assert!(panel.query_by_label(&diagnostic).is_some());
    assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_some());
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    assert!(panel.get_by_label("Approve and generate next review group").rect().max.y < 768.0);
}

#[test]
fn facing_proofs_keep_cover_single_order_pairs_and_review_each_selected_page_separately() {
    let mut fixture = ProofFixture::new();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.total_pages = 6;
    batch.pages_per_batch = 3;
    batch.completed_pages = 3;
    let report = &mut batch.batches[0];
    report.page_count = 3;
    report.proof_paths.push("page-3.png".into());
    report.proof_sha256.push("4".repeat(64));
    fixture.app.ui.ai.accept(fixture.checkpoint.clone());
    fixture.app.ui.ai.settings.facing_pages = true;
    fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
    let mut panel = fixture.panel();
    panel.run_steps(3);
    assert!(panel.query_by_label("Selected page proof").is_some(), "the first cover remains a single page");
    assert!(panel.query_by_label("Page 2 proof").is_none());
    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert!(panel.get_by_label("Page 2 proof").rect().center().x < panel.get_by_label("Page 3 proof").rect().center().x);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "showing a pair reviews neither page");
    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
    panel.get_by_label("Page 3").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-3.png"));
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(), "the cover still requires its own review");
    panel.state_mut().ui.ai.settings.right_to_left_binding = Some(true);
    panel.run_steps(3);
    assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2, "changing physical reading order cannot add a review");
    panel.get_by_label("Show facing pair").click();
    panel.run_steps(3);
    assert!(panel.query_by_label("Selected page proof").is_some());
    assert!(panel.state().ui.ai.companion_proof.is_none());
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    panel.get_by_label("Show facing pair").click();
    panel.run_steps(3);
    assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
    panel.get_by_label("Previous").click();
    panel.run_steps(3);
    assert!(panel.get_by_label("Reviewed").accesskit_node().is_disabled());
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
}

#[test]
fn facing_partner_outside_review_group_is_a_placeholder_without_implied_review() {
    let mut fixture = ProofFixture::new();
    fixture.app.ui.ai.settings.facing_pages = true;
    fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
    let mut panel = fixture.panel();
    panel.run_steps(3);
    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert!(panel.query_by_label("Page 2 proof").is_some());
    assert!(panel.query_by_label("Page 3 proof").is_none());
    assert!(panel.query_by_label("Outside this review group").is_some());
    assert!(panel.state().ui.ai.companion_proof.is_none());
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
    panel.get_by_label("Previous").click();
    panel.run_steps(3);
    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    assert!(
        !panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(),
        "only actual pages in the current group are required"
    );
}

#[test]
fn facing_proof_prefers_matching_document_binding_and_drops_it_after_revision_change() {
    let mut fixture = ProofFixture::new();
    fixture.app.run("layout.documentSetup", json!({"facingPages":true,"binding":"rightToLeft"})).unwrap();
    let document = fixture.app.session.active().unwrap();
    let batch = fixture.checkpoint.batch.as_mut().unwrap();
    batch.document_uid = Some(document.uid);
    batch.document_revision = Some(document.revision);
    batch.total_pages = 6;
    batch.pages_per_batch = 3;
    batch.completed_pages = 3;
    let report = &mut batch.batches[0];
    report.page_count = 3;
    report.proof_paths.push("page-3.png".into());
    report.proof_sha256.push("4".repeat(64));
    fixture.app.ui.ai.accept(fixture.checkpoint.clone());
    fixture.app.ui.ai.settings.facing_pages = true;
    fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
    let mut panel = fixture.panel();
    panel.run_steps(3);
    panel.get_by_label("Next").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_binding, Some(true));
    assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
    panel.state_mut().run("layout.documentSetup", json!({"binding":"leftToRight"})).unwrap();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.proof_binding, None);
    assert!(!panel.state().ui.ai.can_review_selected_proof());
    assert!(!panel.state().ui.ai.can_approve_batch());
}

#[test]
fn proof_navigation_rejects_foreign_pages_and_discards_a_press_on_the_previous_page() {
    let mut panel = ProofFixture::new().panel();
    panel.run_steps(3);
    assert!(!panel.state_mut().ui.ai.select_proof("foreign.png"));
    assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-1.png"));
    let pos = panel.get_by_label("I reviewed the displayed page").rect().center();
    panel.event(egui::Event::PointerMoved(pos));
    panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    panel.run_steps(1);
    assert!(panel.state().ui.ai.review_pointer_press.is_some());
    assert!(panel.state_mut().ui.ai.select_proof("page-2.png"));
    assert!(panel.state().ui.ai.proof_texture.is_none());
    assert!(panel.state().ui.ai.review_pointer_press.is_none());
    panel.run_steps(2);
    panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    panel.run_steps(3);
    assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "releasing a press from another page cannot confirm the new preview");
    panel.get_by_label("I reviewed the displayed page").click();
    panel.run_steps(3);
    assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
}

#[test]
fn batch_approval_requires_explicit_review_of_every_page_not_first_autoload() {
    let mut fixture = ProofFixture::new();
    load_proof_preview(&mut fixture.app, &fixture.ctx);
    assert!(fixture.app.ui.ai.proof_texture.is_some());
    assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "autoload is not human review");
    assert!(!fixture.app.ui.ai.can_approve_batch());
    fixture.app.ui.ai.review_selected_proof();
    assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
    assert!(!fixture.app.ui.ai.can_approve_batch(), "first page alone must not approve a two-page batch");
    assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
    assert_eq!(fixture.calls.get(), 0);
    fixture.app.ui.ai.proof_path = Some("page-2.png".into());
    fixture.app.ui.ai.review_selected_proof();
    assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "the previous texture cannot confirm the newly selected page");
    fixture.view("page-2.png");
    assert!(!fixture.app.ui.ai.can_approve_batch(), "loading the second page is not confirmation");
    fixture.app.ui.ai.review_selected_proof();
    assert!(fixture.app.ui.ai.can_approve_batch());
}

#[test]
fn batch_review_resets_for_checkpoint_proof_fingerprints_and_document_revision() {
    for change in ["checkpoint", "png hash", "pdf hash", "path", "page range", "task shape", "document uid", "document revision"] {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        assert!(fixture.app.ui.ai.can_approve_batch());
        let mut changed = fixture.checkpoint.clone();
        let batch = changed.batch.as_mut().unwrap();
        match change {
            "checkpoint" => batch.checkpoint += 1,
            "png hash" => batch.batches[0].proof_sha256[2] = "a".repeat(64),
            "pdf hash" => batch.batches[0].proof_sha256[1] = "a".repeat(64),
            "path" => batch.batches[0].proof_paths[2] = "replacement.png".into(),
            "page range" => batch.batches[0].page_start += 1,
            "task shape" => batch.total_pages += 1,
            "document uid" => batch.document_uid = batch.document_uid.map(|uid| uid + 1),
            "document revision" => batch.document_revision = batch.document_revision.map(|revision| revision + 1),
            _ => panic!("Unknown proof regression case: {change}"),
        }
        let document = Some((batch.document_uid.unwrap(), batch.document_revision.unwrap()));
        fixture.app.ui.ai.accept(changed);
        fixture.app.ui.ai.sync_proof_document(document);
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "{change}");
        assert!(fixture.app.ui.ai.proof_texture.is_none(), "{change}");
        assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
    }
    let mut fixture = ProofFixture::new();
    fixture.confirm_all();
    let document = fixture.app.ui.ai.proof_key.as_ref().unwrap().document;
    fixture.app.ui.ai.sync_proof_document(Some((document.0, document.1 + 1)));
    assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 2, "review evidence stays disabled until the host validates this revision");
    assert!(!fixture.app.ui.ai.can_review_selected_proof());
    assert!(!fixture.app.ui.ai.can_approve_batch());
}

#[test]
fn empty_missing_duplicate_or_unfingerprinted_batch_pages_cannot_approve() {
    for change in ["empty", "missing png", "missing hash", "duplicate png", "duplicate pdf", "case alias png", "invalid hash"] {
        let mut fixture = ProofFixture::new();
        let mut changed = fixture.checkpoint.clone();
        let report = &mut changed.batch.as_mut().unwrap().batches[0];
        match change {
            "empty" => {
                report.page_count = 0;
                report.proof_paths.clear();
                report.proof_sha256.clear();
            }
            "missing png" => {
                report.proof_paths.pop();
                report.proof_sha256.pop();
            }
            "missing hash" => {
                report.proof_sha256.pop();
            }
            "duplicate png" => report.proof_paths[3] = report.proof_paths[2].clone(),
            "case alias png" => report.proof_paths[3] = "PAGE-1.PNG".into(),
            "duplicate pdf" => {
                report.proof_paths.push("proof.pdf".into());
                report.proof_sha256.push("1".repeat(64));
            }
            "invalid hash" => report.proof_sha256[2] = "missing".into(),
            _ => panic!("Unknown proof regression case: {change}"),
        }
        fixture.app.ui.ai.accept(changed);
        assert!(fixture.app.ui.ai.proof_key.is_none(), "{change}");
        assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
    }
}

#[test]
fn failed_page_reload_revokes_its_previous_human_confirmation() {
    let mut fixture = ProofFixture::new();
    fixture.confirm_all();
    assert!(fixture.app.ui.ai.can_approve_batch());
    fixture.app.services.read = Some(Box::new(|_| Err("synthetic proof read failure".into())));
    fixture.view("page-1.png");
    assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
    assert!(!fixture.app.ui.ai.can_approve_batch());
    fixture.app.ui.ai.review_selected_proof();
    assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "a failed preview cannot be confirmed");
}

#[test]
fn control_cannot_approve_after_all_pages_reviewed_but_typed_human_action_can() {
    let mut fixture = ProofFixture::new();
    fixture.confirm_all();
    assert!(fixture.app.ui.ai.can_approve_batch());
    assert!(fixture.app.run("ai.request", json!({"method":"approve_batch", "job_id":"two-page-proof", "checkpoint":1})).is_err());
    assert_eq!(fixture.calls.get(), 0);
    assert!(
        dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err(),
        "absence of control input is not a human gesture"
    );
    let ctx = fixture.ctx.clone();
    let mut focused = false;
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        let response = ui.button("Approve proof");
        response.request_focus();
        focused = response.has_focus();
    });
    output.textures_delta.clear();
    assert!(focused);
    let input = egui::RawInput {
        events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }],
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| {
        let response = ui.button("Approve proof");
        assert!(AiUiState::fresh_review_gesture(true, fixture.app.ui.ai.proof_key.as_ref(), &mut fixture.app.ui.ai.review_pointer_press, &response));
        fixture.app.ui.ai.batch_approval_gesture =
            fixture.app.ui.ai.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
        dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).unwrap();
    });
    output.textures_delta.clear();
    assert_eq!(fixture.calls.get(), 1);
    assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "the human approval is consumed once");
    assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
    assert_eq!(fixture.calls.get(), 1);
}

#[test]
fn idle_batch_review_frames_never_confirm_or_approve_and_expired_gesture_is_rejected() {
    let mut fixture = ProofFixture::new();
    fixture.view("page-1.png");
    let ctx = fixture.ctx.clone();
    for reviewed in [false, true] {
        if reviewed {
            fixture.confirm_all();
        }
        let before = fixture.app.ui.ai.reviewed_proofs.clone();
        for _ in 0..4 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut requests = Vec::new();
                batch_review(ui, &mut fixture.app.ui.ai, false, &mut requests);
                assert!(requests.is_empty());
                assert!(fixture.app.ui.ai.batch_approval_gesture.is_none());
            });
            output.textures_delta.clear();
            assert_eq!(fixture.app.ui.ai.reviewed_proofs, before);
        }
    }
    fixture.app.ui.ai.batch_approval_gesture = fixture.app.ui.ai.proof_key.clone().map(|key| (key, ctx.clone(), ctx.cumulative_frame_nr()));
    let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
    output.textures_delta.clear();
    assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
    assert_eq!(fixture.calls.get(), 0);
}

#[test]
fn review_gesture_rejects_repeated_keys_and_pointer_press_from_previous_proof() {
    fn frame(fixture: &mut ProofFixture, events: Vec<egui::Event>) -> (egui::Rect, bool) {
        let ctx = fixture.ctx.clone();
        let mut result = (egui::Rect::NOTHING, false);
        let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            let response = ui.button("Confirm proof");
            response.request_focus();
            result = (
                response.rect,
                AiUiState::fresh_review_gesture(true, fixture.app.ui.ai.proof_key.as_ref(), &mut fixture.app.ui.ai.review_pointer_press, &response),
            );
        });
        output.textures_delta.clear();
        result
    }
    let mut fixture = ProofFixture::new();
    let (rect, activated) = frame(&mut fixture, vec![]);
    assert!(!activated);
    let key = |repeat| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat, modifiers: Default::default() };
    assert!(frame(&mut fixture, vec![key(false)]).1);
    assert!(!frame(&mut fixture, vec![key(true)]).1);
    assert!(
        !frame(
            &mut fixture,
            vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() }]
        )
        .1
    );
    assert!(frame(&mut fixture, vec![key(false)]).1);
    let pointer =
        |pressed| egui::Event::PointerButton { pos: rect.center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
    assert!(!frame(&mut fixture, vec![egui::Event::PointerMoved(rect.center()), pointer(true)]).1);
    assert!(fixture.app.ui.ai.review_pointer_press.is_some());
    let mut changed = fixture.checkpoint.clone();
    changed.batch.as_mut().unwrap().checkpoint += 1;
    fixture.app.ui.ai.accept(changed);
    assert!(!frame(&mut fixture, vec![pointer(false)]).1, "release cannot approve a replacement proof");
    assert!(!frame(&mut fixture, vec![pointer(true)]).1);
    assert!(frame(&mut fixture, vec![pointer(false)]).1, "a fresh press and release on the current proof remains usable");
    assert!(frame(&mut fixture, vec![pointer(true), pointer(false)]).1, "coalesced fresh pointer input remains usable");
    let outside = egui::Event::PointerButton {
        pos: rect.right_bottom() + egui::vec2(50.0, 50.0),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    };
    assert!(!frame(&mut fixture, vec![outside, pointer(false)]).1, "a press originating outside the proof widget is not approval");
}

#[test]
fn synthetic_control_frames_cannot_confirm_pages_or_submit_batch_approval() {
    let mut fixture = ProofFixture::new();
    fixture.view("page-1.png");
    fixture.app.control_input_frame.store(true, std::sync::atomic::Ordering::Relaxed);
    fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
    fixture.app.ui.ai.review_selected_proof();
    assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
    fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
    fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
    fixture.confirm_all();
    for control_request in [true, false] {
        fixture.app.in_control = control_request;
        fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 0);
    }
}

#[test]
fn human_only_typed_dispatch_rejects_control_requests_and_synthetic_frames() {
    let mut fixture = ProofFixture::new();
    for control_request in [true, false] {
        fixture.app.in_control = control_request;
        fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
        for request in [
            AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() },
            AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() },
            AiRequest::SaveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone(), key: "fixture".into() },
            AiRequest::RemoveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone() },
            AiRequest::RefreshCredentials,
            AiRequest::OpenPrintProof { token: "fixture".into() },
            AiRequest::StartLocalOllama { executable: "C:/fixture/ollama.exe".into() },
            AiRequest::StopLocalOllama,
        ] {
            assert!(dispatch(&mut fixture.app, request).is_err());
            assert_eq!(fixture.calls.get(), 0);
        }
    }
    fixture.app.in_control = false;
    fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
    dispatch(&mut fixture.app, AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() }).unwrap();
    dispatch(&mut fixture.app, AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() }).unwrap();
    assert_eq!(fixture.calls.get(), 2, "ordinary human typed actions still reach the mock host");
    assert!(is_non_text_model("gemini-3.1-flash-lite-image"));
    assert!(is_non_text_model("models/imagen-3.0-generate-002"));
    assert!(is_non_text_model("gemini-3.8-flash-tts"));
    assert!(is_non_text_model("dall-e-3"));
    assert!(is_non_text_model("gpt-image-1"));
    assert!(is_non_text_model("gemini-embedding-001"));
    assert!(!is_non_text_model("gemini-3.8-flash"));
    assert!(!is_non_text_model("gemini-flash-latest"));
    assert!(!is_non_text_model("gemini-pro-latest"));
    assert!(!is_non_text_model("silver-text"));
    assert!(!is_non_text_model("olive-chat:latest"));
    assert!(!is_non_text_model("imaginative-writer-7b"));
    let mut model_state = AiUiState { prompt: "Page 1 brief".into(), new_document: true, ..Default::default() };

    model_state.settings.output_mode = OutputMode::NativeOnly;
    model_state.settings.text_model.model = "gemini-3.1-flash-lite-image".into();
    assert!(model_state.production_issue(true, true, false).is_some_and(|msg| msg.contains("image/media model")));
    assert!(model_state.production_issue(true, true, true).is_some_and(|msg| msg.contains("للصور أو الوسائط")));
    model_state.settings.text_model.model = "gemini-3.8-flash".into();
    assert_eq!(model_state.production_issue(true, true, false), None);
}

#[test]
fn live_status_page_map_and_script_conflict_actions_work_without_starting_production() {
    let mut live = AiUiState {
        snapshot: AiSnapshot {
            busy: true,
            stage: "planning".into(),
            message: "Generating editable native layout for page 23".into(),
            batch: Some(BatchSnapshot {
                status: "running".into(),
                current_batch: 1,
                total_batches: 13,
                completed_pages: 22,
                total_pages: 48,
                pages_per_batch: 2,
                elapsed_ms: 5_000,
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(live.live_status_line(false), "Batch 1 / 13 · Page 23 / 48 · Phase: Planning · 00:00:05");
    assert_eq!(live.live_status_line(true), "الدفعة 1 / 13 · الصفحة 23 / 48 · المرحلة: التخطيط · 00:00:05");
    live.snapshot.stage = "rendering_proof".into();
    live.snapshot.message = "Preparing complete review proofs".into();
    live.snapshot.batch.as_mut().unwrap().elapsed_ms = 18_000;
    assert_eq!(live.live_status_line(false), "Batch 1 / 13 · Pages 23–24 / 48 · Phase: Proof preparation · 00:00:18");

    let mut state = AiUiState {
        new_document: false,
        continue_book: true,
        prompt: (1..=48).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect(),
        ..Default::default()
    };
    state.settings.text_model.model = "fixture-model".into();
    let documents = vec![
        OpenDocument {
            index: 0,
            uid: 10,
            path: None,
            title: "Untitled-11".into(),
            facing_pages: true,
            right_to_left_binding: false,
            pages: 49,
            remaining_pages: 49,
            continuable: false,
            filled_ranges: vec![[2, 15]],
            blank_ranges: vec![[1, 1], [16, 49]],
        },
        OpenDocument {
            index: 1,
            uid: 11,
            path: None,
            title: "book-48".into(),
            facing_pages: true,
            right_to_left_binding: false,
            pages: 48,
            remaining_pages: 26,
            continuable: true,
            filled_ranges: vec![[1, 22]],
            blank_ranges: vec![[23, 48]],
        },
    ];
    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
        (state, Vec::new(), Some(0)),
    );
    panel.run_steps(3);
    assert!(panel.query_by_label("Page map — Completed: 2–15 · Blank: 1, 16–49").is_some());
    assert!(
        panel
            .query_by_label(
                "Leading page 1 is blank while later pages contain items (2–15); prefix continuation requires completed pages starting at page 1."
            )
            .is_some()
    );
    assert!(panel.query_by_label("Script defines 48 explicit PAGE blocks, but selected document 'Untitled-11' has 49 pages.").is_some());
    assert!(panel.get_by_label("Produce").accesskit_node().is_disabled());

    panel.get_by_label("Switch to matching book (book-48)").click();
    panel.run_steps(3);
    assert_eq!(panel.state().2, Some(1));
    assert!(panel.state().1.is_empty(), "switching to the matching book never starts production automatically");
    assert!(panel.query_by_label("Page map — Completed: 1–22 · Blank: 23–48").is_some());
    assert!(!panel.get_by_label("Produce").accesskit_node().is_disabled());

    panel.state_mut().0.new_document = true;
    panel.state_mut().0.continue_book = false;
    panel.state_mut().0.pages = 8;
    panel.run_steps(3);
    assert!(panel.query_by_label("Script defines 48 explicit PAGE blocks, but New document is set to 8 pages.").is_some());
    assert!(panel.get_by_label("Produce").accesskit_node().is_disabled());
    panel.get_by_label("Match document page count to script (48 pages)").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.pages, 48);
    assert!(!panel.get_by_label("Produce").accesskit_node().is_disabled());
    assert!(panel.state().1.is_empty());
}

#[test]
fn diagnostic_alert_banner_displays_cause_solution_and_executes_recovery_actions() {
    use egui_kittest::kittest::Queryable;

    let mut state = AiUiState {
        automatic_skills: false,
        selected_skills: std::collections::BTreeSet::from([
            "skill-1".to_string(),
            "skill-2".to_string(),
            "skill-3".to_string(),
            "skill-4".to_string(),
            "skill-5".to_string(),
        ]),
        ..Default::default()
    };
    state.snapshot.stage = "failed".into();
    state.snapshot.message = "AI context exceeds its byte limit".into();

    let alert = state.active_diagnostic_alert().expect("context limit alert should be active");
    assert_eq!(alert.kind, DiagnosticAlertKind::ContextLimit);

    let documents = vec![OpenDocument {
        index: 0,
        uid: 1,
        path: None,
        title: "KG2-Islamic".into(),
        facing_pages: true,
        right_to_left_binding: true,
        pages: 8,
        remaining_pages: 8,
        continuable: false,
        filled_ranges: Vec::new(),
        blank_ranges: vec![[1, 8]],
    }];

    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
        (state, Vec::new(), Some(0)),
    );
    panel.run_steps(3);
    assert!(panel.query_by_label("⚠ AI Context Capacity Alert — Skill or Reference Context Limit Exceeded").is_some());
    panel.get_by_label("Enable Smart Skill Compaction").click();
    panel.run_steps(3);
    assert!(panel.state().0.automatic_skills);
    assert!(panel.state().0.selected_skills.len() <= 2);
    assert!(panel.state().0.active_diagnostic_alert().is_none());

    panel.state_mut().0.snapshot.stage = "failed".into();
    panel.state_mut().0.snapshot.message = "Provider request failed: 401 Unauthorized API key".into();
    panel.run_steps(3);
    assert!(panel.query_by_label("⚠ Authentication Alert — Provider API Key Missing or Unauthorized").is_some());
    panel.get_by_label("Open Keys & Providers").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.tab, 3);
}

#[test]
fn production_panel_open_file_to_edit_switches_target_to_current_document() {
    use egui_kittest::kittest::Queryable;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("existing-poster.designcraft");
    let txt_path = dir.path().join("imported-notes.txt");
    std::fs::write(&txt_path, "Imported heading line\nSecond body paragraph for editing.").unwrap();
    let mut author = DesignApp::new(
        Session::new(),
        Services { write: Some(Box::new(|path, bytes| std::fs::write(path, bytes).map_err(|e| e.to_string()))), ..Default::default() },
    );
    author.run("file.new", serde_json::json!({"preset":"A4","pages":2})).unwrap();
    author
        .run("frame.create", serde_json::json!({"spread":0,"rect":[72.0,72.0,320.0,160.0],"content":"text","text":"Existing Poster Heading"}))
        .unwrap();
    author.run("file.save", serde_json::json!({"path": file_path.to_string_lossy()})).unwrap();

    let observed_filter = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen_filter = observed_filter.clone();
    let mut app = DesignApp::new(
        Session::new(),
        Services {
            read: Some(Box::new(|path| std::fs::read(path).map_err(|e| e.to_string()))),
            ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })),
            ..Default::default()
        },
    );
    #[cfg(not(target_arch = "wasm32"))]
    app.answer_dialogs(move |request| {
        if let craft_dialogs::Request::File(dialog) = request
            && let Some(first) = dialog.filters.first()
        {
            *seen_filter.lock().unwrap() = first.name.clone();
        }
        craft_dialogs::Answer::Cancelled
    });
    app.ui.ai.open = true;
    app.ui.ai.new_document = true;
    app.ui.ai.continue_book = true;
    app.ui.ai.open_document_path = file_path.to_string_lossy().into_owned();
    app.ui.ai.open_target_document_requested = true;

    let mut harness = harness_builder().with_size(egui::vec2(1300.0, 900.0)).build_ui_state(|ui, app: &mut DesignApp| show(app, ui.ctx()), app);
    harness.run_steps(3);

    assert!(!harness.state().ui.ai.new_document, "opening a file to edit switches target mode to CurrentDocument");
    assert!(!harness.state().ui.ai.continue_book, "opening a file to edit clears continue_book");
    assert!(harness.state().ui.ai.error.is_empty(), "error={}", harness.state().ui.ai.error);
    assert_eq!(harness.state().session.active().map(|d| d.doc.page_count()), Some(2));
    assert!(harness.query_by_label("Open path").is_some());
    assert!(harness.query_by_label("Browse…").is_some());

    // Cancelling the file picker must not flip target mode when a document is already open,
    // and must request the multi-format ai_target_document filter rather than generic open.
    harness.state_mut().ui.ai.new_document = true;
    harness.state_mut().ui.ai.pick_target_document = true;
    harness.run_steps(3);
    #[cfg(not(target_arch = "wasm32"))]
    harness.state_mut().settle_dialogs();
    assert!(harness.state().ui.ai.new_document, "cancelled picker must preserve new_document mode");
    #[cfg(not(target_arch = "wasm32"))]
    assert_eq!(observed_filter.lock().unwrap().as_str(), "Editable & Importable Documents");

    assert_eq!(sanitize_pdf_page_size(Some((420.0, 595.0))), (420.0, 595.0));
    assert_eq!(sanitize_pdf_page_size(Some((f64::NAN, 595.0))), (595.27559, 841.88976));
    assert_eq!(sanitize_pdf_page_size(Some((-10.0, 595.0))), (595.27559, 841.88976));
    assert_eq!(sanitize_pdf_page_size(Some((10.0, 595.0))), (595.27559, 841.88976));
    assert_eq!(sanitize_pdf_page_size(Some((595.0, 99_999.0))), (595.27559, 841.88976));

    // Opening a text file imports and autoflows it into a new editable document and switches to CurrentDocument.
    harness.state_mut().ui.ai.continue_book = true;
    harness.state_mut().ui.ai.open_document_path = txt_path.to_string_lossy().into_owned();
    harness.state_mut().ui.ai.open_target_document_requested = true;
    harness.run_steps(3);
    assert!(!harness.state().ui.ai.new_document);
    assert!(!harness.state().ui.ai.continue_book);
    assert!(harness.state().ui.ai.error.is_empty(), "error={}", harness.state().ui.ai.error);
    assert_eq!(harness.state().session.active().map(|d| d.doc.title.as_str()), Some("imported-notes"));
}

#[test]
fn selectable_skills_and_new_diagnostic_quick_fixes_operate_cleanly() {
    use egui_kittest::kittest::Queryable;

    let cards = vec![
        SkillCard {
            design_profile: None,
            id: "builtin-draft".into(),
            name: "Builtin Draft".into(),
            description: "Fresh unapproved builtin skill".into(),
            revision: "rev-1".into(),
            approved: false,
            automatically_validated: false,
            trusted_source: true,
            active: false,
            enabled: true,
            compatible: true,
        },
        SkillCard {
            design_profile: None,
            id: "untrusted-user-draft".into(),
            name: "Untrusted User Draft".into(),
            description: "Unapproved imported draft".into(),
            revision: "rev-ext".into(),
            approved: false,
            automatically_validated: false,
            trusted_source: false,
            active: false,
            enabled: true,
            compatible: true,
        },
        SkillCard {
            design_profile: None,
            id: "dual-rev".into(),
            name: "Dual Rev Active".into(),
            description: "Approved active revision".into(),
            revision: "rev-active".into(),
            approved: true,
            automatically_validated: true,
            trusted_source: false,
            active: true,
            enabled: true,
            compatible: true,
        },
        SkillCard {
            design_profile: None,
            id: "dual-rev".into(),
            name: "Dual Rev Newer Draft".into(),
            description: "Unapproved newer draft".into(),
            revision: "rev-draft".into(),
            approved: false,
            automatically_validated: false,
            trusted_source: false,
            active: false,
            enabled: true,
            compatible: true,
        },
        SkillCard {
            design_profile: None,
            id: "disabled-skill".into(),
            name: "Disabled".into(),
            description: "Disabled".into(),
            revision: "rev-dis".into(),
            approved: true,
            automatically_validated: false,
            trusted_source: true,
            active: true,
            enabled: false,
            compatible: true,
        },
    ];
    let manual_visible = selectable_skill_cards(&cards, false, true);
    assert_eq!(manual_visible.len(), 2, "manual mode must hide unapproved untrusted drafts");
    assert_eq!(manual_visible[0].id, "builtin-draft");
    assert_eq!(manual_visible[1].id, "dual-rev");
    assert_eq!(manual_visible[1].revision, "rev-active", "active revision must win over newer draft without duplicating IDs");

    let auto_pin_visible = selectable_skill_cards(&cards, false, false);
    assert_eq!(auto_pin_visible.len(), 3, "automatic mode pin list allows automatic validation candidates");
    assert_eq!(auto_pin_visible[1].id, "untrusted-user-draft");

    let mut state = AiUiState {
        prompt: "# PAGE 1 — Cover\nTITLE_AR: أ\n# PAGE 2 — Intro\nTITLE_AR: ب\n# PAGE 3 — Lesson\nTITLE_AR: ج\n# PAGE 4 — Review\nTITLE_AR: د\n"
            .into(),
        new_document: true,
        pages: 12,
        ..Default::default()
    };
    state.settings.allow_paid_repairs = false;
    state.settings.budget.repair_attempts = 0;
    state.snapshot.stage = "failed".into();
    state.snapshot.message = "Script defines 4 explicit PAGE blocks, but target page count differs".into();

    let documents = Vec::new();
    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
        (state, Vec::new(), None),
    );
    panel.run_steps(3);
    panel.get_by_label("Match Page Count to Script").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.pages, 4);
    assert!(panel.state().0.active_diagnostic_alert().is_none());

    panel.state_mut().0.snapshot.stage = "failed".into();
    panel.state_mut().0.snapshot.message = "Print-preflight errors: text frame overflow".into();
    panel.run_steps(3);
    panel.get_by_label("Enable Automatic Layout Repair").click();
    panel.run_steps(3);
    assert!(panel.state().0.settings.allow_paid_repairs);
    assert!(panel.state().0.settings.budget.repair_attempts >= 2);
    assert!(panel.state().0.active_diagnostic_alert().is_none());
}

#[test]
fn model_validation_retired_404_quick_fixes_and_per_provider_discovery_operate_cleanly() {
    use egui_kittest::kittest::Queryable;

    // 1. Pre-flight production_issue checks for image_model and verify_generated_images (English + Arabic).
    let mut state = AiUiState { prompt: "Illustrated worksheet".into(), new_document: true, pages: 4, ..Default::default() };
    state.settings.output_mode = OutputMode::NativeWithImages;
    state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
    state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
    assert!(state.production_issue(true, true, false).unwrap().contains("text/non-image"));
    assert!(state.production_issue(true, true, true).unwrap().contains("مخصص للنصوص"));

    state.settings.image_model = ModelSelection { provider: ProviderKind::Ollama, model: "llava:latest".into(), endpoint: None };
    assert!(state.production_issue(true, true, false).unwrap().contains("Ollama"));

    state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image-preview".into(), endpoint: None };
    state.settings.verify_generated_images = true;
    state.settings.text_model = ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None };
    assert!(state.production_issue(true, true, false).unwrap().contains("Gemini"));

    // 2. HTTP 404 / retired gemini-2.5-flash diagnostic alert banner & 1-click quick fix persisting UpdateSettings.
    state.settings.verify_generated_images = false;
    state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
    state.snapshot.stage = "failed".into();
    state.snapshot.message = "AI provider HTTP 404: request, model, schema, or endpoint not supported".into();
    assert_eq!(state.active_diagnostic_alert().map(|alert| alert.kind), Some(DiagnosticAlertKind::InvalidModel));

    let documents = Vec::new();
    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
        (state, Vec::new(), None),
    );
    panel.run_steps(3);
    panel.get_by_label("Use gemini-3.8-flash").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.text_model.model, "gemini-3.8-flash");
    assert!(panel.state().1.iter().any(|req| matches!(req, AiRequest::UpdateSettings(s) if s.text_model.model == "gemini-3.8-flash")));
    assert!(panel.state().0.active_diagnostic_alert().is_none());

    // 3. Per-provider Discover populates discovered_models without clobbering across Gemini and OpenAI.
    let mut discover_state = AiUiState {
        settings: AiSettings {
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None },
            ..Default::default()
        },
        pending_models: Some(ModelSelection { provider: ProviderKind::Gemini, model: String::new(), endpoint: None }),
        ..Default::default()
    };
    let mut gemini_snap = discover_state.snapshot.clone();
    gemini_snap.busy = false;
    gemini_snap.stage = "completed".into();
    gemini_snap.models = vec!["gemini-3.8-flash".into(), "gemini-3.1-flash-image-preview".into(), "text-embedding-004".into()];
    discover_state.accept(gemini_snap);

    discover_state.pending_models = Some(ModelSelection { provider: ProviderKind::OpenAi, model: String::new(), endpoint: None });
    let mut openai_snap = discover_state.snapshot.clone();
    openai_snap.busy = false;
    openai_snap.stage = "completed".into();
    openai_snap.models = vec!["gpt-5.4".into(), "gpt-image-1".into(), "dall-e-3".into()];
    discover_state.accept(openai_snap);

    let gemini_models = discover_state.discovered_models.get(&(ProviderKind::Gemini, None)).unwrap();
    let openai_models = discover_state.discovered_models.get(&(ProviderKind::OpenAi, None)).unwrap();
    assert_eq!(gemini_models.len(), 3);
    assert_eq!(openai_models.len(), 3);
    assert_eq!(gemini_models.iter().filter(|m| !is_non_text_model(m)).cloned().collect::<Vec<_>>(), vec!["gemini-3.8-flash".to_owned()]);
    assert_eq!(
        openai_models.iter().filter(|m| !designcraft_ai_types::is_non_image_model(ProviderKind::OpenAi, m)).cloned().collect::<Vec<_>>(),
        vec!["gpt-image-1".to_owned(), "dall-e-3".to_owned()]
    );

    // 4. Gemini imagen-* (:predict-only) is rejected on :generateContent with gemini-3.1-flash-image guidance,
    // and Run [100] (text=gemini-3.8-flash, image=gemini-2.5-flash) renders the 1-click "Use gemini-3.1-flash-image" button.
    let mut img_state = AiUiState { prompt: "Illustrated page".into(), new_document: true, pages: 4, ..Default::default() };
    img_state.settings.output_mode = OutputMode::NativeWithImages;
    img_state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
    img_state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "imagen-3.0-generate-002".into(), endpoint: None };
    let issue_en = img_state.production_issue(true, true, false).unwrap();
    assert!(issue_en.contains("gemini-3.1-flash-image") && !issue_en.contains("imagen-3.0-generate-002"), "{issue_en}");

    img_state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
    img_state.snapshot.stage = "failed".into();
    img_state.snapshot.message = "AI provider HTTP 404: request, model, schema, or endpoint not supported".into();
    let docs = Vec::new();
    let mut img_panel = harness_builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
        move |ui, (state, requests, selected)| production(ui, state, false, true, &docs, selected, requests),
        (img_state, Vec::new(), None),
    );
    img_panel.run_steps(3);
    img_panel.get_by_label("Use gemini-3.1-flash-image").click();
    img_panel.run_steps(3);
    assert_eq!(img_panel.state().0.settings.image_model.model, "gemini-3.1-flash-image");
    assert!(img_panel.state().1.iter().any(|req| matches!(req, AiRequest::UpdateSettings(s) if s.image_model.model == "gemini-3.1-flash-image")));
    assert!(img_panel.state().0.active_diagnostic_alert().is_none());

    // 5. Opened facing-pages RTL book enables facing proof pair view even when AiUiState.settings.facing_pages is false.
    let batch = BatchSnapshot {
        job_id: "opened-rtl-book".into(),
        status: "awaiting_human_proof".into(),
        document_uid: Some(77),
        document_revision: Some(12),
        total_pages: 48,
        completed_pages: 2,
        pages_per_batch: 2,
        current_batch: 1,
        total_batches: 24,
        batches: vec![designcraft_ai_types::protocol::BatchReport {
            batch: 1,
            page_start: 1,
            page_count: 2,
            proof_paths: vec!["proof-p2.png".into(), "proof-p3.png".into()],
            proof_sha256: vec!["1".repeat(64), "2".repeat(64)],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut opened_book_ui = AiUiState::default();
    opened_book_ui.settings.facing_pages = false;
    opened_book_ui.accept(AiSnapshot { available: true, stage: "awaiting_human_proof".into(), batch: Some(batch), ..Default::default() });
    opened_book_ui.sync_proof_document(Some((77, 12)));
    opened_book_ui.proof_facing_pages = Some(true);
    opened_book_ui.proof_binding = Some(true);
    assert!(opened_book_ui.effective_proof_facing_pages());
    assert_eq!(opened_book_ui.companion_proof_path(), Some("proof-p3.png"));

    // 6. Reopened failed batch checkpoint surfaces InvalidModel from reports, and benign settings-saved confirmation never triggers a fake alert.
    let mut reopened_failed = AiUiState::default();
    reopened_failed.snapshot.batch = Some(BatchSnapshot { status: "failed".into(), ..Default::default() });
    reopened_failed.snapshot.reports.push(designcraft_ai_types::protocol::TaskReport {
        design_style: None,
        id: "100".into(),
        status: "failed".into(),
        title: "Batch 1".into(),
        provider: ProviderKind::Gemini,
        model: "gemini-2.5-flash".into(),
        settings: None,
        skills: Vec::new(),
        usage: Default::default(),
        elapsed_ms: 480,
        actions: 0,
        issues: Vec::new(),
        artifacts: Vec::new(),
        message: "AI provider HTTP 404: request, model, schema, or endpoint not supported".into(),
    });
    assert_eq!(reopened_failed.active_diagnostic_alert().unwrap().kind, DiagnosticAlertKind::InvalidModel);
    reopened_failed.snapshot.stage = "idle".into();
    reopened_failed.snapshot.message = "AI settings saved; API keys remain outside settings and reports".into();
    assert!(reopened_failed.active_diagnostic_alert().is_none());
}

#[test]
fn ai27_preflight_natural_arabic_pages_retired_gemini_token_limits_and_persisted_allowances() {
    use egui_kittest::kittest::Queryable;

    // 1. Pure natural_prompt_pages parser handles Arabic-Indic digits, headers, English "-page"/"pages",
    //    and ignores page references ("انظر صفحة 5", "see page 2") and per-batch counts ("4 pages per review").
    assert_eq!(natural_prompt_pages("تصميم كتاب من ٤٨ صفحة رياض أطفال"), Some(48));
    assert_eq!(natural_prompt_pages("عدد الصفحات: ٢٤"), Some(24));
    assert_eq!(natural_prompt_pages("Design a 32-page workbook"), Some(32));
    assert_eq!(natural_prompt_pages("Create an illustrated guide of 16 pages."), Some(16));
    assert_eq!(natural_prompt_pages("انظر صفحة 5 وصفحة 6 لمزيد من التفاصيل"), None);
    assert_eq!(natural_prompt_pages("See page 2 and 4 pages per review"), None);
    assert_eq!(natural_prompt_pages("```\n48 pages\n```\nانظر صفحة 3"), None);

    // 2. Natural Arabic prompt with s.pages = 8 shows mismatch button & updates s.pages = 48,
    //    proactive retired gemini-2.5-flash warning switches to gemini-3.8-flash and emits UpdateSettings,
    //    proactive token limit button updates ONLY token_limit + emits UpdateSettings without mutating max_requests,
    //    and "Set request allowance for this review" persists UpdateSettings without mutating token_limit.
    let mut state =
        AiUiState { prompt: "تصميم كتاب من ٤٨ صفحة رياض أطفال".into(), new_document: true, pages: 8, ..Default::default() };
    state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
    state.settings.pages_per_batch = 6;
    state.settings.budget.token_limit = 32_000;
    assert_eq!(state.settings.budget.max_requests, 4);

    let mut panel = harness_builder()
        .with_size(egui::vec2(1300.0, 1400.0))
        .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
    panel.run_steps(3);

    // Match document pages to prompt (48 pages)
    panel.get_by_label("Match document pages to prompt (48 pages)").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.pages, 48);
    assert!(panel.query_by_label("Match document pages to prompt (48 pages)").is_none());

    // Proactive retired Gemini model switch
    panel.get_by_label("Use gemini-3.8-flash").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.text_model.model, "gemini-3.8-flash");
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.text_model.model == "gemini-3.8-flash"));
    assert!(panel.query_by_label("Use gemini-3.8-flash").is_none());

    // Proactive token limit button updates ONLY token_limit (not max_requests!) and emits UpdateSettings
    panel.state_mut().1.clear();
    panel.get_by_label("Set token limit for this review").click();
    panel.run_steps(3);
    let raised_review_tokens = panel.state().0.settings.budget.token_limit;
    assert!(raised_review_tokens > 32_000);
    assert_eq!(panel.state().0.settings.budget.max_requests, 4, "token limit button must never mutate max_requests");
    assert!(matches!(
        panel.state().1.last(),
        Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == raised_review_tokens && s.budget.max_requests == 4
    ));

    // Request allowance button persists UpdateSettings without mutating token_limit
    panel.state_mut().1.clear();
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 6);
    assert_eq!(panel.state().0.settings.budget.token_limit, raised_review_tokens, "request allowance button must leave token_limit untouched");
    assert!(matches!(
        panel.state().1.last(),
        Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 6 && s.budget.token_limit == raised_review_tokens
    ));

    // Recovery UX when completed_pages == 0 offers accurate guidance + Start fresh with current settings
    panel.state_mut().1.clear();
    panel.state_mut().0.snapshot.production_draft = Some(designcraft_ai_types::protocol::ProductionDraft {
        id: "failed-zero".into(),
        request: panel.state().0.production_task(),
        document_uid: None,
        document_path: None,
        document_title: "Untitled".into(),
        progress: Default::default(),
    });
    panel.state_mut().0.snapshot.batch = Some(BatchSnapshot {
        job_id: "failed-zero".into(),
        status: "failed".into(),
        retryable: true,
        completed_pages: 0,
        total_pages: 8,
        pages_per_batch: 2,
        ..Default::default()
    });
    panel.run_steps(3);
    assert!(panel.query_by_label("Adjust limits or settings below, then Retry this group or start fresh.").is_some());
    assert!(
        panel.query_by_label("Open and select the saved book with remaining blank pages.").is_none(),
        "unpublished NewA4 failure with no document_uid/document_path must not prompt to open a non-existent saved book"
    );
    panel.get_by_label("Start fresh with current settings").click();
    panel.run_steps(3);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::NewA4 { pages: 48 }, .. }))));
}

fn assert_natural_prompt_pages_and_kg_estimates() {
    // 1. Fix 5: natural_prompt_pages edge cases (harakat/tatweel, attached units, brackets, singular "page", and negative rejections).
    assert_eq!(natural_prompt_pages("٤٨صفحة"), Some(48));
    assert_eq!(natural_prompt_pages("48صفحة"), Some(48));
    assert_eq!(natural_prompt_pages("48pages"), Some(48));
    assert_eq!(natural_prompt_pages("عدد الصفحات: (٤٨)"), Some(48));
    assert_eq!(natural_prompt_pages("عَدَدُ الصَّفَحَاتِ: [٤٨]"), Some(48));
    assert_eq!(natural_prompt_pages("كتاب من (48) صَفْحَة"), Some(48));
    assert_eq!(natural_prompt_pages("كتاب من ٤٨ صَفْحَـــةً رياض أطفال"), Some(48));
    assert_eq!(natural_prompt_pages("create a 48 page workbook"), Some(48));

    assert_eq!(natural_prompt_pages("page 48"), None);
    assert_eq!(natural_prompt_pages("انظر صفحة 5"), None);
    assert_eq!(natural_prompt_pages("انظر صَفْحَة (5)"), None);
    assert_eq!(natural_prompt_pages("48 pages per review"), None);
    assert_eq!(natural_prompt_pages("v2.48"), None);
    assert_eq!(natural_prompt_pages("v2.48 pages"), None);

    // 2. Fix 1a: Automatic-style KG illustrated prompt with `kg-oxford-cambridge-style` in `snapshot.skills`
    //    estimates `sources_per_page == 4` (3 relational 1:1 vocab cards + story grid), manual `selected_skills` resolves profile, and 0-slot profile clamps to >= 1.
    let kg_manifest: Value = serde_json::from_str(include_str!("../../../../skills/kg-oxford-cambridge-style/designcraft.json")).unwrap();
    let kg_profile: designcraft_ai_types::DesignProfile = serde_json::from_value(kg_manifest["design_profile"].clone()).unwrap();
    let kg_card = SkillCard {
        design_profile: Some(kg_profile.clone()),
        id: "kg-oxford-cambridge-style".into(),
        name: kg_profile.name.clone(),
        description: "International KG style".into(),
        revision: "rev-1".into(),
        approved: true,
        automatically_validated: true,
        trusted_source: true,
        active: true,
        enabled: true,
        compatible: true,
    };

    let mut kg_state = AiUiState {
        prompt: "تصميم كتاب منهج رياض أطفال دولي مصور من 8 صفحات".into(),
        new_document: true,
        pages: 8,
        automatic_skills: true,
        ..Default::default()
    };
    kg_state.settings.output_mode = OutputMode::NativeWithImages;
    kg_state.settings.pages_per_batch = 2;
    kg_state.settings.design_style = None; // "Automatic from the request"
    kg_state.snapshot.skills = vec![kg_card.clone()];

    let auto_est = kg_state.request_estimate(&[], None).unwrap();
    assert_eq!(auto_est.review_pages, 2);
    assert_eq!(auto_est.image_allowance, 8, "automatic KG Oxford/Cambridge style must estimate 4 image sources per page");

    // Manual skill selection (`automatic_skills == false`) also resolves selected style card
    kg_state.automatic_skills = false;
    kg_state.selected_skills.insert("kg-oxford-cambridge-style".into());
    kg_state.prompt = "Generic brief without style keywords".into();
    let manual_est = kg_state.request_estimate(&[], None).unwrap();
    assert_eq!(manual_est.image_allowance, 8);

    // Pinned active style in `selected_skills` with `automatic_skills == true` and generic prompt resolves profile
    kg_state.automatic_skills = true;
    kg_state.pages = 4;
    kg_state.prompt = "Create a 4-page activity book".into();
    let pinned_auto_est = kg_state.request_estimate(&[], None).unwrap();
    assert_eq!(pinned_auto_est.review_pages, 2);
    assert_eq!(
        pinned_auto_est.image_allowance, 8,
        "automatic_skills == true with active style in selected_skills must resolve that pinned style even on a generic prompt"
    );
    kg_state.pages = 8;
    kg_state.automatic_skills = false;

    // Zero-image-slot profile under NativeWithImages clamps sources_per_page to >= 1 (never 0)
    let mut zero_slot_profile = kg_profile;
    for family in &mut zero_slot_profile.page_families {
        family.image_slots.clear();
    }
    let mut zero_slot_card = kg_card;
    zero_slot_card.design_profile = Some(zero_slot_profile);
    kg_state.snapshot.skills = vec![zero_slot_card];
    let clamped_est = kg_state.request_estimate(&[], None).unwrap();
    assert_eq!(clamped_est.image_allowance, 2, "0-slot profile in NativeWithImages must clamp sources_per_page to 1");

    // 3. Fix 1b: Single-page review with `verify_generated_images` + skill overhead NEVER under-estimates `reserve_with_output`.
    let mut single_generic = AiUiState {
        prompt: "Create an illustrated science worksheet.".into(),
        new_document: true,
        pages: 1,
        automatic_skills: true,
        ..Default::default()
    };
    single_generic.settings.output_mode = OutputMode::NativeWithImages;
    single_generic.settings.verify_generated_images = true;
    single_generic.settings.pages_per_batch = 1;
    single_generic.settings.budget.max_output_tokens = 8192;
    let tok_est = single_generic.token_estimate(&[], None).unwrap();
    let text_reserved = (176 * 1024u64 + single_generic.prompt.len() as u64).div_ceil(3) + 1024 + 8192;
    let image_reserved = 2000u64 + 1024;
    let review_reserved = 2000u64 + 1024 + 8192 + 4096;
    assert!(
        tok_est.review_tokens >= text_reserved + image_reserved + review_reserved,
        "review_tokens ({}) must never be smaller than actual reserve_with_output ({})",
        tok_est.review_tokens,
        text_reserved + image_reserved + review_reserved
    );
}

#[test]
fn ai27_round1_audit_auto_style_estimates_vision_tokens_recovery_copy_and_natural_pages_edges() {
    use egui_kittest::kittest::Queryable;

    assert_natural_prompt_pages_and_kg_estimates();

    // 4. Fix 4: Bound failed batch with `completed_pages > 0` shows saved book instructions + accurate Retry/Continue copy (EN + AR).
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages": 4, "facingPages": false})).unwrap();
    session.execute("frame.create", &json!({"spread": 0, "rect": [20, 20, 100, 80]})).unwrap();
    let doc = session.active().unwrap();
    let req = continuation_request(TaskTarget::ContinueBook);
    let mut snap = saved_production_snapshot(&req, doc);
    snap.batch = Some(BatchSnapshot {
        job_id: "saved-book".into(),
        status: "failed".into(),
        retryable: true,
        completed_pages: 1,
        total_pages: 4,
        pages_per_batch: 1,
        document_uid: Some(doc.uid),
        document_revision: Some(doc.revision),
        ..Default::default()
    });
    let mut rec_state = AiUiState::default();
    rec_state.accept(snap);
    let docs = vec![OpenDocument::from_document(0, doc)];
    let mut en_panel = harness_builder()
        .with_size(egui::vec2(1300.0, 1000.0))
        .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs, &mut Some(0), reqs), (rec_state.clone(), Vec::new()));
    en_panel.run_steps(3);
    assert!(en_panel.query_by_label("Prepare saved book continuation").is_some());
    assert!(
        en_panel
            .query_by_label(
                "Retry keeps cumulative usage, pinned style, and pagination while applying updated budget limits and compatible model selections. To change the brief, style, or batch size, prepare Continue and press Produce."
            )
            .is_some()
    );

    let docs_ar = vec![OpenDocument::from_document(0, doc)];
    let mut ar_fonts_ready = false;
    let mut ar_panel = harness_builder().with_size(egui::vec2(1300.0, 1000.0)).build_ui_state(
        move |ui, (s, reqs)| {
            if !ar_fonts_ready {
                crate::theme::install_fonts(ui.ctx(), "ar");
                ar_fonts_ready = true;
                return;
            }
            production(ui, s, true, true, &docs_ar, &mut Some(0), reqs);
        },
        (rec_state, Vec::new()),
    );
    ar_panel.run_steps(3);
    assert!(ar_panel.query_by_label("تجهيز استكمال الكتاب المحفوظ").is_some());
    assert!(
        ar_panel
            .query_by_label(
                "تحتفظ إعادة المحاولة بالاستخدام التراكمي والنمط المثبّت وتقسيم الدفعات مع تطبيق حدود الميزانية والنماذج المتوافقة المحدّثة. لتغيير التعليمات أو النمط أو حجم الدفعة، جهّز الاستكمال واضغط إنتاج."
            )
            .is_some()
    );

    // 5. Unpublished `NewA4` draft (`document_uid: None, document_path: None`) omits `"Prepare saved book continuation"`,
    //    whereas restored unsaved `ContinueBook` draft (`document_uid: None, document_path: None`, `restored_target_unconfirmed = true`)
    //    renders `"Prepare saved book continuation"` and enables `"Produce"` once clicked on a continuable open document.
    let mut unpub_snap = saved_production_snapshot(&continuation_request(TaskTarget::NewA4 { pages: 8 }), doc);
    if let Some(d) = &mut unpub_snap.production_draft {
        d.document_uid = None;
        d.document_path = None;
    }
    let mut unpub_state = AiUiState::default();
    unpub_state.accept(unpub_snap);
    let docs_restored = vec![OpenDocument::from_document(0, doc)];
    let mut unpub_panel = harness_builder()
        .with_size(egui::vec2(1300.0, 1000.0))
        .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs_restored, &mut Some(0), reqs), (unpub_state, Vec::new()));
    unpub_panel.run_steps(3);
    assert!(
        unpub_panel.query_by_label("Prepare saved book continuation").is_none(),
        "unpublished NewA4 draft with no document_uid/document_path must omit Prepare saved book continuation"
    );

    let mut restored_cont_snap = saved_production_snapshot(&continuation_request(TaskTarget::ContinueBook), doc);
    if let Some(d) = &mut restored_cont_snap.production_draft {
        d.document_uid = None;
        d.document_path = None;
    }
    let mut restored_cont_state = AiUiState::default();
    restored_cont_state.accept(restored_cont_snap);
    assert!(restored_cont_state.restored_target_unconfirmed);
    let docs_cont = vec![OpenDocument::from_document(0, doc)];
    let mut cont_panel = harness_builder()
        .with_size(egui::vec2(1300.0, 1000.0))
        .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs_cont, &mut Some(0), reqs), (restored_cont_state, Vec::new()));
    cont_panel.run_steps(3);
    assert!(cont_panel.query_by_label("Choose the target book or prepare its remaining pages before producing the saved request.").is_some());
    cont_panel.get_by_label("Prepare saved book continuation").click();
    cont_panel.run_steps(3);
    assert!(!cont_panel.state().0.restored_target_unconfirmed);
    cont_panel.get_by_label("Produce").click();
    cont_panel.run_steps(3);
    assert!(matches!(cont_panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::ContinueBook, .. }))));
}

#[test]
fn retryable_failed_batch_allowance_buttons_include_consumed_requests_and_tokens() {
    use designcraft_ai_types::protocol::UsageSummary;
    use egui_kittest::kittest::Queryable;

    let mut session = Session::new();
    session.execute("file.new", &json!({"pages": 20, "facingPages": false})).unwrap();
    for spread in 0..8 {
        session.execute("frame.create", &json!({"spread": spread, "rect": [20, 20, 100, 80]})).unwrap();
    }
    let doc = session.active().unwrap();
    let open_doc = OpenDocument::from_document(0, doc);
    assert_eq!(open_doc.pages, 20);
    assert_eq!(open_doc.remaining_pages, 12);
    assert!(open_doc.continuable);

    let mut req = continuation_request(TaskTarget::ContinueBook);
    req.settings.pages_per_batch = 2;
    req.settings.budget.max_requests = 8;
    req.settings.budget.token_limit = 120_000;

    let mut snap = saved_production_snapshot(&req, doc);
    snap.batch = Some(BatchSnapshot {
        job_id: "saved-book".into(),
        status: "failed".into(),
        retryable: true,
        completed_pages: 8,
        total_pages: 20,
        pages_per_batch: 2,
        document_uid: Some(doc.uid),
        document_revision: Some(doc.revision),
        cumulative_usage: UsageSummary { requests: 8, input_tokens: 100_000, output_tokens: 20_000, ..Default::default() },
        ..Default::default()
    });

    let mut state = AiUiState::default();
    state.accept(snap);
    state.restored_target_unconfirmed = false;
    assert!(!state.new_document);
    assert!(state.continue_book);

    let docs = vec![open_doc];
    let fresh_tokens = state.token_estimate(&docs, Some(0)).unwrap();
    let expected_review_tokens = 120_000u64.saturating_add(fresh_tokens.review_tokens).clamp(256, MAX_TOKEN_LIMIT);
    let expected_book_tokens = 120_000u64.saturating_add(fresh_tokens.book_tokens).clamp(256, MAX_TOKEN_LIMIT);

    let mut panel = harness_builder().with_size(egui::vec2(1300.0, 1400.0)).build_ui_state(
        |ui, (s, reqs, selected, docs)| production(ui, s, false, true, docs, selected, reqs),
        (state, Vec::new(), Some(0usize), docs),
    );
    panel.run_steps(3);

    // 1. Review request allowance includes 8 consumed requests + 2 review requests = 10.
    assert!(
        panel
            .query_by_label("Configured request limit 8 is below the conservative cumulative allowance to retry this review: 10 (8 already used).")
            .is_some()
    );
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 10);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 10));

    // 2. Remaining book request allowance includes 8 consumed requests + 12 remaining requests = 20.
    panel.state_mut().1.clear();
    panel.get_by_label("Set request allowance for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 20);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 20));
    assert!(panel.query_by_label("Set request allowance for remaining book").is_none());

    // 3. Review and remaining-book token limits include the 120,000 consumed tokens on top of remaining estimates.
    panel.state_mut().1.clear();
    panel.get_by_label("Set token limit for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.token_limit, expected_review_tokens);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_review_tokens));

    panel.state_mut().1.clear();
    panel.get_by_label("Set token limit for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.token_limit, expected_book_tokens);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_book_tokens));

    assert_diagnostic_quick_fixes_and_mid_book_newa4(&mut panel, fresh_tokens.book_tokens);
    assert_non_retryable_and_edge_case_allowances(&mut panel, (fresh_tokens.review_tokens, fresh_tokens.book_tokens));
}

fn assert_diagnostic_quick_fixes_and_mid_book_newa4(
    panel: &mut egui_kittest::Harness<'_, (AiUiState, Vec<AiRequest>, Option<usize>, Vec<OpenDocument>)>,
    fresh_book_tokens: u64,
) {
    use egui_kittest::kittest::Queryable;

    // Diagnostic alert quick fixes ("Raise Request Allowance" and "Raise Token Limit") also include consumed + estimated_tokens.
    panel.state_mut().1.clear();
    panel.state_mut().0.settings.budget.max_requests = 8;
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.requests = 12;
    panel.state_mut().0.snapshot.stage = "failed".into();
    panel.state_mut().0.snapshot.message = "AI request limit reached".into();
    panel.run_steps(3);
    panel.get_by_label("Raise Request Allowance").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 24, "12 consumed + 12 remaining = 24 (> 8 * 2)");
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 24));

    panel.state_mut().1.clear();
    panel.state_mut().0.settings.budget.token_limit = 100_000;
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.estimated_tokens = 90_000;
    panel.state_mut().0.snapshot.stage = "failed".into();
    panel.state_mut().0.snapshot.message = "Token budget cannot admit another request".into();
    panel.run_steps(3);
    panel.get_by_label("Raise Token Limit").click();
    panel.run_steps(3);
    let expected_raised_tokens = 210_000u64.saturating_add(fresh_book_tokens).clamp(256_000, MAX_TOKEN_LIMIT);
    assert_eq!(panel.state().0.settings.budget.token_limit, expected_raised_tokens);
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_raised_tokens));

    // Mid-book NewA4 failure with new_document == true does not double-count the 8 already-completed pages.
    panel.state_mut().1.clear();
    panel.state_mut().0.new_document = true;
    panel.state_mut().0.continue_book = false;
    panel.state_mut().0.pages = 20;
    panel.state_mut().0.settings.budget.max_requests = 8;
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.requests = 8;
    panel.run_steps(3);
    panel.get_by_label("Set request allowance for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 20, "8 consumed + 12 remaining = 20 without double-counting pages 1..8");
}

fn assert_non_retryable_and_edge_case_allowances(
    panel: &mut egui_kittest::Harness<'_, (AiUiState, Vec<AiRequest>, Option<usize>, Vec<OpenDocument>)>,
    fresh_tokens: (u64, u64),
) {
    use designcraft_ai_types::protocol::UsageSummary;
    use egui_kittest::kittest::Queryable;

    let (fresh_review_tokens, fresh_book_tokens) = fresh_tokens;
    // 4. Once retryable becomes false (fresh task with new ledger), targets revert to 2 / 12 and fresh token estimates.
    panel.state_mut().1.clear();
    panel.state_mut().0.new_document = false;
    panel.state_mut().0.continue_book = true;
    panel.state_mut().0.snapshot.batch.as_mut().unwrap().retryable = false;
    panel.state_mut().0.settings.budget.max_requests = 1;
    panel.state_mut().0.settings.budget.token_limit = 1_000;
    panel.run_steps(3);
    assert!(panel.query_by_label("Configured request limit 1 is below the conservative planning allowance for this review: 2.").is_some());
    panel.get_by_label("Set request allowance for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 2);
    panel.get_by_label("Set request allowance for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 12);

    panel.get_by_label("Set token limit for this review").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.token_limit, fresh_review_tokens);
    panel.get_by_label("Set token limit for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.token_limit, fresh_book_tokens);

    // 5. Edge case A: When budget_targets is None (e.g. CurrentDocument mode) and consumed_tokens >= token_limit * 2,
    //    "Raise Token Limit" grants 256,000 fresh tokens of headroom above consumed_tokens.
    panel.state_mut().1.clear();
    panel.state_mut().0.new_document = false;
    panel.state_mut().0.continue_book = false;
    panel.state_mut().0.settings.budget.token_limit = 140_000;
    {
        let batch = panel.state_mut().0.snapshot.batch.as_mut().unwrap();
        batch.retryable = true;
        batch.cumulative_usage = UsageSummary { requests: 8, input_tokens: 250_000, output_tokens: 50_000, ..Default::default() };
    }
    panel.state_mut().0.snapshot.stage = "failed".into();
    panel.state_mut().0.snapshot.message = "Token budget cannot admit another request".into();
    panel.run_steps(3);
    assert!(budget_targets(&panel.state().0, &panel.state().3, panel.state().2).is_none());
    panel.get_by_label("Raise Token Limit").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.token_limit, 556_000, "300_000 consumed + 256_000 fallback headroom = 556_000");
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == 556_000));

    // 6. Edge case B: Unpublished NewA4 retryable batch (total_pages = 20, completed_pages = 0, document_uid = None)
    //    while new_document == false on a smaller open document (remaining_pages = 4) still uses batch.total_pages (20), not 4.
    panel.state_mut().1.clear();
    {
        let state = &mut panel.state_mut().0;
        state.new_document = false;
        state.continue_book = true;
        state.settings.budget.max_requests = 4;
        let draft = state.snapshot.production_draft.as_mut().unwrap();
        draft.request.target = TaskTarget::NewA4 { pages: 20 };
        draft.document_uid = None;
        draft.document_path = None;
        let batch = state.snapshot.batch.as_mut().unwrap();
        batch.completed_pages = 0;
        batch.total_pages = 20;
        batch.pages_per_batch = 2;
        batch.document_uid = None;
        batch.retryable = true;
        batch.cumulative_usage.requests = 4;
    }
    panel.state_mut().3[0].pages = 6;
    panel.state_mut().3[0].remaining_pages = 4;
    panel.run_steps(3);
    panel.get_by_label("Set request allowance for remaining book").click();
    panel.run_steps(3);
    assert_eq!(panel.state().0.settings.budget.max_requests, 24, "4 consumed + 20 unpublished NewA4 batch pages = 24 (not 4 + 4 = 8)");
    assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 24));
}

#[test]
fn ai28_ui_refresh_edited_proofs_retired_gemini_and_bound_draft_pages() {
    use egui_kittest::kittest::Queryable;

    // 1. Expanded retired Gemini detection (gemini-3-pro-preview, imagen-3.0, gemini-2.5-flash).
    assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-3-pro-preview"));
    assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "imagen-3.0-generate-002"));
    assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "models/gemini-3.1-flash-lite-preview"));
    assert!(!is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-3.1-pro"));
    assert!(!is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-4-argon"));

    // 2. 404 "model or endpoint not found" maps to InvalidModel.
    assert_eq!(
        classify_diagnostic_alert("Provider HTTP 404: model or endpoint not found (`gemini-2.5-flash`).").kind,
        DiagnosticAlertKind::InvalidModel
    );

    // 3. Bound ContinueBook draft restores saved target_total_pages on accept().
    let mut init_state = AiUiState::default();
    init_state.accept(AiSnapshot {
        available: true,
        production_draft: Some(designcraft_ai_types::protocol::ProductionDraft {
            id: "bound-32".into(),
            request: TaskRequest {
                prompt: "Continue 32-page book".into(),
                settings: AiSettings::default(),
                skill_ids: vec![],
                auto_select_skills: false,
                target: TaskTarget::ContinueBook,
                export_directory: None,
            },
            document_uid: Some(77),
            document_path: None,
            document_title: "Book 32".into(),
            progress: designcraft_ai_types::protocol::SavedProductionProgress { completed_pages: 4, target_total_pages: Some(32) },
        }),
        ..Default::default()
    });
    assert!(init_state.continue_book);
    assert!(!init_state.new_document);
    assert_eq!(init_state.pages, 32);

    // 4. Refresh proofs button appears when the active document has the same UID but a newer edited revision.
    let mut fixture = ProofFixture::new();
    let document = fixture.app.ui.ai.proof_key.as_ref().unwrap().document;
    fixture.app.ui.ai.sync_proof_document(Some((document.0, document.1 + 1)));
    assert!(!fixture.app.ui.ai.proof_document_matches);

    let mut harness = harness_builder().build_ui_state(
        |ui, (s, reqs): &mut (AiUiState, Vec<AiRequest>)| {
            batch_review(ui, s, false, reqs);
        },
        (fixture.app.ui.ai, Vec::<AiRequest>::new()),
    );
    harness.run_steps(2);
    harness.get_by_label("Refresh proofs for edited document").click();
    harness.run_steps(2);
    assert!(matches!(
        harness.state().1.last(),
        Some(AiRequest::RefreshBatchProofs { job_id }) if job_id == "two-page-proof"
    ));
}

#[test]
fn ai28_round1_review_fixes_16m_tokens_spread_aligned_estimates_and_http_alerts() {
    use egui_kittest::kittest::Queryable;

    // 1. HTTP 504 / 429 / 400 / 422 diagnostic alert classifications.
    assert_eq!(classify_diagnostic_alert("AI provider HTTP 504: provider temporarily unavailable").kind, DiagnosticAlertKind::ConnectionOrRateLimit);
    assert_eq!(classify_diagnostic_alert("AI provider HTTP 429: rate or quota limit reached").kind, DiagnosticAlertKind::ConnectionOrRateLimit);
    assert_eq!(classify_diagnostic_alert("AI provider HTTP 400: invalid request or unsupported model parameter").kind, DiagnosticAlertKind::General);
    assert_eq!(classify_diagnostic_alert("AI provider HTTP 422: request schema or parameters rejected").kind, DiagnosticAlertKind::General);

    // 2. 32-page facing_pages=true, pages_per_batch=2: cover Batch 1 is 1 page out of 17 batches.
    let mut s = AiUiState {
        new_document: true,
        continue_book: false,
        pages: 32,
        prompt: "Design a 32-page illustrated science book.".into(),
        ..Default::default()
    };
    s.settings.facing_pages = true;
    s.settings.pages_per_batch = 2;
    s.settings.output_mode = OutputMode::NativeOnly;
    let est = s.request_estimate(&[], None).expect("request estimate");
    assert_eq!(est.review_pages, 1);
    assert_eq!(est.remaining_groups, 17);

    s.snapshot.stage = "planning".into();
    s.snapshot.message = "Preparing native production context".into();
    s.snapshot.batch = Some(BatchSnapshot {
        job_id: "spread-32".into(),
        status: "running".into(),
        total_pages: 32,
        completed_pages: 0,
        pages_per_batch: 2,
        spread_aligned: true,
        current_batch: 1,
        total_batches: 17,
        ..Default::default()
    });
    let en_line = s.live_status_line(false);
    let ar_line = s.live_status_line(true);
    assert!(en_line.contains("Batch 1 / 17 · Page 1 / 32"), "en_line={en_line}");
    assert!(ar_line.contains("الدفعة 1 / 17 · الصفحة 1 / 32"), "ar_line={ar_line}");

    // Failed retryable Batch 1 on spread_aligned book uses 1 page (not 2) in budget_targets_for_estimate.
    s.snapshot.stage = "failed".into();
    s.snapshot.message = "AI request limit reached".into();
    s.snapshot.production_draft = Some(ProductionDraft {
        id: "spread-32".into(),
        request: TaskRequest {
            prompt: s.prompt.clone(),
            settings: s.settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: TaskTarget::NewA4 { pages: 32 },
            export_directory: None,
        },
        document_uid: None,
        document_path: None,
        document_title: "Book 32".into(),
        progress: Default::default(),
    });
    if let Some(batch) = s.snapshot.batch.as_mut() {
        batch.status = "failed".into();
        batch.retryable = true;
        batch.cumulative_usage.requests = 2;
    }
    let targets = budget_targets(&s, &[], None).expect("budget targets");
    assert_eq!(targets.review_requests, 3, "2 consumed + 1 spread-aligned cover page = 3");

    // 3. image_call_tokens matches ledger.rs (3_072 without workflow/verify) & 16M token ceiling scales above 4M.
    s.settings.verify_generated_images = false;
    let native_only_tokens = s.token_estimate(&[], None).unwrap().per_page_tokens;
    s.settings.output_mode = OutputMode::NativeWithImages;
    let with_images_tokens = s.token_estimate(&[], None).unwrap().per_page_tokens;
    assert_eq!(with_images_tokens - native_only_tokens, 3_072);

    s.pages = 120;
    s.settings.budget.max_output_tokens = 32_768;
    s.settings.budget.token_limit = 3_000_000;
    let large_tokens = s.token_estimate(&[], None).unwrap();
    assert!(large_tokens.book_tokens > 4_000_000 && large_tokens.book_tokens <= MAX_TOKEN_LIMIT, "book_tokens={}", large_tokens.book_tokens);
    let large_targets = budget_targets(&s, &[], None).unwrap();
    assert!(large_targets.book_tokens.unwrap() > 4_000_000);

    s.snapshot.stage = "failed".into();
    s.snapshot.message = "Token budget cannot admit another request".into();
    let mut harness = harness_builder().with_size(egui::vec2(1200.0, 900.0)).build_ui_state(
        |ui, (st, reqs): &mut (AiUiState, Vec<AiRequest>)| {
            render_diagnostic_alert_banner(ui, st, false, &[], None, reqs);
        },
        (s, Vec::<AiRequest>::new()),
    );
    harness.run_steps(2);
    harness.get_by_label("Raise Token Limit").click();
    harness.run_steps(2);
    assert!(
        harness.state().0.settings.budget.token_limit >= 6_000_000 && harness.state().0.settings.budget.token_limit <= MAX_TOKEN_LIMIT,
        "raised={}",
        harness.state().0.settings.budget.token_limit
    );
}

#[test]
fn ai28_package5_settings_sync_output_mode_language_toggle_and_bidi_layouter() {
    use egui_kittest::kittest::Queryable;

    // 1. Inactive draft preserves saved user settings (models, budget, image_rates) while keeping draft batch/page settings.
    let mut saved_settings = AiSettings::default();
    saved_settings.text_model.model = "gemini-3.8-flash".into();
    saved_settings.budget.max_requests = 48;
    saved_settings.budget.rates = Some(TokenRates {
        input_per_million: 1.25,
        cached_input_per_million: 0.3,
        output_per_million: 5.0,
        cache_write_per_million: None,
        image_per_request: None,
        image_reserve_usd: None,
    });
    saved_settings.image_rates = Some(TokenRates {
        input_per_million: 0.0,
        cached_input_per_million: 0.0,
        output_per_million: 0.0,
        cache_write_per_million: None,
        image_per_request: Some(0.04),
        image_reserve_usd: None,
    });

    let mut draft_req = continuation_request(TaskTarget::NewA4 { pages: 12 });
    draft_req.settings.output_mode = OutputMode::NativeWithImages;
    draft_req.settings.text_model.model = "gemini-2.5-flash".into();
    draft_req.settings.budget.max_requests = 4;

    let mut state = AiUiState::default();
    state.accept(AiSnapshot {
        available: true,
        settings: saved_settings,
        production_draft: Some(ProductionDraft {
            id: "old-draft".into(),
            request: draft_req,
            document_uid: None,
            document_path: None,
            document_title: "Draft".into(),
            progress: Default::default(),
        }),
        ..Default::default()
    });
    assert_eq!(state.settings.text_model.model, "gemini-3.8-flash");
    assert_eq!(state.settings.budget.max_requests, 48);
    assert_eq!(state.settings.pages_per_batch, 3);
    assert!(state.rates_confirmed, "image_per_request + text rates satisfy NativeWithImages rate confirmation");

    // 2. OutputMode toggle in main Tab 0 + 1-click language switch button in AI window top bar.
    let mut app = DesignApp::new(
        Session::new(),
        Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
    );
    app.ui.ai.open = true;
    app.ui.ai.snapshot.available = true;
    app.ui.ai.initialized = true;
    app.ui.ai.prompt = "صمّم كراسة علوم تفاعلية للأطفال من 8 صفحات".into();

    let mut win = crate::test_window::open(app, egui::vec2(1280.0, 960.0));
    win.run_steps(3);
    assert_eq!(win.state().app.ui.ai.settings.output_mode, OutputMode::NativeOnly);
    win.get_by_label("Editable elements and images").click();
    win.run_steps(3);
    assert_eq!(win.state().app.ui.ai.settings.output_mode, OutputMode::NativeWithImages);

    // Single-page mode also exposes Book binding (RTL/LTR/Automatic).
    assert!(!win.state().app.ui.ai.settings.facing_pages);
    assert_eq!(win.state().app.ui.ai.settings.right_to_left_binding, None);
    win.get_by_label("Right to left (RTL)").click();
    win.run_steps(3);
    assert_eq!(win.state().app.ui.ai.settings.right_to_left_binding, Some(true));

    // Both the top app bar and the AI window expose the 1-click language toggle.
    let toggles: Vec<_> = win.query_all_by_label("العربية").collect();
    assert_eq!(toggles.len(), 2);
    toggles.into_iter().last().unwrap().click();
    win.run_steps(3);
    assert_eq!(win.state().app.ui.language, "ar");
    win.query_all_by_label("EN").next().unwrap().click();
    win.run_steps(3);
    assert_eq!(win.state().app.ui.language, "");

    // 3. Single-page batch review exposes the 100% image preview toggle even when facing_pages == false.
    let mut fixture = ProofFixture::new();
    fixture.app.ui.ai.settings.facing_pages = false;
    let mut single_review = fixture.panel();
    single_review.run_steps(3);
    assert!(single_review.query_by_label("Show facing pair").is_none(), "single-page review does not offer facing pairs");
    assert!(!single_review.state().ui.ai.proof_full_resolution);
    single_review.get_by_label("100% image preview").click();
    single_review.run_steps(3);
    assert!(single_review.state().ui.ai.proof_full_resolution, "single-page review must allow 100% image inspection");
}

#[test]
fn control_channel_ai_request_cannot_escalate_spending_budget_above_user_ceiling() {
    let persisted_rates = TokenRates {
        input_per_million: 2.50,
        cached_input_per_million: 0.25,
        output_per_million: 10.0,
        cache_write_per_million: None,
        image_per_request: Some(0.08),
        image_reserve_usd: None,
    };
    let mut persisted_ceiling = AiSettings::default();
    persisted_ceiling.budget.max_requests = 6;
    persisted_ceiling.budget.token_limit = 120_000;
    persisted_ceiling.budget.max_cost_usd = Some(1.50);
    persisted_ceiling.budget.rates = Some(persisted_rates.clone());
    persisted_ceiling.allow_paid_repairs = false;
    persisted_ceiling.output_mode = OutputMode::NativeOnly;
    persisted_ceiling.verify_generated_images = false;

    let host_settings = std::rc::Rc::new(std::cell::RefCell::new(persisted_ceiling));
    let host_settings_ref = host_settings.clone();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen_host = seen.clone();
    let mut app = DesignApp::new(
        Session::new(),
        Services {
            ai: Some(Box::new(move |_, req| {
                if let AiRequest::UpdateSettings(s) = &req {
                    seen_host.borrow_mut().push(s.clone());
                    *host_settings_ref.borrow_mut() = s.clone();
                }
                AiSnapshot { available: true, settings: host_settings_ref.borrow().clone(), ..Default::default() }
            })),
            ..Default::default()
        },
    );
    // Verify uninitialized `app.ui.ai` hydrates persisted runtime ceiling BEFORE clamping!
    assert!(!app.ui.ai.initialized);

    let mut escalated = AiSettings::default();
    escalated.budget.max_requests = 9_999;
    escalated.budget.token_limit = 15_000_000;
    escalated.budget.max_cost_usd = None;
    escalated.budget.rates = Some(TokenRates {
        input_per_million: 1e-9,
        cached_input_per_million: 1e-9,
        output_per_million: 1e-9,
        cache_write_per_million: None,
        image_per_request: Some(1e-9),
        image_reserve_usd: None,
    });
    escalated.allow_paid_repairs = true;
    escalated.output_mode = OutputMode::NativeWithImages;
    escalated.verify_generated_images = true;

    app.run("ai.request", json!({"method": "update_settings", "settings": escalated})).unwrap();
    assert!(app.ui.ai.initialized, "request_command must hydrate uninitialized settings first");
    let applied = seen.borrow().last().cloned().expect("settings forwarded");
    assert_eq!(applied.budget.max_requests, 6);
    assert_eq!(applied.budget.token_limit, 120_000);
    assert_eq!(applied.budget.max_cost_usd, Some(1.50));
    assert!(!applied.allow_paid_repairs);
    assert_eq!(applied.output_mode, OutputMode::NativeOnly);
    assert!(!applied.verify_generated_images);
    assert_eq!(applied.budget.rates.as_ref().map(|r| r.input_per_million), Some(2.50));

    // Tightening below the ceiling remains allowed while preserving user rate tables.
    let mut tighter = app.ui.ai.settings.clone();
    tighter.budget.max_requests = 2;
    tighter.budget.token_limit = 50_000;
    tighter.budget.max_cost_usd = Some(0.50);
    app.run("ai.request", json!({"method": "update_settings", "settings": tighter})).unwrap();
    let applied = seen.borrow().last().cloned().expect("tighter settings forwarded");
    assert_eq!(applied.budget.max_requests, 2);
    assert_eq!(applied.budget.token_limit, 50_000);
    assert_eq!(applied.budget.max_cost_usd, Some(0.50));
    assert_eq!(applied.budget.rates.as_ref().map(|r| r.output_per_million), Some(10.0));
}
#[test]
fn paid_image_confirmation_requires_human_ui_gesture_and_allows_dismiss() {
    let dismissed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let dismissed_host = dismissed.clone();
    let confirmed = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let confirmed_host = confirmed.clone();
    let mut app = DesignApp::new(
        Session::new(),
        Services {
            ai: Some(Box::new(move |_, req| {
                match &req {
                    AiRequest::DismissImageRegeneration { job_id } => dismissed_host.borrow_mut().push(job_id.clone()),
                    AiRequest::ConfirmImageRegeneration { .. } => confirmed_host.set(confirmed_host.get() + 1),
                    _ => {}
                }
                AiSnapshot { available: true, ..Default::default() }
            })),
            ..Default::default()
        },
    );
    // Remote control-channel confirm_image_regeneration must be rejected:
    let err = app.run("ai.request", json!({"method": "confirm_image_regeneration", "job_id": "job-42"})).unwrap_err();
    assert!(err.contains("human review button"), "err={err}");
    assert_eq!(confirmed.get(), 0);

    // Synthetic control-channel dispatch must also be rejected when synthetic input is active:
    app.in_control = true;
    let err2 = dispatch(&mut app, AiRequest::ConfirmImageRegeneration { job_id: "job-42".into() }).unwrap_err();
    assert!(err2.contains("human gesture"), "err2={err2}");
    assert_eq!(confirmed.get(), 0);

    // Real human UI gesture succeeds:
    app.in_control = false;
    dispatch(&mut app, AiRequest::ConfirmImageRegeneration { job_id: "job-42".into() }).unwrap();
    assert_eq!(confirmed.get(), 1);

    // Dismissing queued images is safe and allowed over ai.request:
    app.run("ai.request", json!({"method": "dismiss_image_regeneration", "job_id": "job-42"})).unwrap();
    assert_eq!(dismissed.borrow().as_slice(), &["job-42"]);
}

#[test]
fn ai_run_prompt_auto_dispatches_when_ready_and_opens_panel_when_unconfigured() {
    let started: std::rc::Rc<std::cell::RefCell<Vec<TaskRequest>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let started_host = started.clone();
    let configured: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(false));
    let configured_host = configured.clone();

    let mut app = DesignApp::new(
        Session::new(),
        Services {
            ai: Some(Box::new(move |_session, req| {
                if let AiRequest::Start(task) = req {
                    started_host.borrow_mut().push(task);
                }
                let mut settings = AiSettings::default();
                if configured_host.get() {
                    settings.text_model = ModelSelection { provider: ProviderKind::Ollama, model: "qwen2.5:14b".into(), endpoint: None };
                    settings.budget.max_requests = 64;
                }
                AiSnapshot { available: true, settings, ..Default::default() }
            })),
            ..Default::default()
        },
    );

    // Hostile/empty explicit prompts fail cleanly, while promptless command invocation opens AI Production awaiting input:
    assert!(app.run("ai.runPrompt", json!({"prompt": "   "})).is_err());
    assert!(app.run("ai.runPrompt", json!({"prompt": "bad\0prompt"})).is_err());
    let awaiting = app.run("ai.runPrompt", json!({})).unwrap();
    assert_eq!(awaiting["open"], json!(true));
    assert_eq!(awaiting["dispatched"], json!(false));
    assert_eq!(awaiting["reason"], json!("awaiting_prompt"));
    assert!(app.ui.ai.open);
    app.ui.ai.open = false;

    // When no text model is configured yet, ai.runPrompt opens AI Production with prompt pre-filled and detects page count:
    let fallback = app.run("ai.runPrompt", json!({"prompt": "Create a 12-page Arabic science workbook"})).unwrap();
    assert_eq!(fallback["open"], json!(true));
    assert_eq!(fallback["dispatched"], json!(false));
    assert_eq!(fallback["target"], json!("new_document"));
    assert!(app.ui.ai.open);
    assert_eq!(app.ui.ai.prompt, "Create a 12-page Arabic science workbook");
    assert_eq!(app.ui.ai.pages, 12);
    assert!(started.borrow().is_empty());

    // When a model is configured and a document is open, Quick Apply natural-language query hands off and auto-starts CurrentDocument:
    app.ui.ai.initialized = false;
    configured.set(true);
    app.run("file.new", json!({"pages": 2})).unwrap();
    app.ui.palette = Some("اربط كل صورة بتسمية حية مجمعة أسفلها".into());

    let mut h = harness_builder().with_size(egui::vec2(900.0, 600.0)).build_ui_state(|ui, app| crate::menus::palette(app, ui.ctx()), app);
    h.run_steps(3);
    assert!(h.query_by_label_contains("Run with AI…").is_some(), "Quick Apply must show Run with AI… for natural-language queries");
    h.get_by_label_contains("Run with AI…").click();
    h.run_steps(2);

    assert!(h.state().ui.palette.is_none(), "Quick Apply closes after handing off to AI");
    assert!(h.state().ui.ai.open, "AI Production panel opens");
    assert_eq!(started.borrow().len(), 1);
    assert_eq!(started.borrow()[0].prompt, "اربط كل صورة بتسمية حية مجمعة أسفلها");
    assert!(matches!(started.borrow()[0].target, TaskTarget::CurrentDocument));

    // Even when a multi-word query partially matches an existing command ("new document" -> "New Document…"),
    // Quick Apply also offers the trailing "Run with AI…" option:
    h.state_mut().ui.palette = Some("new document".into());
    h.run_steps(3);
    assert!(h.query_by_label_contains("New Document…").is_some(), "matching command stays first");
    assert!(h.query_by_label_contains("Run with AI…").is_some(), "trailing Run with AI… is offered for multi-word queries");
    h.get_by_label_contains("Run with AI…").click();
    h.run_steps(2);
    assert_eq!(started.borrow().len(), 2);
    assert_eq!(started.borrow()[1].prompt, "new document");
}

#[test]
fn ai33_ui_large_script_card_slicing_scoped_skill_errors_and_blank_planner_continuation() {
    use egui_kittest::kittest::Queryable;

    // 1. Flexible explicit script headings: PAGES, zero-padded numbers, ascending ranges, numeric titles, and Arabic ranges.
    let mixed_script = "\
### PAGES 001 - 2026 Starlight Cover\n\
### PAGE 002 - Yearly Vision\n\
### PAGES 003-004 - January Spread\n\
### الصفحات ٥-٦ - تخطيط الأسبوع\n";
    assert_eq!(explicit_script_pages(mixed_script), Some(6));
    assert_eq!(natural_prompt_pages("صمم بلانر سنوي متكامل وشامل وإبداعي"), Some(365));

    // 2. 739 KB 365-page Starlight & Blossom script: sliced token estimate, SmartHybrid/PureNativeVector allowances, and zero-lag Large Script Card.
    let mut big_script = String::with_capacity(750_000);
    big_script.push_str("# COMPLETE 365-PAGE STARLIGHT & BLOSSOM WONDERLAND PLANNER\n");
    for p in 1..=365u32 {
        big_script.push_str(&format!(
            "### PAGES {p:03} - Starlight Daily Page {p}\n* **Grid & Layout Architecture:** A5 148x210mm, 12mm margin.\n* **Visual & Editorial Styling Prompt:** Watercolor botanical border.\n* **Exact Verbatim Copy:** {}\n\n",
            "Focus & Gratitude Schedule · ".repeat(65)
        ));
    }
    assert!(big_script.len() > 730_000 && big_script.len() < designcraft_ai_types::protocol::MAX_BRIEF_BYTES);

    let mut state = AiUiState { prompt: big_script, new_document: true, pages: 365, ..Default::default() };
    state.settings.text_model.model = "gemini-3.8-flash".into();
    state.settings.image_model.provider = ProviderKind::Gemini;
    state.settings.image_model.model = "gemini-3.1-flash-image".into();
    state.settings.output_mode = OutputMode::NativeWithImages;
    state.settings.visual_density = VisualDensityMode::SmartHybrid;
    state.settings.verify_generated_images = true;
    state.settings.pages_per_batch = 6;

    let hybrid_est = state.request_estimate(&[], None).expect("365-page planner estimate");
    assert!(hybrid_est.scripted);
    assert_eq!(hybrid_est.remaining_pages, 365);
    assert!(hybrid_est.book_allowance < 450, "SmartHybrid reuses seasonal motifs instead of 3,285 calls: {}", hybrid_est.book_allowance);
    let hybrid_tok = state.token_estimate(&[], None).expect("365-page token estimate");
    assert!(
        hybrid_tok.per_page_tokens < 60_000,
        "Per-page tokens must reflect sliced batch script, not 247k full-script per page: {}",
        hybrid_tok.per_page_tokens
    );

    state.settings.visual_density = VisualDensityMode::PureNativeVector;
    state.settings.image_model.model.clear();
    assert_eq!(state.production_issue(true, false, false), None, "PureNativeVector requires no image model");
    let vector_est = state.request_estimate(&[], None).expect("pure vector estimate");
    assert_eq!(vector_est.image_allowance, 0);
    assert_eq!(vector_est.vision_allowance, 0);
    assert_eq!(vector_est.book_allowance, 365);

    // 3. Skill-scoped import error never triggers Production tab diagnostic alert, and Skills tab sanitizes quoted paths.
    state.snapshot.stage = "failed".into();
    state.snapshot.message = "skill I/O: The filename, directory name, or volume label syntax is incorrect. (os error 123)".into();
    assert!(state.active_diagnostic_alert().is_none(), "Skill-tab I/O errors must not lock an AI Production Alert onto Tab 0");

    let mut prod_panel = harness_builder()
        .with_size(egui::vec2(1280.0, 960.0))
        .build_ui_state(|ui, (st, reqs)| production(ui, st, false, true, &[], &mut None, reqs), (state, Vec::<AiRequest>::new()));
    prod_panel.run_steps(3);
    assert!(prod_panel.query_by_label_contains("Large production script loaded").is_some(), "739 KB script renders fast card");
    assert!(prod_panel.query_by_label_contains("Explicit PAGE script is sliced automatically per batch").is_some());
    assert!(!prod_panel.get_by_label("Produce").accesskit_node().is_disabled());

    // Verify quoted Windows Copy-as-path is sanitized before ImportSkill is dispatched:
    let mut skill_state =
        AiUiState { import_path: "  \"C:\\Users\\alara\\Desktop\\planners-journals-notebooks-studio\"  ".into(), ..Default::default() };
    skill_state.snapshot.stage = "failed".into();
    skill_state.snapshot.message = "skill I/O: quoted path failed".into();
    let mut skill_panel = harness_builder()
        .with_size(egui::vec2(1100.0, 800.0))
        .build_ui_state(|ui, (st, reqs)| skills(ui, st, false, true, true, reqs), (skill_state, Vec::<AiRequest>::new()));
    skill_panel.run_steps(2);
    assert!(skill_panel.query_by_label_contains("skill I/O: quoted path failed").is_some());
    skill_panel.get_by_label("Import draft").click();
    skill_panel.run_steps(2);
    assert!(matches!(
        skill_panel.state().1.last(),
        Some(AiRequest::ImportSkill { path }) if path == "C:\\Users\\alara\\Desktop\\planners-journals-notebooks-studio"
    ));

    // 4. 1-page blank document (`بلانر`) supports Continue remaining book pages and auto-expands to 365 pages!
    let mut blank_planner_doc = Session::new();
    blank_planner_doc.execute("file.new", &json!({"pages": 1, "title": "بلانر"})).unwrap();
    let open_docs = vec![OpenDocument::from_document(0, blank_planner_doc.active().unwrap())];
    let mut cont_state = AiUiState {
        prompt: "Create a complete 365-page creative planner and journal".into(),
        new_document: false,
        continue_book: true,
        ..Default::default()
    };
    cont_state.settings.text_model.model = "gemini-3.8-flash".into();
    let mut cont_panel = harness_builder().with_size(egui::vec2(1280.0, 960.0)).build_ui_state(
        move |ui, (st, reqs, sel)| production(ui, st, false, true, &open_docs, sel, reqs),
        (cont_state, Vec::<AiRequest>::new(), Some(0usize)),
    );
    cont_panel.run_steps(3);
    assert!(cont_panel.query_by_label("Blank document will automatically expand from 1 to 365 pages and start at page 1.").is_some());
    assert!(!cont_panel.get_by_label("Produce").accesskit_node().is_disabled());
}
