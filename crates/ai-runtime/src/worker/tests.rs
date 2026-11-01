use super::*;
use designcraft_ai_providers::Usage;
use std::cell::RefCell;

fn ai24_png(width: u32, height: u32, color: u8) -> providers::GeneratedImage {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(width, height, image::Rgb([color, 80, 120])).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "candidate.png".into() }
}

#[test]
fn ai24_repeated_unit_placements_stay_within_inline_raster_and_action_limits() {
    struct UnitProvider {
        image: providers::GeneratedImage,
        requests: RefCell<Vec<String>>,
    }
    impl Provider for UnitProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, r: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.requests.borrow_mut().push(r.prompt.clone());
            Ok(GenerationResponse { text: json!({"verdict":"pass","reasons":[]}).to_string(), usage: Usage::default(), error: None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, r: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.requests.borrow_mut().push(r.prompt.clone());
            Ok(ImageResponse { images: vec![self.image.clone()], usage: Usage::default(), error: None })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let mut rng = 87u32;
    let pixels = image::RgbImage::from_fn(1024, 1024, |_, _| {
        let color = std::array::from_fn(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng as u8
        });
        image::Rgb(color)
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let image = providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "fish.png".into() };
    assert!(image.bytes.len() > 3 * 1024 * 1024);
    for groups in [vec![10, 4], vec![20, 4]] {
        let count: usize = groups.iter().sum();
        let raw = json!({"title":"Counted subjects", "commands":[], "images":[{
            "prompt":"One isolated reef fish", "page":13,"rect":[60,180,540,430],"label":"Fish", "repeat_groups":groups
        }]});
        let mut plan = crate::script_contract::compile_native_plan(&raw.to_string(), BatchScope { start: 13, count: 1 }).unwrap();
        if count == 24
            && let Some(last) = plan.images[0].repeat_rects.last_mut()
        {
            last[2] = last[0] + (last[2] - last[0]) * 0.8;
            last[3] = last[1] + (last[3] - last[1]) * 0.8;
        }
        let cells: Vec<_> = plan.images[0].all_rects().collect();
        let settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            verify_generated_images: true,
            text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
            budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
            ..Default::default()
        };
        let provider = UnitProvider { image: image.clone(), requests: RefCell::new(vec![]) };
        let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let mut runner = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        };
        let expanded = runner.expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, None).unwrap();
        assert_eq!(ledger.summary().requests, 2, "one image and one advisory review for the whole set");
        assert_eq!(provider.requests.borrow().len(), 2);
        assert!(provider.requests.borrow().iter().all(|request| request.contains("EXACTLY ONE single isolated subject")));
        assert_eq!(expanded.commands.iter().filter(|a| a.command == "file.place").count(), 1, "inline source is embedded once");
        assert!(serde_json::to_vec(&expanded).unwrap().len() < 6 * 1024 * 1024);
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":14,"facingPages":false})).unwrap();
        session
            .execute("production.apply", &json!({"commands":expanded.commands,"outputMode":"native_with_images","requirePreflight":false}))
            .unwrap();
        let doc = &session.doc().unwrap().doc;
        let items: Vec<_> = doc.spreads[13].items.iter().filter(|item| item.graphic().is_some()).collect();
        assert_eq!(items.len(), count);
        let asset = items[0].graphic().unwrap().asset;
        assert!(items.iter().all(|item| item.graphic().unwrap().asset == asset));
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item.label, format!("Fish / unit {}", index + 1));
            let r = item.bounds();
            assert!(cells[index].iter().zip([r.x0, r.y0, r.x1, r.y1]).all(|(a, b)| (*a - b).abs() < 0.001));
            assert!(r.x0 >= 60.0 && r.x1 <= 540.0 && r.y0 >= 180.0 && r.y1 <= 430.0);
            assert!(items.iter().take(index).all(|other| {
                let o = other.bounds();
                r.x1 <= o.x0 || r.x0 >= o.x1 || r.y1 <= o.y0 || r.y0 >= o.y1
            }));
        }
        let first = items[0].id;
        session.execute("object.label", &json!({"ids":[first.0],"label":"Manual native edit"})).unwrap();
        session.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().doc.item(first).unwrap().label, "Fish / unit 1");
        let first_graphic = session.doc().unwrap().doc.item(first).unwrap().graphic().cloned();
        session.execute("object.fittingOptions", &json!({"ids":[first.0],"fitting":"fitProportionally","crop":[1,1,1,1]})).unwrap();
        session.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().doc.item(first).unwrap().graphic().cloned(), first_graphic);
        session.execute("edit.undo", &json!({})).unwrap();
        assert!(session.doc().unwrap().doc.spreads[13].items.is_empty());
    }
}

#[test]
fn ai24_repeated_cell_action_reservation_matches_native_copy_cost() {
    for (authored, accepted) in [(420, true), (470, false)] {
        let mut plan = crate::script_contract::compile_native_plan(
            &json!({"title":"Units","commands":[],"images":[{
                "prompt":"One fish","page":0,"rect":[60,180,540,430],"label":"Fish","repeat_groups":[20,4]
            }]})
            .to_string(),
            BatchScope { start: 0, count: 1 },
        )
        .unwrap();
        plan.commands = vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,100,100]}) }; authored];
        let settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
            ..Default::default()
        };
        let provider = Ai23SyntheticProvider::new(vec![]);
        let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let mut runner = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        };
        let result = runner.expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, None);
        if accepted {
            let result = result.expect("uniform-size native copies need only move-copy and label; do not reject a valid dense page");
            assert_eq!(result.commands.len(), 470);
            assert_eq!(ledger.summary().requests, 1);
        } else {
            let error = result.unwrap_err();
            assert!(error.contains("action limit"), "{error}");
            assert_eq!(ledger.summary().requests, 0);
            assert!(provider.image_sizes.borrow().is_empty());
        }
    }
}

#[test]
fn ai24_cached_plan_with_no_room_for_repeated_images_is_rejected_before_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new();
    session.execute("file.new", &json!({"facingPages":false})).unwrap();
    let mut plan = crate::script_contract::compile_native_plan(
        &json!({"title":"Dense page","commands":[],"images":[{
            "prompt":"One fish","page":0,"rect":[60,180,540,430],"label":"Fish","repeat_groups":[20,4]
        }]})
        .to_string(),
        BatchScope { start: 0, count: 1 },
    )
    .unwrap();
    plan.commands =
        vec![EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[60,60,100,100],"content":"unassigned"}) }; 470];
    let key = "f".repeat(64);
    crate::production_cache::store_plan(dir.path(), &key, &plan).unwrap();
    let provider = Ai23SyntheticProvider::new(vec![]);
    let request = TaskRequest {
        prompt: "Draw counted units".into(),
        settings: AiSettings::default(),
        skill_ids: vec![],
        auto_select_skills: true,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(request.settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let runner = Runner {
        cache_root: Some(dir.path().to_path_buf()),
        print: None,
        planning_only: true,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let cached = runner
        .cached_plan(
            Some(&key),
            &request,
            &json!({}),
            session.doc().unwrap(),
            &session.prefs,
            None,
            &designcraft_engine::cmd::production::catalog(),
            None,
        )
        .unwrap();
    assert!(cached.is_none(), "a cached native draft must reserve space for its pending generated-image commands");
    assert_eq!(ledger.summary().requests, 0);
}

#[test]
fn ai24_authored_matching_page_zones_override_only_the_matching_novelty_pair() {
    let actions = |page| {
        [[60, 160, 285, 240], [310, 160, 540, 240], [60, 300, 285, 380], [310, 300, 540, 380]]
            .into_iter()
            .map(|rect| EngineAction { command: "frame.create".into(), params: json!({"spread":page,"content":"unassigned","rect":rect}) })
            .collect::<Vec<_>>()
    };
    let script = |second: &str| {
        format!(
            "### PAGE 01 — First numbers\n* **Layout & Grid Architecture:** Top 42%: First number. Middle 42%: Second number. Bottom 16%: Match.\n### PAGE 02 — Next numbers\n{second}\n"
        )
    };
    let authored = script("* **Layout & Grid Architecture:** Top 42%: Third number. Middle 42%: Fourth number. Bottom 16%: Complete.");
    let mut plan =
        ProductionPlan { title: "Two authored lessons".into(), commands: actions(0).into_iter().chain(actions(1)).collect(), images: vec![] };
    prepare_editorial_plan(&mut plan, &authored, &json!({}), Some(BatchScope { start: 0, count: 2 })).unwrap();
    for brief in [
        "Create two varied lessons".to_owned(),
        script("* **Layout & Grid Architecture:** Top 50%: Explore. Middle 30%: Draw. Bottom 20%: Explain."),
        script("* **English Print Copy (LTR):** Next lesson."),
    ] {
        assert!(
            prepare_editorial_plan(&mut plan, &brief, &json!({}), Some(BatchScope { start: 0, count: 2 }))
                .unwrap_err()
                .contains("Repeated body layout")
        );
    }
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":2,"facingPages":false})).unwrap();
    session.execute("production.apply", &json!({"outputMode":"native_only","commands":actions(0),"requirePreflight":false})).unwrap();
    let next = ProductionPlan { title: "Continue authored lesson".into(), commands: actions(1), images: vec![] };
    validate_previous_body_layout(&next, session.doc().unwrap(), &session.prefs, Some(BatchScope { start: 1, count: 1 }), &authored).unwrap();
    assert!(
        validate_previous_body_layout(
            &next,
            session.doc().unwrap(),
            &session.prefs,
            Some(BatchScope { start: 1, count: 1 }),
            "Create varied lessons"
        )
        .unwrap_err()
        .contains("Repeated body layout")
    );
}

fn ai24_expand_candidates(
    images: Vec<providers::GeneratedImage>,
    min_ppi: Option<f64>,
) -> (Result<ProductionPlan, String>, UsageSummary, Vec<Value>) {
    struct Candidates(Vec<providers::GeneratedImage>);
    impl Provider for Candidates {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("No extra paid planning or inspection is authorized by this fixture".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Ok(ImageResponse {
                images: self.0.clone(),
                usage: Usage { input_tokens: Some(41), output_tokens: Some(91), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
        ..Default::default()
    };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: min_ppi.map(|min_ppi| crate::print_contract::PrintContract { min_ppi: Some(min_ppi), ..Default::default() }),
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &Candidates(images),
    }
    .expand_images(
        ProductionPlan {
            title: "Page 16".into(),
            commands: vec![],
            images: vec![designcraft_ai_types::protocol::ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: Vec::new(),
                repeat_groups: Vec::new(),
                prompt: "One complete classroom illustration".into(),
                page: 15,
                rect: [60.0, 160.0, 420.0, 280.0],
                label: "Native editable image frame".into(),
            }],
        },
        &settings,
        &mut BTreeMap::new(),
        &mut 0,
        None,
    );
    let issues = receiver.try_iter().filter_map(|message| if let Message::Diagnostic(value) = message { Some(value) } else { None }).collect();
    (result, ledger.summary(), issues)
}

#[test]
fn ai24_multiple_final_images_choose_matching_geometry_without_an_extra_request() {
    let wide = ai24_png(900, 300, 200);
    let (result, usage, issues) = ai24_expand_candidates(vec![ai24_png(300, 900, 20), wide.clone()], None);
    let plan = result.unwrap();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.output_tokens, 91);
    assert!(plan.images.is_empty());
    let placed: Vec<_> = plan.commands.iter().filter(|action| action.command == "file.place").collect();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].params["base64"], base64::engine::general_purpose::STANDARD.encode(wide.bytes));
    assert!(
        issues.iter().any(|issue| issue["kind"] == "imageOutputSelection" && issue["selected_index"] == 1 && issue["human_review_required"] == true)
    );
}

#[test]
fn ai24_final_image_selection_filters_print_resolution_before_aspect_score() {
    let printable = ai24_png(1800, 700, 200);
    let (result, usage, _) = ai24_expand_candidates(vec![ai24_png(900, 300, 20), printable.clone()], Some(300.0));
    let plan = result.unwrap();
    assert_eq!(usage.requests, 1);
    let placed = plan.commands.iter().find(|action| action.command == "file.place").unwrap();
    assert_eq!(placed.params["base64"], base64::engine::general_purpose::STANDARD.encode(printable.bytes));
}

#[test]
fn ai24_image_selection_rejects_empty_invalid_or_unprintable_sets_and_keeps_usage() {
    let invalid = providers::GeneratedImage { bytes: vec![0; 8], mime_type: "image/png".into(), filename: "bad.png".into() };
    for images in [vec![], vec![invalid], vec![ai24_png(900, 300, 10), ai24_png(600, 200, 50)]] {
        let (result, usage, _) = ai24_expand_candidates(images, Some(300.0));
        assert!(result.is_err());
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.input_tokens, 41);
    }
}

#[test]
fn ai24_final_image_selection_is_stable_and_can_skip_a_bad_candidate() {
    let first = ai24_png(900, 300, 10);
    for candidates in [
        vec![first.clone(), ai24_png(900, 300, 200)],
        vec![providers::GeneratedImage { bytes: vec![], mime_type: "image/png".into(), filename: "bad.png".into() }, first.clone()],
    ] {
        let (result, usage, _) = ai24_expand_candidates(candidates, None);
        let plan = result.unwrap();
        assert_eq!(usage.requests, 1);
        let placed = plan.commands.iter().find(|action| action.command == "file.place").unwrap();
        assert_eq!(placed.params["base64"], base64::engine::general_purpose::STANDARD.encode(&first.bytes));
    }
}

struct Ai23SyntheticProvider {
    plans: RefCell<Vec<ProductionPlan>>,
    image_sizes: RefCell<Vec<Option<String>>>,
    content: Option<Value>,
}

impl Ai23SyntheticProvider {
    fn new(plans: Vec<ProductionPlan>) -> Self {
        Self { plans: RefCell::new(plans), image_sizes: RefCell::new(vec![]), content: None }
    }
}

impl Provider for Ai23SyntheticProvider {
    fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
        let text = if let Some(content) = &self.content {
            content.to_string()
        } else {
            let plan = self.plans.borrow_mut().pop().ok_or("Unexpected synthetic planning request")?;
            serde_json::to_string(&plan).map_err(|error| error.to_string())?
        };
        Ok(GenerationResponse { text, usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() }, error: None })
    }

    fn images(&self, _: &ModelSelection, _: Option<&str>, request: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
        self.image_sizes.borrow_mut().push(request.size.clone());
        let (width, height) = if request.size.as_deref().is_some_and(|size| size.starts_with("4:1")) { (1600, 400) } else { (1024, 1024) };
        let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([20, 80, 100]));
        let mut bytes = std::io::Cursor::new(vec![]);
        pixels.write_to(&mut bytes, image::ImageFormat::Png).map_err(|error| error.to_string())?;
        Ok(ImageResponse {
            images: vec![providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "synthetic.png".into() }],
            usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
            error: None,
        })
    }

    fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
        Err("No synthetic model discovery".into())
    }
}

#[test]
fn ai23_template_family_checks_previous_native_page_and_internal_sequence() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut profile = shared_source_profile_fixture();
    let original = profile.page_families[0].clone();
    let mut templates = BTreeMap::new();
    profile.page_families.clear();
    for id in ["family-a", "family-b"] {
        let mut family = original.clone();
        family.id = id.into();
        family.template = format!("templates/{id}.json");
        family.image_slots.clear();
        family.text_slots = vec![designcraft_ai_types::DesignTextSlot { id: "heading".into(), purpose: "Lesson".into(), command: 0, max_chars: 50 }];
        templates.insert(
            family.template.clone(),
            vec![
                EngineAction {
                    command: "frame.create".into(),
                    params: json!({"spread":0,"content":"text","text":"Lesson","rect":[60,180,300,230],"caret":false}),
                },
                EngineAction { command: "object.label".into(), params: json!({"ids":["$0.id"],"label":format!("native.{id}")}) },
            ],
        );
        profile.page_families.push(family);
    }
    let style = SkillSnapshot {
        skill_id: "family-fixture".into(),
        revision: "fixed".into(),
        instructions: String::new(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates,
        design_profile: Some(profile.clone()),
    };
    for (start, families, brief, repeated) in [
        (1, vec!["family-a"], "Vary the science lessons", true),
        (1, vec!["family-b"], "Vary the science lessons", false),
        (1, vec!["family-a"], "Use the same layout", false),
        (0, vec!["family-a", "family-a"], "Vary the science lessons", true),
        (0, vec!["family-a", "family-b"], "Vary the science lessons", false),
    ] {
        let mut session = Session::new();
        session.execute("file.new", &crate::design::document_params(Some(&profile), 2)).unwrap();
        if start > 0 {
            let item = session
                .execute("frame.create", &json!({"spread":0,"content":"text","text":"Previous","rect":[60,180,300,230],"caret":false}))
                .unwrap();
            session.execute("object.label", &json!({"ids":[item["id"]],"label":"native.family-a"})).unwrap();
            let other = session.execute("frame.create", &json!({"spread":0,"content":"unassigned","rect":[60,240,80,260]})).unwrap();
            session.execute("object.group", &json!({"ids":[item["id"],other["id"]]})).unwrap();
        }
        let scope = BatchScope { start, count: families.len() as u32 };
        let pages: Vec<_> = families
            .iter()
            .enumerate()
            .map(|(index, family)| json!({"page":start+index as u32,"family":family,"text":{"heading":"Next"},"illustrations":[]}))
            .collect();
        let provider =
            Ai23SyntheticProvider { content: Some(json!({"title":"Family sequence","pages":pages})), ..Ai23SyntheticProvider::new(vec![]) };
        let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
        let request = TaskRequest {
            prompt: brief.into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 2 },
            export_directory: None,
        };
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let before = session.doc().unwrap().doc.clone();
        let result = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .template_content(&request, &json!({}), &style, session.doc().unwrap(), &session.prefs, scope);
        if repeated {
            assert!(
                result.err().is_some_and(|error| error.contains("Repeated template family")),
                "The same family must not repeat across a native boundary or within one internal part"
            );
        } else {
            assert!(result.is_ok(), "{brief}: {:?}", result.err());
        }
        assert!(Arc::ptr_eq(&before, &session.doc().unwrap().doc));
        assert!(provider.image_sizes.borrow().is_empty());
    }
}

#[test]
fn ai23_body_variety_uses_final_transformed_native_geometry() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":2,"facingPages":false})).unwrap();
    let mut commands = Vec::new();
    for row in 0..4 {
        let rect = json!([60, 180 + row * 100, 300, 250 + row * 100]);
        session.execute("frame.create", &json!({"spread":0,"content":"unassigned","rect":rect})).unwrap();
        let index = commands.len();
        commands.push(EngineAction { command: "frame.create".into(), params: json!({"spread":1,"content":"unassigned","rect":rect}) });
        commands.push(EngineAction { command: "transform.move".into(), params: json!({"ids":[format!("${index}.id")],"dx":50,"dy":0}) });
    }
    let plan = ProductionPlan { title: "Moved body".into(), commands, images: vec![] };
    assert!(
        validate_previous_body_layout(&plan, session.doc().unwrap(), &session.prefs, Some(BatchScope { start: 1, count: 1 }), "Vary the layouts")
            .is_ok(),
        "Final moved frames differ from the previous native page"
    );
}

#[test]
fn ai23_single_page_parts_check_body_variety_against_actual_previous_native_page() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let part = |page| ProductionPlan {
        title: format!("Lesson {page}"),
        commands: (0..4)
            .map(|row| EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":page,"content":"unassigned","rect":[60,180+row*100,300,250+row*100]}),
            })
            .collect(),
        images: vec![],
    };
    let provider = Ai23SyntheticProvider::new(vec![part(1), part(0)]);
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":2,"facingPages":false})).unwrap();
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: "Create varied lessons.\nImage Resolution: 300 PPI".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 2 },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({"batch_scope":{"start":0,"count":2}}), catalog(), vec![], session.doc().unwrap().clone(), session.prefs.clone());
    let error = result.err();
    assert!(
        error.as_ref().is_some_and(|error| error.contains("Repeated body layout")),
        "A one-page internal chunk must not bypass adjacent-page validation: {error:?}"
    );
    assert!(session.doc().unwrap().doc.spreads.iter().all(|spread| spread.items.is_empty()));
}

#[test]
fn ai23_page_zero_authored_image_frame_accepts_omitted_and_null_spread() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    for spread in [Some(json!(0)), None, Some(Value::Null)] {
        let provider = Ai23SyntheticProvider::new(vec![]);
        let settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
            budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
            ..Default::default()
        };
        let mut plan = review_plan();
        let mut params = json!({"content":"graphic","rect":plan.images[0].rect});
        if let Some(spread) = spread {
            params["spread"] = spread;
        }
        plan.commands.push(EngineAction { command: "frame.create".into(), params });
        let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let expanded = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, None)
        .unwrap();
        assert_eq!(expanded.commands.iter().filter(|action| action.command == "frame.create").count(), 1, "Fill the existing page-zero frame");
        let placements: Vec<_> = expanded.commands.iter().filter(|action| action.command == "file.place").collect();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].params["frame"], "$0.id");
        assert_eq!(provider.image_sizes.borrow().len(), 1);
        assert_eq!(ledger.summary().requests, 1);
    }
}

#[test]
fn ai23_cached_plan_emits_measured_native_layout_recovery_diagnostics() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false,"margins":42.52})).unwrap();
    let raw: ProductionPlan = serde_json::from_value(json!({"title":"Measured text", "images":[], "commands":[
            {"command":"frame.create","params":{"spread":0,"rect":[60,60,225,82],"content":"text","text":"Count the flowers carefully. Write each number. Compare your answer with your partner.","caret":false}},
            {"command":"type.char","params":{"attrs":{"size":12,"leading":{"kind":"points","value":17}}}},
            {"command":"frame.create","params":{"spread":0,"rect":[60,180,225,280],"content":"graphic"}}
        ]})).unwrap();
    let base = session.doc().unwrap().clone();
    let cancel = AtomicBool::new(false);
    let mut expected = raw.clone();
    let expected_notes = crate::native_layout::prepare(&mut expected, &base, &session.prefs, &cancel).unwrap();
    assert!(expected_notes.iter().any(|note| note["kind"] == "nativeTextReflow"), "The fixture must exercise real measured recovery");
    let key = "c".repeat(64);
    crate::production_cache::store_plan(directory.path(), &key, &raw).unwrap();
    let request = TaskRequest {
        prompt: "Create one editable lesson".into(),
        settings: AiSettings::default(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let provider = Ai23SyntheticProvider::new(vec![]);
    let mut ledger = UsageLedger::new(request.settings.budget.clone()).unwrap();
    let (sender, receiver) = mpsc::channel();
    let cached = Runner {
        cache_root: Some(directory.path().to_path_buf()),
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .cached_plan(Some(&key), &request, &json!({}), &base, &session.prefs, None, &designcraft_engine::cmd::production::catalog(), None)
    .unwrap();
    assert!(
        cached.is_some(),
        "Cached plan was ignored: {:?}",
        receiver.try_iter().filter_map(|message| if let Message::Diagnostic(issue) = message { Some(issue) } else { None }).collect::<Vec<_>>()
    );
    let cached = cached.unwrap();
    assert_eq!(cached.commands, expected.commands);
    let emitted: Vec<_> =
        receiver.try_iter().filter_map(|message| if let Message::Diagnostic(issue) = message { Some(issue) } else { None }).collect();
    for note in expected_notes {
        assert!(emitted.contains(&note), "Cached recovery must emit the same measured note: {note}");
    }
    assert_eq!(ledger.summary().requests, 0);
    assert!(provider.image_sizes.borrow().is_empty());
    assert!(Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn ai23_group_print_admission_checks_later_resolution_before_first_image() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let make_plan = |page, rect| -> ProductionPlan {
        serde_json::from_value(json!({"title":format!("Lesson {page}"), "commands":[
                {"command":"frame.create","params":{"spread":page,"content":"graphic","rect":rect}}
            ], "images":[{"page":page,"rect":rect,"prompt":format!("Original lesson scene {page}"),"label":format!("Scene {page}")}]}))
        .unwrap()
    };
    // At 800 PPI, the first 72-point square needs 800 pixels and is supported;
    // the later 432-point square needs 4800 pixels, beyond the 4K square tier.
    let provider = Ai23SyntheticProvider::new(vec![make_plan(1, [60, 60, 492, 492]), make_plan(0, [60, 60, 132, 132])]);
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 8, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Image Resolution: 800 PPI\nCreate two different editable lessons.".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 2 },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({"batch_scope":{"start":0,"count":2}}), catalog(), vec![], base.clone(), session.prefs.clone());
    let error = result.err().expect("An unsupported later image tier must reject the whole review");
    assert!(error.contains("4K"), "{error}");
    assert!(provider.plans.borrow().is_empty(), "Both native parts must be validated before image admission");
    assert!(provider.image_sizes.borrow().is_empty(), "Predictably unsupported later artwork must not buy an earlier image");
    assert_eq!(ledger.summary().requests, 2, "Projection does not charge the live ledger");
    assert!(Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn ai23_fixed_profile_generic_group_keeps_shared_source_crops_and_views() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut profile = shared_source_profile_fixture();
    for family in &mut profile.page_families {
        family.text_slots.clear();
    }
    profile.validate().unwrap();
    let skill = SkillSnapshot {
        skill_id: "ai23-fixed-generic-style".into(),
        revision: "ai23-fixed-profile-pin".into(),
        instructions: "Preserve the pinned shared source and its native views.".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::new(),
        design_profile: Some(profile.clone()),
    };
    assert!(!crate::template_plan::eligible(&skill), "Exercise the fixed-profile generic path");
    let part = |start: u32, count: u32| -> ProductionPlan {
        let commands: Vec<_> = (start..start + count)
            .map(|page| json!({"command":"frame.create","params":{"spread":page,"content":"graphic","rect":[60,180,160,280]}}))
            .collect();
        let images: Vec<_> = (start..start + count)
            .map(|page| json!({"page":page,"rect":[60,180,160,280],"prompt":format!("Distinct original atlas scene {page}"),"label":"atlas art"}))
            .collect();
        serde_json::from_value(json!({"title":format!("Atlas part {start}"),"commands":commands,"images":images})).unwrap()
    };
    let provider = Ai23SyntheticProvider::new(vec![part(4, 1), part(0, 4)]);
    let mut session = Session::new();
    session.execute("file.new", &crate::design::document_params(Some(&profile), 5)).unwrap();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 1_000_000, max_requests: 16, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Create five distinct editable atlas lessons.".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 5 },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let product = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({"batch_scope":{"start":0,"count":5}}), catalog(), vec![skill], base.clone(), session.prefs.clone())
    .unwrap();
    let Product::BatchGroup { plans, .. } = product else { panic!("Expected a complete generic review group") };
    assert_eq!(plans.len(), 2);
    let sizes = provider.image_sizes.borrow();
    assert_eq!(sizes.len(), 5, "Distinct page identities must remain distinct sources");
    assert!(sizes.iter().all(|size| size.as_deref() == Some("4:1")), "Keep the pinned atlas ratio in every internal part: {sizes:?}");
    assert_eq!(
        plans.iter().flat_map(|plan| &plan.commands).filter(|action| action.command == "file.place").count(),
        10,
        "Each source has its authored frame and extra native view"
    );
    assert_eq!(
        plans.iter().flat_map(|plan| &plan.commands).filter(|action| action.command == "object.fittingOptions").count(),
        10,
        "Every primary crop and extra view retains its fitting options"
    );
    assert_eq!(ledger.summary().requests, 7);
    assert!(Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn long_bilingual_brief_reaches_model_without_truncation() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let brief = format!("{}END_OF_EXACT_BRIEF", "English + العربية مشكول؛ ".repeat(2048));
    assert!(brief.len() > 32 * 1024);
    validate_prompt(&brief).unwrap();
    assert!(validate_prompt(&"x".repeat(designcraft_ai_types::protocol::MAX_BRIEF_BYTES + 1)).is_err());
    struct LongBriefProvider(String);
    impl Provider for LongBriefProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(value["request"], self.0, "The complete bilingual brief must reach the model");
            Ok(GenerationResponse { text:json!({"title":"Long brief","commands":[{"command":"frame.create","params":{"rect":[54,54,154,154],"content":"unassigned"}}],"images":[]}).to_string(),
                    usage:Usage {input_tokens:Some(100),output_tokens:Some(10),..Default::default()},error:None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No images".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let provider = LongBriefProvider(brief.clone());
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: brief,
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({}), catalog(), vec![], session.doc().unwrap().clone(), session.prefs.clone());
    assert!(result.is_ok(), "{}", result.err().unwrap_or_default());
    assert_eq!(ledger.summary().requests, 1);
}

#[test]
fn explicit_mashkool_request_preserves_generated_arabic_vowels() {
    let copy = "عِلْمٌ وَتَعَلُّمٌ";
    for prompt in ["English + Arabic مشكول", "اكتب نصاً عربياً مشكولاً"] {
        let mut plan = ProductionPlan {
            title: copy.into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"text":copy}) }],
            images: vec![],
        };
        prepare_editorial_plan(&mut plan, prompt, &json!({}), None).unwrap();
        assert_eq!(plan.title, copy);
        assert_eq!(plan.commands[0].params["text"], copy);
    }
}

#[test]
fn current_document_uses_open_geometry_and_ignores_unused_profile_atlas() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let skill = SkillSnapshot {
        skill_id: "current-style".into(),
        revision: "pin".into(),
        instructions: "Preserve this visual identity".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::new(),
        design_profile: Some(shared_source_profile_fixture()),
    };
    let provider = Mock { responses: RefCell::new(vec![Ok(GenerationResponse {
            text: json!({"title":"Existing page","commands":[{"command":"frame.create","params":{"rect":[10,10,110,110],"content":"unassigned"}}],"images":[]}).to_string(),
            usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
            error: None,
        })]) };
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":360,"height":420,"margins":0,"facingPages":false})).unwrap();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Adapt this opened file".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({}), catalog(), vec![skill], session.doc().unwrap().clone(), session.prefs.clone());
    assert!(matches!(result, Ok(Product::Plan(_))), "{}", result.err().unwrap_or_default());
    assert_eq!(ledger.summary().requests, 1);
}

#[test]
fn current_document_images_follow_actual_frames_and_keep_profile_style() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut profile = shared_source_profile_fixture();
    profile.name = "CURRENT_IMAGE_STYLE_SENTINEL".into();
    let skill = SkillSnapshot {
        skill_id: "current-image-style".into(),
        revision: "pin".into(),
        instructions: "Style guidance".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::new(),
        design_profile: Some(profile),
    };
    struct CurrentImageProvider;
    impl Provider for CurrentImageProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(value["document"]["pageWidth"].as_f64(), Some(360.0));
            assert_eq!(value["document"]["pageHeight"].as_f64(), Some(420.0));
            assert!(request.system.contains("CURRENT DOCUMENT GEOMETRY"));
            Ok(GenerationResponse { text:json!({"title":"Current file image","commands":[{"command":"frame.create","params":{"spread":0,"rect":[60,180,160,280],"content":"graphic"}}],
                    "images":[{"prompt":"A coherent original illustration","page":0,"rect":[60,180,160,280],"label":"atlas art"}]}).to_string(),
                    usage:Usage {input_tokens:Some(10),output_tokens:Some(10),..Default::default()},error:None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, request: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            assert_eq!(request.size.as_deref(), Some("1:1"), "Use this square frame, not the profile's unused 4:1 atlas");
            assert!(request.prompt.contains("CURRENT_IMAGE_STYLE_SENTINEL"));
            let pixels = image::RgbaImage::from_pixel(400, 400, image::Rgba([20, 80, 100, 255]));
            let mut bytes = std::io::Cursor::new(vec![]);
            pixels.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "current.png".into() }],
                usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":360,"height":420,"margins":0,"facingPages":false})).unwrap();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Illustrate the actual current frame".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &CurrentImageProvider,
    }
    .production(request, json!({}), catalog(), vec![skill], session.doc().unwrap().clone(), session.prefs.clone())
    .unwrap();
    let Product::Plan(plan) = result else { panic!("Expected current-document plan") };
    assert_eq!(
        plan.commands.iter().filter(|action| action.command == "frame.create").count(),
        1,
        "Do not add profile atlas views to the current file"
    );
    assert_eq!(plan.commands.iter().filter(|action| action.command == "file.place").count(), 1);
    assert_eq!(ledger.summary().requests, 2);
}

#[test]
fn current_document_geometry_changes_are_rejected_before_image_purchase() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct GeometryProvider {
        setup: Value,
        illustrated: bool,
        image_calls: std::cell::Cell<u32>,
    }
    impl Provider for GeometryProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let images = if self.illustrated {
                json!([{"prompt":"One original illustration","page":0,"rect":[54,54,154,154],"label":"scene"}])
            } else {
                json!([])
            };
            Ok(GenerationResponse {text:json!({"title":"Keep opened geometry","commands":[
                    {"command":"layout.documentSetup","params":self.setup},
                    {"command":"frame.create","params":{"spread":0,"rect":[54,54,154,154],"content":if self.illustrated {"graphic"} else {"unassigned"}}}
                ],"images":images}).to_string(),usage:Usage {input_tokens:Some(10),output_tokens:Some(10),..Default::default()},error:None})
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.image_calls.set(self.image_calls.get().saturating_add(1));
            let pixels = image::RgbaImage::from_pixel(400, 400, image::Rgba([20, 80, 100, 255]));
            let mut bytes = std::io::Cursor::new(vec![]);
            pixels.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "scene.png".into() }],
                usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    for (setup, geometry_changed) in [
        (json!({"width":595,"height":841}), true),
        (json!({"margins":36}), true),
        (json!({"facingPages":true}), true),
        (json!({"pages":3}), true),
        (json!({"width":360,"height":480,"margins":18,"pages":2,"facingPages":false}), false),
        (json!({"intent":"web"}), false),
    ] {
        for illustrated in [true, false] {
            let mut session = Session::new();
            session.execute("file.new", &json!({"width":360,"height":480,"margins":18,"pages":2,"facingPages":false})).unwrap();
            let original = session.doc().unwrap().doc.clone();
            let provider = GeometryProvider { setup: setup.clone(), illustrated, image_calls: std::cell::Cell::new(0) };
            let settings = AiSettings {
                output_mode: if illustrated { OutputMode::NativeWithImages } else { OutputMode::NativeOnly },
                image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
                budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
                ..Default::default()
            };
            let request = TaskRequest {
                prompt: "Preserve the opened pages and illustrate their existing layout".into(),
                settings: settings.clone(),
                skill_ids: vec![],
                auto_select_skills: false,
                target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
                export_directory: None,
            };
            let mut ledger = UsageLedger::new(settings.budget).unwrap();
            let (sender, _receiver) = mpsc::channel();
            let cancel = AtomicBool::new(false);
            let result = Runner {
                cache_root: None,
                print: None,
                planning_only: false,
                ledger: &mut ledger,
                keys: &[],
                cancel: &cancel,
                sender: &sender,
                provider: &provider,
            }
            .production(
                request,
                json!({}),
                session.execute("production.catalog", &json!({})).unwrap(),
                vec![],
                session.doc().unwrap().clone(),
                session.prefs.clone(),
            );
            if geometry_changed {
                let error = result.err().expect("A provider cannot resize or repaginate an opened document");
                assert!(error.contains("Opened document geometry"), "{setup}: {error}");
                assert_eq!(provider.image_calls.get(), 0, "Geometry must be checked before purchasing an image");
                assert_eq!(ledger.summary().requests, 1);
            } else {
                assert!(
                    matches!(result, Ok(Product::Plan(_))),
                    "Unchanged geometry and non-geometric setup remain supported: {}",
                    result.err().unwrap_or_default()
                );
                assert_eq!(provider.image_calls.get(), u32::from(illustrated));
                assert_eq!(ledger.summary().requests, 1 + u32::from(illustrated));
            }
            assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
        }
    }
}

#[test]
fn generic_review_beyond_one_plan_is_partitioned_before_provider_calls() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct GenericScopeProvider(RefCell<Vec<BatchScope>>);
    impl Provider for GenericScopeProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            let scope: BatchScope = serde_json::from_value(value["document"]["batch_scope"].clone()).unwrap();
            assert!(scope.count <= 32, "Generic production must partition the review before buying its first plan");
            assert!(u64::from(scope.count) <= request.json_schema["properties"]["commands"]["maxItems"].as_u64().unwrap());
            assert_eq!(value["document"]["review_scope"]["start"], 1);
            assert_eq!(value["document"]["review_scope"]["count"], 513);
            assert!(request.system.contains("GENERIC_PIN_CONTEXT"));
            assert!(request.system.contains("generic-reference-pin"));
            if scope.start > 1 {
                assert_eq!(value["document"]["objects"][0]["spread"], scope.start - 1, "Continue from actual preceding internal content");
            }
            self.0.borrow_mut().push(scope);
            let commands = (scope.start..scope.start + scope.count)
                .map(|page| json!({"command":"frame.create","params":{"spread":page,"rect":[8,8,16,16],"content":"unassigned"}}))
                .collect::<Vec<_>>();
            Ok(GenerationResponse {
                text: json!({"title":"Generic review part","commands":commands,"images":[]}).to_string(),
                usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No images in native mock".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No model discovery".into())
        }
    }
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":64,"height":64,"margins":0,"pages":515,"facingPages":false})).unwrap();
    session.execute("frame.create", &json!({"spread":0,"rect":[8,8,16,16],"content":"unassigned"})).unwrap();
    let base = session.doc().unwrap().clone();
    let original = base.doc.clone();
    let skill = SkillSnapshot {
        skill_id: "generic-style".into(),
        revision: "generic-reference-pin".into(),
        instructions: "GENERIC_PIN_CONTEXT".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::new(),
        design_profile: None,
    };
    let settings = AiSettings { budget: BudgetSpec { token_limit: 4_000_000, max_requests: 64, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: "Continue every requested page using the same native layout".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::ContinueBook,
        export_directory: None,
    };
    let provider = GenericScopeProvider(RefCell::new(vec![]));
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let scope = BatchScope { start: 1, count: 513 };
    let product = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(request, json!({"batch_scope":scope}), catalog(), vec![skill], base.clone(), session.prefs.clone())
    .unwrap();
    let Product::BatchGroup { plans, proofs } = product else { panic!("Expected one complete generic review group") };
    let scopes = provider.0.borrow();
    assert_eq!(scopes.len(), 513_u32.div_ceil(32) as usize);
    assert_eq!(scopes.first().unwrap().start, 1);
    assert_eq!(scopes.last().unwrap().start + scopes.last().unwrap().count, 514);
    assert!(scopes.windows(2).all(|parts| parts[0].start + parts[0].count == parts[1].start));
    assert_eq!(scopes.iter().map(|part| part.count).sum::<u32>(), 513);
    assert_eq!(plans.len(), scopes.len());
    simulate_group_scoped(&plans, &base, &session.prefs, OutputMode::NativeOnly, &cancel, scope).unwrap();
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".png")).count(), 513);
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".pdf")).count(), 17);
    let doc = designcraft_format::load(&proofs.iter().find(|proof| proof.name.ends_with(".designcraft")).unwrap().bytes).unwrap();
    assert_eq!(doc.spreads[0], base.doc.spreads[0]);
    assert!(doc.spreads.iter().skip(1).take(513).all(|spread| !spread.items.is_empty()));
    assert!(doc.spreads[514].items.is_empty(), "The next review must stay untouched");
    assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    assert_eq!(ledger.summary().requests, scopes.len() as u32);
}

#[test]
fn generic_internal_failure_or_cancellation_keeps_live_review_unpublished() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct AbortProvider {
        cancel_second: bool,
        calls: std::cell::Cell<u32>,
    }
    impl Provider for AbortProvider {
        fn generate(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            request: &GenerationRequest,
            cancel: &AtomicBool,
        ) -> Result<GenerationResponse, String> {
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            let scope: BatchScope = serde_json::from_value(value["document"]["batch_scope"].clone()).unwrap();
            assert!(scope.count <= 32);
            self.calls.set(self.calls.get().saturating_add(1));
            if self.calls.get() == 2 {
                assert_eq!(value["document"]["objects"][0]["spread"], 32);
                if self.cancel_second {
                    cancel.store(true, Ordering::Relaxed);
                } else {
                    return Err("synthetic second-part failure".into());
                }
            }
            let commands = (scope.start..scope.start + scope.count)
                .map(|page| json!({"command":"frame.create","params":{"spread":page,"rect":[8,8,16,16],"content":"unassigned"}}))
                .collect::<Vec<_>>();
            Ok(GenerationResponse {
                text: json!({"title":"Unpublished generic part","commands":commands,"images":[]}).to_string(),
                usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No image request".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    for cancel_second in [false, true] {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":64,"height":64,"margins":0,"pages":35,"facingPages":false})).unwrap();
        let original = session.doc().unwrap().doc.clone();
        let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, max_requests: 4, ..Default::default() }, ..Default::default() };
        let request = TaskRequest {
            prompt: "Create every page in this review".into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 35 },
            export_directory: None,
        };
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let provider = AbortProvider { cancel_second, calls: std::cell::Cell::new(0) };
        let error = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .production(request, json!({"batch_scope":{"start":1,"count":33}}), catalog(), vec![], session.doc().unwrap().clone(), session.prefs.clone())
        .err()
        .unwrap();
        assert!(error.contains(if cancel_second { "cancelled" } else { "synthetic second-part failure" }), "{error}");
        assert_eq!(provider.calls.get(), 2);
        assert_eq!(ledger.summary().requests, 2);
        assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
        assert!(session.doc().unwrap().doc.spreads.iter().all(|spread| spread.items.is_empty()));
    }
}

#[test]
fn template_review_scope_is_split_internally_without_reducing_requested_pages() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let temp = tempfile::tempdir().unwrap();
    let mut registry = SkillRegistry::open(temp.path()).unwrap();
    let record = registry.install_builtin_drafts().unwrap().into_iter().find(|r| r.manifest.design_profile.is_some()).unwrap();
    let mut skill = SkillSnapshot {
        skill_id: record.skill_id.clone(),
        revision: record.revision.clone(),
        instructions: "Exact pinned content guidance".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: registry.review_templates(&record.skill_id, &record.revision).unwrap(),
        design_profile: record.manifest.design_profile.clone(),
    };
    for family in &skill.design_profile.as_ref().unwrap().page_families {
        let actions = skill.templates.get_mut(&family.template).unwrap();
        let frame = actions.iter().position(|action| action.command == "frame.create").unwrap();
        while actions.len() < 200 {
            actions.push(EngineAction {
                command: "object.label".into(),
                params: json!({"ids":[format!("${frame}.id")],"label":"Internal capacity fixture"}),
            });
        }
    }
    assert_eq!(crate::template_plan::capacity(&skill, OutputMode::NativeOnly).unwrap(), 2);
    struct ScopedContentProvider {
        profile: designcraft_ai_types::DesignProfile,
        scopes: RefCell<Vec<BatchScope>>,
    }
    impl Provider for ScopedContentProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            let scope: BatchScope = serde_json::from_value(value["scope"].clone()).unwrap();
            assert_eq!(value["document"]["review_scope"]["count"], 4);
            assert!(scope.count <= 2);
            if scope.start > 0 {
                assert!(!value["document"]["objects"].as_array().unwrap().is_empty(), "Later chunks need actual preceding native content");
            }
            self.scopes.borrow_mut().push(scope);
            let pages = (scope.start..scope.start + scope.count)
                .map(|page| {
                    let family = &self.profile.page_families[page as usize % self.profile.page_families.len()];
                    let text = family.text_slots.iter().map(|slot| (slot.id.clone(), format!("Page {}", page + 1))).collect::<BTreeMap<_, _>>();
                    json!({"page":page,"family":family.id,"text":text,"illustrations":[]})
                })
                .collect::<Vec<_>>();
            Ok(GenerationResponse {
                text: json!({"title":"Review book","pages":pages}).to_string(),
                usage: Usage { input_tokens: Some(10), output_tokens: Some(10), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No image request in native test".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No model discovery".into())
        }
    }
    let provider = ScopedContentProvider { profile: skill.design_profile.clone().unwrap(), scopes: RefCell::new(vec![]) };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":4,"facingPages":false})).unwrap();
    let original = session.doc().unwrap().doc.clone();
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, max_requests: 4, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: "Create four kindergarten pages with English and Arabic مشكول".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 4 },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let result = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(
        request,
        json!({"batch_scope":{"start":0,"count":4}}),
        catalog(),
        vec![skill],
        session.doc().unwrap().clone(),
        session.prefs.clone(),
    );
    let Product::BatchGroup { plans, proofs } = result.unwrap() else { panic!("Expected one complete review group") };
    assert_eq!(plans.len(), 2);
    assert!(plans.iter().all(|plan| plan.commands.len() <= MAX_ACTIONS));
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".png")).count(), 4);
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".pdf")).count(), 1);
    let native = proofs.iter().find(|proof| proof.name.ends_with(".designcraft")).unwrap();
    let doc = designcraft_format::load(&native.bytes).unwrap();
    assert_eq!(doc.spreads.len(), 4);
    assert!(doc.spreads.iter().all(|spread| !spread.items.is_empty()));
    let scope = BatchScope { start: 0, count: 4 };
    simulate_group_scoped(&plans, session.doc().unwrap(), &session.prefs, OutputMode::NativeOnly, &cancel, scope).unwrap();
    assert!(simulate_group_scoped(&plans[..1], session.doc().unwrap(), &session.prefs, OutputMode::NativeOnly, &cancel, scope).is_err());
    let reversed = plans.iter().rev().cloned().collect::<Vec<_>>();
    assert!(simulate_group_scoped(&reversed, session.doc().unwrap(), &session.prefs, OutputMode::NativeOnly, &cancel, scope).is_err());
    assert_eq!(provider.scopes.borrow().iter().map(|scope| scope.count).sum::<u32>(), 4);
    assert_eq!(provider.scopes.borrow().len(), 2);
    assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    assert_eq!(ledger.summary().requests, 2);
}

#[test]
fn review_proofs_split_pdf_ranges_but_cover_every_review_page() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":100,"height":100,"pages":33,"facingPages":false,"margins":0})).unwrap();
    let base = session.doc().unwrap().clone();
    let plan = ProductionPlan {
        title: "33 page proof".into(),
        images: vec![],
        commands: (0..33)
            .map(|page| EngineAction { command: "frame.create".into(), params: json!({"spread":page,"rect":[10,10,50,50],"content":"unassigned"}) })
            .collect(),
    };
    let proofs =
        batch_proofs(&plan, &base, &session.prefs, OutputMode::NativeOnly, &AtomicBool::new(false), BatchScope { start: 0, count: 33 }).unwrap();
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".pdf")).count(), 2);
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".png")).count(), 33);
    let native = proofs.iter().find(|proof| proof.name.ends_with(".designcraft")).unwrap();
    let doc = designcraft_format::load(&native.bytes).unwrap();
    assert_eq!(doc.spreads.len(), 33);
    assert!(doc.spreads.iter().all(|spread| !spread.items.is_empty()));
}

#[test]
fn shared_skill_reference_bodies_keep_exact_guidance_and_per_skill_pins() {
    let body = "Exact reference guidance العربية ".repeat(600);
    let first = SkillSnapshot {
        skill_id: "first".into(),
        revision: "pin-one".into(),
        instructions: "First rules".into(),
        references: BTreeMap::from([("references/rules.md".into(), body.clone())]),
        reference_hashes: BTreeMap::from([("references/rules.md".into(), "original-file-hash".into())]),
        templates: BTreeMap::new(),
        design_profile: None,
    };
    let mut second = first.clone();
    second.skill_id = "second".into();
    second.revision = "pin-two".into();
    second.instructions = "Distinct second rules".into();
    let source = vec![first, second];
    let text = compact_skill_guidance(&source).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["reference_bodies"].as_object().unwrap().len(), 1);
    assert_eq!(value["reference_bodies"].as_object().unwrap().values().next().unwrap(), &json!(body));
    assert_eq!(value["skills"][0]["revision"], "pin-one");
    assert_eq!(value["skills"][1]["revision"], "pin-two");
    assert_eq!(value["skills"][1]["instructions"], source[1].instructions);
    assert_eq!(value["skills"][0]["reference_hashes"]["references/rules.md"], "original-file-hash");
    assert_eq!(source[0].references["references/rules.md"], body);
    let mut oversized = source;
    oversized[0].instructions = "x".repeat(33 * 1024);
    assert!(compact_skill_guidance(&oversized).is_err());
}

#[test]
fn editorial_reflow_preserves_vocalized_document_copy_unless_removal_is_explicit() {
    let copy = "آدَابُ الْقِرَاءَةِ";
    for context in [
        json!({"title":copy}),
        json!({"objects":[{"text":copy}]}),
        json!({"recentApprovedObjects":[{"text":copy}]}),
        json!({"approvedPageBriefs":[[0,copy]]}),
    ] {
        let mut plan = ProductionPlan {
            title: copy.into(),
            commands: vec![EngineAction { command: "type.insert".into(), params: json!({"text":copy}) }],
            images: vec![],
        };
        prepare_editorial_plan(&mut plan, "Reflow existing text", &context, None).unwrap();
        assert_eq!(plan.title, copy);
        assert_eq!(plan.commands[0].params["text"], copy);
        prepare_editorial_plan(&mut plan, "Reflow existing text without diacritics", &context, None).unwrap();
        assert_eq!(plan.title, "آداب القراءة");
        assert_eq!(plan.commands[0].params["text"], "آداب القراءة");
    }
    assert!(!context_has_vocalized_copy(&json!({"selected_skill_guidance":"نَبَاتٌ","fontFaces":[{"family":"نَبَاتٌ"}]})));
}

#[test]
fn ai10_generated_arabic_is_plain_without_altering_explicitly_supplied_or_requested_marks() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let fonts = designcraft_fonts::FontDb::global();
    let Some(family) = ["Tahoma", "Noto Naskh Arabic", "DejaVu Sans", "Arial"]
        .into_iter()
        .find(|family| fonts.has_family(family) && "نبات".chars().all(|c| fonts.face(family, "Regular").glyph_for(c) != 0))
    else {
        return;
    };
    for (prompt, context, expected) in [
        ("أنشئ نشاط علوم بالعربية دون تشكيل", json!({}), "نبات"),
        ("أنشئ نشاط علوم بالعربية", json!({}), "نبات"),
        ("انسخ العنوان نَبَاتٌ كما هو", json!({}), "نَبَاتٌ"),
        ("أريد التشكيل الكامل لدرس القراءة", json!({}), "نَبَاتٌ"),
        ("Reflow existing text", json!({"objects":[{"text":"نَبَاتٌ"}]}), "نَبَاتٌ"),
        ("Reflow existing text without diacritics", json!({"objects":[{"text":"نَبَاتٌ"}]}), "نبات"),
    ] {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4"})).unwrap();
        let original = session.doc().unwrap().doc.clone();
        let response = json!({"title":"Arabic lesson","commands":[
                {"command":"frame.create","params":{"content":"text","text":"نَبَاتٌ","rect":[54,54,500,180],"caret":false}},
                {"command":"type.char","params":{"attrs":{"fontFamily":family,"fontStyle":"Regular","size":18}}},
                {"command":"type.para","params":{"attrs":{"direction":"rightToLeft","align":"right"}}}
            ],"images":[]});
        let provider = Mock {
            responses: RefCell::new(vec![Ok(GenerationResponse {
                text: response.to_string(),
                usage: Usage { input_tokens: Some(20), output_tokens: Some(30), ..Default::default() },
                error: None,
            })]),
        };
        let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
        let request = TaskRequest {
            prompt: prompt.into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
            export_directory: None,
        };
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let mut runner = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        };
        let Product::Plan(plan) = runner
            .production(
                request,
                context,
                session.execute("production.catalog", &json!({})).unwrap(),
                vec![],
                session.doc().unwrap().clone(),
                session.prefs.clone(),
            )
            .unwrap()
        else {
            panic!("Expected native plan")
        };
        assert_eq!(plan.commands[0].params["text"], expected, "{prompt}");
        assert_eq!(ledger.summary().requests, 1, "Plain Arabic is a local policy, not another model call");
        assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    }
}

#[test]
fn ai10_repeated_body_layout_is_rejected_before_buying_illustrations() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct RepeatedPageProvider {
        response: String,
        image_calls: std::cell::Cell<usize>,
    }
    impl Provider for RepeatedPageProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Ok(GenerationResponse {
                text: self.response.clone(),
                usage: Usage { input_tokens: Some(20), output_tokens: Some(30), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.image_calls.set(self.image_calls.get() + 1);
            Err("This repetitive layout must be stopped before an image request".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }
    let mut commands = Vec::new();
    for spread in 0..2 {
        for rect in [[54, 180, 270, 365], [310, 180, 540, 365], [54, 416, 540, 536], [54, 552, 540, 720]] {
            commands.push(json!({"command":"frame.create","params":{"spread":spread,"rect":rect,"content":"unassigned"}}));
        }
    }
    let provider = RepeatedPageProvider {
        response: json!({"title":"Repeated science","commands":commands,"images":[
            {"page":0,"rect":[54,180,270,365],"prompt":"Plant photo","label":"plant"},
            {"page":1,"rect":[54,180,270,365],"prompt":"Seed photo","label":"seed"}
        ]})
        .to_string(),
        image_calls: std::cell::Cell::new(0),
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":48,"facingPages":false})).unwrap();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "mock-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Create two varied Arabic science pages with different activities".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 48 },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let result = runner.production(
        request,
        json!({"batch_scope":{"start":0,"count":2}}),
        session.execute("production.catalog", &json!({})).unwrap(),
        vec![],
        session.doc().unwrap().clone(),
        session.prefs.clone(),
    );
    let error = result.err().unwrap();
    assert!(error.contains("Repeated body layout"), "{error}");
    assert_eq!(provider.image_calls.get(), 0);
    assert_eq!(ledger.summary().requests, 1);
}

fn design_profile_fixture() -> designcraft_ai_types::DesignProfile {
    let temp = tempfile::tempdir().unwrap();
    let mut registry = SkillRegistry::open(temp.path()).unwrap();
    registry.install_builtin_drafts().unwrap().into_iter().find_map(|r| r.manifest.design_profile).unwrap()
}

fn shared_source_profile_fixture() -> designcraft_ai_types::DesignProfile {
    let mut profile = design_profile_fixture();
    profile.page_families[0].image_slots = vec![
        serde_json::from_value(json!({
            "id":"atlas", "purpose":"atlas art", "rect":[60,180,160,280],
            "crop":[0,0,0.5,1], "source_aspect_ratio":4,
            "views":[{"rect":[200,180,300,280],"crop":[0.5,0,1,1]}]
        }))
        .unwrap(),
    ];
    profile.validate().unwrap();
    profile
}

#[test]
fn gemini_image_ratios_follow_the_selected_model_capabilities() {
    for model in ["gemini-3.1-flash-image", "gemini-3.1-flash-image-preview", "models/gemini-3.1-flash-image-preview-01-01"] {
        for (ratio, expected) in [(0.125, "1:8"), (0.25, "1:4"), (4.0, "4:1"), (8.0, "8:1")] {
            assert_eq!(gemini_image_ratio(model, ratio).0, expected, "{model}");
        }
    }
    for model in ["gemini-2.5-flash-image", "gemini-3-pro-image-preview", "gemini-3.1-pro-image", "unknown-image-model"] {
        assert_eq!(gemini_image_ratio(model, 4.0).0, "21:9", "{model}");
        assert_eq!(gemini_image_ratio(model, 0.25).0, "9:16", "{model}");
        assert_eq!(gemini_image_ratio(model, 1.5).0, "3:2", "{model}");
    }
    let profile = shared_source_profile_fixture();
    let mut settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image-preview".into(), endpoint: None },
        ..Default::default()
    };
    preflight_profile_image_ratios(&profile, &settings).unwrap();
    settings.image_model.model = "gemini-2.5-flash-image".into();
    assert!(preflight_profile_image_ratios(&profile, &settings).unwrap_err().contains("cannot request"));
    settings.output_mode = OutputMode::NativeOnly;
    preflight_profile_image_ratios(&profile, &settings).unwrap();
}

#[test]
fn incompatible_shared_image_ratio_fails_before_text_or_any_image_purchase() {
    let profile = shared_source_profile_fixture();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash-image".into(), endpoint: None },
        ..Default::default()
    };
    let skill = SkillSnapshot {
        skill_id: "ratio-test".into(),
        revision: "pin".into(),
        instructions: "Keep exact source crops".into(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::new(),
        design_profile: Some(profile.clone()),
    };
    let provider = Mock { responses: RefCell::new(vec![]) };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let error = runner
        .production(
            TaskRequest {
                prompt: "Create an illustrated lesson".into(),
                settings: settings.clone(),
                skill_ids: vec!["ratio-test".into()],
                auto_select_skills: false,
                target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 1 },
                export_directory: None,
            },
            json!({}),
            catalog(),
            vec![skill],
            session.doc().unwrap().clone(),
            session.prefs.clone(),
        )
        .err()
        .unwrap();
    assert!(error.contains("cannot request"), "{error}");
    // The second placement is incompatible; even the first must remain unbought.
    let plan = ProductionPlan {
        title: "Whole set preflight".into(),
        commands: vec![],
        images: vec![
            designcraft_ai_types::protocol::ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: Vec::new(),
                repeat_groups: Vec::new(),
                prompt: "Tree".into(),
                page: 0,
                rect: [60.0, 60.0, 160.0, 160.0],
                label: "tree".into(),
            },
            designcraft_ai_types::protocol::ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: Vec::new(),
                repeat_groups: Vec::new(),
                prompt: "Atlas".into(),
                page: 0,
                rect: [60.0, 180.0, 160.0, 280.0],
                label: "atlas art".into(),
            },
        ],
    };
    let error = runner.expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, Some(&profile)).unwrap_err();
    assert!(error.contains("cannot request"), "{error}");
    assert_eq!(ledger.summary().requests, 0);
}

#[test]
fn template_production_uses_one_short_content_request_and_native_commands_without_paid_repairs() {
    template_content_request(0);
}

fn text_fit_style() -> SkillSnapshot {
    let mut profile = design_profile_fixture();
    profile.page_families = serde_json::from_value(json!([{
        "id":"practice", "purpose":"Short classroom rule", "template":"templates/practice.json",
        "text_slots":[{"id":"grammar-rule-1","purpose":"Complete classroom rule","command":0,"max_chars":52}],
        "image_slots":[{"id":"picture","purpose":"Independent scene","rect":[60,180,160,280]}]
    }]))
    .unwrap();
    let mut actions: Vec<EngineAction> = serde_json::from_value(json!([
        {"command":"frame.create","params":{"content":"text","text":"Rule","rect":[60,60,225,122],"caret":false}},
        {"command":"type.char","params":{"attrs":{"size":12,"leading":{"kind":"points","value":17}}}},
        {"command":"type.para","params":{"attrs":{"align":"left","direction":"leftToRight","hyphenate":false}}},
        {"command":"frame.create","params":{"content":"graphic","rect":[60,180,160,280]}}
    ]))
    .unwrap();
    while actions.len() < 260 {
        actions.push(EngineAction { command: "object.label".into(), params: json!({"ids":["$0.id"],"label":"Rule"}) });
    }
    SkillSnapshot {
        skill_id: "text-fit".into(),
        revision: "pin".into(),
        instructions: String::new(),
        references: BTreeMap::new(),
        reference_hashes: BTreeMap::new(),
        templates: BTreeMap::from([("templates/practice.json".into(), actions)]),
        design_profile: Some(profile),
    }
}

#[test]
fn template_text_uses_actual_native_fit_without_cutting_copy_or_shrinking_type() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    for text in ["i".repeat(53), "W".repeat(400)] {
        let skill = text_fit_style();
        let provider = Mock {
            responses: RefCell::new(vec![Ok(GenerationResponse {
                text: json!({"title":"Rule","pages":[{"page":0,"family":"practice","text":{"grammar-rule-1":text},"illustrations":[]}]}).to_string(),
                usage: Usage::default(),
                error: None,
            })]),
        };
        let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
        let request = TaskRequest {
            prompt: "Keep the exact supplied classroom wording".into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 1 },
            export_directory: None,
        };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let original = session.doc().unwrap().doc.clone();
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let result = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .production(request, json!({}), catalog(), vec![skill], session.doc().unwrap().clone(), session.prefs.clone());
        if text.starts_with('i') {
            let Product::Plan(plan) = result.unwrap_or_else(|error| panic!("Fitting copy was rejected: {error}")) else {
                panic!("Expected native plan")
            };
            assert_eq!(plan.commands[0].params["text"], text);
            assert_eq!(plan.commands[1].params["attrs"]["size"], 12);
            assert_eq!(plan.commands[0].params["rect"], json!([60, 60, 225, 122]));
        } else {
            let error = result.err().unwrap();
            assert!(error.contains("overset"), "Real native overset must remain blocked: {error}");
        }
        assert_eq!(ledger.summary().requests, 1, "Fitting is local and never buys a repair");
        assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    }
}

#[test]
fn template_review_validates_all_text_before_buying_any_images() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct InvalidLaterContent {
        later: String,
        calls: std::cell::Cell<u32>,
        images: std::cell::Cell<u32>,
    }
    impl Provider for InvalidLaterContent {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.calls.set(self.calls.get() + 1);
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            let page = value["scope"]["start"].as_u64().unwrap();
            let text = if page == 0 { "Count." } else { self.later.as_str() };
            Ok(GenerationResponse {
                text: json!({"title":"Review","pages":[{"page":page,"family":"practice","text":{"grammar-rule-1":text},
                    "illustrations":[{"slot":"picture","prompt":"A red ball on white"}]}]})
                .to_string(),
                usage: Usage::default(),
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            assert_eq!(self.calls.get(), 2, "Every part's copy must be validated before buying the first image");
            self.images.set(self.images.get() + 1);
            let pixels = image::RgbaImage::from_pixel(400, 400, image::Rgba([20, 80, 100, 255]));
            let mut bytes = std::io::Cursor::new(vec![]);
            pixels.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { bytes: bytes.into_inner(), mime_type: "image/png".into(), filename: "scene.png".into() }],
                usage: Usage::default(),
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    for (later, max_requests, expected_error) in
        [(" ".to_owned(), 8, "empty"), ("W".repeat(400), 8, "overset"), ("Write.".to_owned(), 3, "request limit"), ("Write.".to_owned(), 8, "")]
    {
        let provider = InvalidLaterContent { later, calls: std::cell::Cell::new(0), images: std::cell::Cell::new(0) };
        let settings = AiSettings {
            output_mode: OutputMode::NativeWithImages,
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "mock-image".into(), endpoint: None },
            budget: BudgetSpec { token_limit: 500_000, max_requests, ..Default::default() },
            ..Default::default()
        };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
        let original = session.doc().unwrap().doc.clone();
        let request = TaskRequest {
            prompt: "Make two illustrated pages".into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 2 },
            export_directory: None,
        };
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let result = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .production(
            request,
            json!({"batch_scope":{"start":0,"count":2}}),
            catalog(),
            vec![text_fit_style()],
            session.doc().unwrap().clone(),
            session.prefs.clone(),
        );
        assert_eq!(provider.calls.get(), 2);
        if expected_error.is_empty() {
            let Product::BatchGroup { plans, proofs } = result.unwrap_or_else(|error| panic!("{error}")) else { panic!("Expected complete review") };
            assert_eq!(plans.len(), 2);
            assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".png")).count(), 2);
            assert!(plans.iter().all(|plan| plan.images.is_empty() && plan.commands.iter().any(|action| action.command == "file.place")));
            assert_eq!(provider.images.get(), 2);
            assert_eq!(ledger.summary().requests, 4);
        } else {
            let error = result.err().unwrap();
            assert!(error.contains(expected_error), "{error}");
            assert_eq!(provider.images.get(), 0, "No artwork should be bought for a predictably invalid review group");
            assert_eq!(ledger.summary().requests, 2, "Projection must not charge the live ledger");
        }
        assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    }
}

#[test]
fn continuing_templates_compile_only_the_remaining_scoped_pages() {
    template_content_request(2);
}

fn template_content_request(start_page: u32) {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let temp = tempfile::tempdir().unwrap();
    let mut registry = SkillRegistry::open(temp.path()).unwrap();
    let record = registry.install_builtin_drafts().unwrap().into_iter().find(|r| r.manifest.design_profile.is_some()).unwrap();
    let skill = SkillSnapshot {
        skill_id: record.skill_id.clone(),
        revision: record.revision.clone(),
        instructions: "CONTENT_GUIDANCE_REGRESSION".into(),
        references: BTreeMap::from([("references/content.md".into(), "REFERENCE_GUIDANCE_REGRESSION".into())]),
        reference_hashes: BTreeMap::new(),
        templates: registry.review_templates(&record.skill_id, &record.revision).unwrap(),
        design_profile: record.manifest.design_profile.clone(),
    };
    let profile = skill.design_profile.as_ref().unwrap();
    let pages = profile
        .page_families
        .iter()
        .take(2)
        .enumerate()
        .map(|(page, family)| {
            let text = family.text_slots.iter().map(|slot| (slot.id.clone(), "Test".to_owned())).collect::<BTreeMap<_, _>>();
            json!({"page":page + start_page as usize,"family":family.id,"text":text,"illustrations":[]})
        })
        .collect::<Vec<_>>();
    struct ContentProvider {
        response: String,
        calls: std::cell::Cell<u32>,
    }
    impl Provider for ContentProvider {
        fn generate(
            &self,
            selection: &ModelSelection,
            _: Option<&str>,
            request: &GenerationRequest,
            _: &AtomicBool,
        ) -> Result<GenerationResponse, String> {
            self.calls.set(self.calls.get().saturating_add(1));
            assert_eq!(self.calls.get(), 1, "Template mode must not request paid command repairs");
            assert!(!request.system.contains("NATIVE CATALOG"));
            assert!(request.prompt.contains("CONTENT_GUIDANCE_REGRESSION"));
            assert!(request.prompt.contains("REFERENCE_GUIDANCE_REGRESSION"));
            assert!(request.max_output_tokens <= 8192);
            if selection.provider == ProviderKind::Gemini {
                assert!(request.max_output_tokens > 4096, "Thinking allowance must not truncate the small visible content plan");
            }
            Ok(GenerationResponse {
                text: self.response.clone(),
                usage: Usage { input_tokens: Some(100), output_tokens: Some(64), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("Native-only test cannot request images".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No model discovery in content generation".into())
        }
    }
    let provider = ContentProvider { response: json!({"title":"Short native content","pages":pages}).to_string(), calls: std::cell::Cell::new(0) };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":4,"facingPages":false})).unwrap();
    for page in 0..start_page {
        session.execute("frame.create", &json!({"spread":page,"rect":[54,54,100,100],"content":"unassigned"})).unwrap();
    }
    let base = session.doc().unwrap().clone();
    let original = base.doc.clone();
    let settings = AiSettings {
        allow_paid_repairs: true,
        text_model: if start_page > 0 {
            ModelSelection { provider: ProviderKind::Gemini, model: "mock-thinking-model".into(), endpoint: None }
        } else {
            AiSettings::default().text_model
        },
        budget: BudgetSpec { token_limit: 500_000, repair_attempts: 3, max_requests: 8, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Create the first two kindergarten pages".into(),
        settings: settings.clone(),
        skill_ids: vec![skill.skill_id.clone()],
        auto_select_skills: false,
        target: if start_page == 0 {
            designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 4 }
        } else {
            designcraft_ai_types::protocol::TaskTarget::ContinueBook
        },
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let result = runner
        .production(request, json!({"batch_scope":{"start":start_page,"count":2}}), catalog(), vec![skill], base, session.prefs.clone())
        .unwrap();
    let Product::BatchPlan { plan, proofs } = result else {
        panic!("Expected a native batch proof");
    };
    assert!(plan.commands.len() > 20);
    assert_eq!(proofs.iter().filter(|proof| proof.name.ends_with(".png")).count(), 2);
    assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    assert_eq!(ledger.summary().requests, 1);
}

#[test]
fn generated_style_packages_link_all_native_page_families_and_bound_corrections() {
    let mut profile = design_profile_fixture();
    profile.page_families = vec![designcraft_ai_types::PageFamily {
        id: "practice".into(),
        purpose: "Count then draw".into(),
        template: "templates/practice.json".into(),
        text_slots: vec![],
        image_slots: vec![],
    }];
    let actions = vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,160,160]}) }];
    let draft = SkillDraft {
        skill_id: "fixture-style".into(),
        description: "School layout".into(),
        skill_md: "Native editable practice".into(),
        compatibility_notes: "A4".into(),
        sample_actions: actions.clone(),
        analyses: vec![],
        design_profile: Some(profile.clone()),
        page_templates: vec![PageTemplateDraft { id: "practice".into(), actions }],
    };
    let (files, _) = package_draft(draft, &catalog(), &json!([]), vec![]).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut registry = SkillRegistry::open(temp.path()).unwrap();
    let record = registry.import_files(files, Default::default()).unwrap();
    assert_eq!(record.manifest.design_profile, Some(profile));
    assert_eq!(record.manifest.templates.len(), 2);
    assert!(record.approval.is_none());
    let schema = skill_schema(&catalog()).unwrap();
    assert!(schema["properties"]["design_profile"]["properties"].get("corrections").is_some());
}

#[test]
fn explicit_low_ram_in_spawned_worker_stops_before_ledger_and_http() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicU32::new(0));
    let observed = calls.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    observed.fetch_add(1, Ordering::Relaxed);
                    socket.set_nonblocking(false).unwrap();
                    socket.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                    // A regression sends only to this bounded spy, never a real model.
                    let _ = socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("Low-memory HTTP spy: {error}"),
            }
        }
    });
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    for available in [1024, 4096] {
        // The first fixture fails the reserve floor; the second exceeds 85% used
        // while remaining above the 2048-MiB floor. Both use the real policy.
        let _memory = crate::resources::TestAdmissionGuard::install(designcraft_ai_types::ResourceSnapshot {
            cpu_threads_available: 8,
            total_ram_mb: 32_768,
            available_ram_mb: available,
            ..Default::default()
        });
        let settings = AiSettings {
            text_model: ModelSelection { provider: ProviderKind::Ollama, model: "low-memory-spy".into(), endpoint: Some(endpoint.clone()) },
            budget: BudgetSpec { token_limit: 256_000, max_output_tokens: 512, max_requests: 1, repair_attempts: 0, ..Default::default() },
            ..Default::default()
        };
        let work = Work::Production {
            request: TaskRequest {
                prompt: "Create an editable native frame".into(),
                settings,
                skill_ids: vec![],
                auto_select_skills: false,
                target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
                export_directory: None,
            },
            context: json!({}),
            catalog: designcraft_engine::cmd::production::catalog(),
            skills: vec![],
            base: session.doc().unwrap().clone(),
            prefs: session.prefs.clone(),
        };
        let worker = spawn(work, vec![], None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Low-memory worker exceeded its deadline");
            match worker.receiver.recv_timeout(remaining).unwrap() {
                Message::Progress { .. } | Message::Usage(_) | Message::Diagnostic(_) => {}
                Message::Finished(outcome) | Message::FinishedWithLedger(outcome, _) => break outcome,
            }
        };
        let error = outcome.result.err().unwrap();
        assert!(error.contains("memory protection"), "{error}");
        assert!(error.contains(&format!("{available} MiB available")), "The worker must use its captured synthetic telemetry: {error}");
        assert_eq!(outcome.usage.requests, 0, "Resource rejection must precede admission reservation");
        assert_eq!(outcome.usage.estimated_tokens, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "Resource rejection must precede any HTTP connection");
    }
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn batch_scope_rejects_global_commands_old_pages_literal_ids_and_inherited_selection() {
    let scope = BatchScope { start: 1, count: 1 };
    let mut plan = ProductionPlan {
        title: "Isolated".into(),
        commands: vec![EngineAction { command: "frame.create".into(), params: json!({"spread":1,"rect":[50,50,200,200],"content":"unassigned"}) }],
        images: vec![],
    };
    assert!(validate_batch_plan(&plan, scope).is_ok());
    assert!(validate_batch_plan(&plan, BatchScope { start: 1, count: 2 }).is_err(), "A model cannot mark an unproduced page complete");
    plan.commands[0].params["spread"] = json!(0);
    assert!(validate_batch_plan(&plan, scope).is_err());
    plan.commands[0].params["spread"] = json!(1);
    plan.commands.push(EngineAction { command: "object.label".into(), params: json!({"ids":[123],"label":"prior object"}) });
    assert!(validate_batch_plan(&plan, scope).is_err());
    plan.commands[1].params["ids"] = json!(["$0.id"]);
    assert!(validate_batch_plan(&plan, scope).is_ok());
    plan.commands.push(EngineAction { command: "layout.pages.insert".into(), params: json!({"count":1}) });
    assert!(validate_batch_plan(&plan, scope).is_err());
    plan.commands = vec![EngineAction { command: "type.char".into(), params: json!({"attrs":{"size":12}}) }];
    assert!(validate_batch_plan(&plan, scope).is_err());
}

#[test]
fn batch_result_rejects_shared_story_and_global_changes_without_spread_changes() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
    let created = session
        .execute("frame.create", &json!({"spread":0,"rect":[50,50,300,200],"content":"text","text":"Earlier approved text","caret":false}))
        .unwrap();
    let before = session.doc().unwrap().clone();
    session.execute("frame.create", &json!({"spread":1,"rect":[50,50,200,200],"content":"unassigned"})).unwrap();
    let after = session.doc().unwrap().clone();
    let scope = BatchScope { start: 1, count: 1 };
    assert!(validate_batch_result(&before, &after, scope).is_ok());
    let mut global = after.clone();
    Arc::make_mut(&mut global.doc).settings.page_width += 1.0;
    assert_eq!(global.doc.spreads[0], before.doc.spreads[0]);
    assert!(validate_batch_result(&before, &global, scope).is_err());
    let story_id = created["story"].as_u64().unwrap();
    let mut story_changed = after;
    let story = Arc::make_mut(&mut story_changed.doc).stories.iter_mut().find(|(id, _)| id.0 == story_id).unwrap().1;
    Arc::make_mut(story).text.push('!');
    assert_eq!(story_changed.doc.spreads[0], before.doc.spreads[0]);
    assert!(validate_batch_result(&before, &story_changed, scope).is_err());
}

#[test]
fn spawned_reference_reader_retains_thread_confined_roots() {
    let profile = tempfile::tempdir().unwrap();
    let allowed = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = outside.path().join("reference.md");
    std::fs::write(&source, "private reference").unwrap();
    let roots = designcraft_engine::file_access::AutomationRoots::new(&[allowed.path()], &[allowed.path()]).unwrap().unwrap();
    let work = Work::Skill {
        design_style: false,
        prompt: "Analyze references".into(),
        reference_paths: vec![source.to_string_lossy().into_owned()],
        context: None,
        settings: Default::default(),
        catalog: json!({}),
        registry_root: profile.path().join("skills"),
    };
    let worker = designcraft_engine::file_access::confine(Some(&roots), || spawn(work, vec![], designcraft_engine::file_access::current())).unwrap();
    loop {
        match worker.receiver.recv_timeout(std::time::Duration::from_secs(10)).unwrap() {
            Message::Progress { .. } | Message::Usage(_) | Message::Diagnostic(_) => {}
            Message::Finished(outcome) | Message::FinishedWithLedger(outcome, _) => {
                assert!(outcome.result.err().unwrap().contains("outside the read root"));
                assert_eq!(outcome.usage.requests, 0);
                break;
            }
        }
    }
}

fn catalog() -> Value {
    json!({"commands":[{"command":"frame.create","paramsSchema":{"type":"object"},"example":{"rect":[0,0,100,100]},"description":"verbose help"},{"command":"file.place","paramsSchema":{"type":"object"}},{"command":"object.label","paramsSchema":{"type":"object"}}]})
}
fn plan() -> Value {
    json!({"title":"Test","commands":[{"command":"frame.create","params":{"rect":[36,36,300,200],"content":"unassigned"}}],"images":[]})
}
#[test]
fn native_only_rejects_raster_and_unknown_commands() {
    let mut value = plan();
    assert!(parse_plan(&value.to_string(), OutputMode::NativeOnly, &catalog()).is_ok());
    value["images"] = json!([{"prompt":"tree","page":0,"rect":[0,0,100,100],"label":"tree"}]);
    assert!(parse_plan(&value.to_string(), OutputMode::NativeOnly, &catalog()).is_err());
    assert!(parse_plan(&value.to_string(), OutputMode::NativeWithImages, &catalog()).is_ok());
    value["commands"][0]["command"] = json!("file.place");
    assert!(parse_plan(&value.to_string(), OutputMode::NativeWithImages, &catalog()).is_err());
    value["commands"][0]["command"] = json!("file.exportPdf");
    assert!(parse_plan(&value.to_string(), OutputMode::NativeOnly, &catalog()).is_err());
}
#[test]
fn schema_and_parser_bound_images_and_rectangles() {
    let schema = plan_schema(&catalog(), OutputMode::NativeOnly).unwrap();
    assert_eq!(schema["properties"]["images"]["maxItems"], 0);
    let illustrated = plan_schema(&catalog(), OutputMode::NativeWithImages).unwrap();
    assert_eq!(illustrated["properties"]["images"]["maxItems"], 8);
    let placement = json!({"prompt":"tree","page":0,"rect":[54,220,200,366],"label":"tree"});
    let mut multi = plan();
    multi["images"] = json!(vec![placement.clone(); 8]);
    assert!(parse_plan(&multi.to_string(), OutputMode::NativeWithImages, &catalog()).is_ok());
    multi["images"] = json!(vec![placement; 9]);
    assert!(parse_plan(&multi.to_string(), OutputMode::NativeWithImages, &catalog()).is_err());
    let mut value = plan();
    value["images"] = json!([{"prompt":"tree","page":0,"rect":[100,0,0,100],"label":"tree"}]);
    assert!(parse_plan(&value.to_string(), OutputMode::NativeWithImages, &catalog()).is_err());
    assert!(parse_plan("```json\n{}\n```", OutputMode::NativeOnly, &catalog()).is_err());
    value = plan();
    value["shell"] = json!("anything");
    assert!(parse_plan(&value.to_string(), OutputMode::NativeOnly, &catalog()).is_err());
}
#[test]
fn cached_references_are_text_only_and_duplicates_are_not_resent() {
    let digest = "a".repeat(64);
    let material = || ReferenceMaterial {
        digest: digest.clone(),
        description: "sample".into(),
        text: "source text".into(),
        images: vec![ImageInput { mime_type: "image/jpeg".into(), bytes: vec![1, 2, 3] }],
    };
    let (context, images, cached) = reference_prompt(vec![material(), material()], |digest| {
        Some(CachedAnalysis { reference_sha256: digest.into(), analyzer: ANALYZER.into(), text: "cached style".into() })
    })
    .unwrap();
    assert!(images.is_empty());
    assert_eq!(cached.len(), 1);
    assert_eq!(context.as_array().unwrap().len(), 1);
    assert_eq!(context[0]["cached_analysis"], "cached style");
    assert!(!context.to_string().contains("source text"));
}
#[test]
fn generated_package_imports_as_draft_without_external_permissions() {
    let draft = SkillDraft {
        design_profile: None,
        page_templates: Vec::new(),
        skill_id: "test-layout".into(),
        description: "Description\nallowed-tools: shell".into(),
        skill_md: "Use exact native margins.".into(),
        compatibility_notes: "A4".into(),
        sample_actions: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[0,0,100,100]}) }],
        analyses: vec![],
    };
    let (files, analyses) = package_draft(draft, &catalog(), &json!([]), vec![]).unwrap();
    assert!(analyses.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let mut registry = SkillRegistry::open(dir.path()).unwrap();
    let revision = registry.import_files(files, Default::default()).unwrap();
    assert_eq!(revision.status, designcraft_ai_skills::RevisionStatus::Draft);
    assert!(revision.approval.is_none());
    assert!(revision.manifest.external_tools.is_empty());
    assert_eq!(revision.manifest.required_commands, vec!["frame.create"]);
}
struct Mock {
    responses: RefCell<Vec<Result<GenerationResponse, String>>>,
}
#[test]
fn image_command_capacity_is_checked_before_any_image_charge() {
    let provider = Mock { responses: RefCell::new(vec![]) };
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let plan = ProductionPlan {
        title: "Capacity test".into(),
        commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,100,100]}) }; 510],
        images: vec![designcraft_ai_types::protocol::ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: Vec::new(),
            repeat_groups: Vec::new(),
            prompt: "Tree".into(),
            page: 0,
            rect: [60.0, 60.0, 160.0, 160.0],
            label: "tree".into(),
        }],
    };
    let error = runner
        .expand_images(plan, &AiSettings { output_mode: OutputMode::NativeWithImages, ..Default::default() }, &mut BTreeMap::new(), &mut 0, None)
        .unwrap_err();
    assert!(error.contains("native action limit"));
    assert_eq!(ledger.summary().requests, 0);
}
#[test]
fn complete_image_set_is_admitted_before_purchasing_its_first_image() {
    let provider = Mock { responses: RefCell::new(vec![]) };
    let budget = BudgetSpec { max_requests: 1, ..Default::default() };
    let mut ledger = UsageLedger::new(budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let image = designcraft_ai_types::protocol::ImagePlacement {
        family_id: None,
        slot_id: None,
        repeat_rects: Vec::new(),
        repeat_groups: Vec::new(),
        prompt: "Tree".into(),
        page: 0,
        rect: [60.0, 60.0, 160.0, 160.0],
        label: "tree".into(),
    };
    let plan = ProductionPlan { title: "Whole set admission".into(), commands: vec![], images: vec![image.clone(), image] };
    let error = runner
        .expand_images(plan, &AiSettings { output_mode: OutputMode::NativeWithImages, ..Default::default() }, &mut BTreeMap::new(), &mut 0, None)
        .unwrap_err();
    assert!(error.contains("limit"));
    assert_eq!(ledger.summary().requests, 0, "No partial image purchase on a predictably impossible set");
}
#[test]
fn failed_raw_plan_does_not_repeat_a_provider_call_without_explicit_opt_in() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let rejected_directory = tempfile::tempdir().unwrap();
    let mut invalid = plan();
    invalid["commands"][0]["params"]["rect"] = json!([100, 60, 40, 80]);
    let provider = Mock {
        responses: RefCell::new(vec![Ok(GenerationResponse {
            text: invalid.to_string(),
            usage: Usage { input_tokens: Some(100), output_tokens: Some(20), ..Default::default() },
            error: None,
        })]),
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, repair_attempts: 3, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: "Create an editable frame".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    assert!(
        runner
            .production(request, json!({"_production_cache_root":rejected_directory.path()}), catalog(), vec![], base, session.prefs.clone())
            .is_err()
    );
    assert_eq!(ledger.summary().requests, 1);
    let diagnostic = receiver
        .try_iter()
        .find_map(|message| match message {
            Message::Diagnostic(value) if value["kind"] == "nativePlanningValidation" => Some(value),
            _ => None,
        })
        .expect("Rejected plans must leave bounded local diagnostic evidence");
    assert_eq!(diagnostic["attempt"], 1);
    assert_eq!(diagnostic["rejected_plan_included"], true);
    assert_eq!(serde_json::from_str::<Value>(diagnostic["rejected_plan"].as_str().unwrap()).unwrap(), invalid);
    let evidence = diagnostic["rejected_response_path"]
        .as_str()
        .expect("Keep exact private failure evidence for offline repair, without paying to reproduce it");
    assert_eq!(std::fs::read_to_string(evidence).unwrap(), invalid.to_string());
    assert_eq!(std::fs::read_dir(rejected_directory.path()).unwrap().count(), 1, "Rejected responses must never be stored as validated plans");
    assert!(evidence.ends_with("last-rejected-response.txt"));
}
impl Provider for Mock {
    fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
        self.responses.borrow_mut().pop().unwrap()
    }
    fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
        Err("mock image failure".into())
    }
    fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
        Ok(vec!["mock".into()])
    }
}
#[test]
fn provider_failures_and_missing_usage_are_accounted_without_network() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut ledger = UsageLedger::new(BudgetSpec { max_requests: 2, ..Default::default() }).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = Mock {
        responses: RefCell::new(vec![
            Ok(GenerationResponse { text: "{}".into(), usage: Usage::default(), error: None }),
            Err("synthetic failure".into()),
        ]),
    };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let selection = AiSettings::default().text_model;
    let request = GenerationRequest {
        system: "native".into(),
        prompt: "test".into(),
        json_schema: json!({"type":"object"}),
        max_output_tokens: 8192,
        images: vec![],
        resources: Default::default(),
    };
    assert!(runner.generate(&selection, &request).is_err());
    assert!(runner.generate(&selection, &request).is_ok());
    assert!(runner.generate(&selection, &request).is_err());
    assert_eq!(ledger.summary().requests, 2);
    assert!(ledger.summary().has_unreported_usage);
    assert!(ledger.summary().estimated_tokens > 0);
}
#[test]
fn cancellation_prevents_provider_admission() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _) = mpsc::channel();
    let cancel = AtomicBool::new(true);
    let provider = Mock { responses: RefCell::new(vec![]) };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let request = GenerationRequest {
        system: String::new(),
        prompt: String::new(),
        json_schema: json!({}),
        max_output_tokens: 1,
        images: vec![],
        resources: Default::default(),
    };
    assert!(runner.generate(&AiSettings::default().text_model, &request).is_err());
    assert_eq!(ledger.summary().requests, 0);
}
#[test]
fn compact_catalog_drops_verbose_help_and_repairs_preserve_unicode() {
    assert!(!compact_catalog(&catalog()).unwrap().to_string().contains("verbose help"));
    let repaired = repair_prompt("نص عربي", &plan().to_string(), "invalid parameter").unwrap();
    assert!(repaired.contains("نص عربي"));
    assert!(repaired.contains("invalid parameter"));
    assert!(validate_skill_id("con").is_err());
    assert!(validate_skill_id("school-print").is_ok());
}

#[test]
fn continuation_context_exposes_resolved_safe_rect_and_page_side() {
    let mut session = Session::new();
    session
        .execute(
            "file.new",
            &json!({"preset":"A4","pages":3,"facingPages":true,
            "margins":{"top":42.52,"bottom":42.52,"inside":51.02,"outside":42.52}}),
        )
        .unwrap();
    let state = session.doc().unwrap();
    let mut context = json!({});
    opened_document_geometry(&mut context, state).unwrap();
    assert_eq!(context["rightToLeftBinding"], json!(state.doc.settings.right_to_left_binding));
    for (record, (_, _, page)) in context["pageGeometry"].as_array().unwrap().iter().zip(state.doc.pages()) {
        let safe = page.margin_rect();
        assert_eq!(record["safeRect"], json!([safe.x0, safe.y0, safe.x1, safe.y1]));
        assert_eq!(record["side"], serde_json::to_value(page.side).unwrap());
    }
}

#[test]
fn production_reference_contracts_reject_null_and_non_story_sources() {
    let native_catalog = designcraft_engine::cmd::production::catalog();
    let missing: Vec<_> = native_catalog["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["command"].as_str())
        .filter(|cmd| native_result_contract(cmd).is_none())
        .collect();
    assert!(missing.is_empty(), "missing native_result_contract for: {missing:?}");
    let compact = compact_catalog(&native_catalog).unwrap();
    let commands = compact["commands"].as_array().unwrap();
    assert_eq!(commands.iter().find(|c| c["command"] == "type.char").unwrap()["result"], json!(null));
    assert_eq!(commands.iter().find(|c| c["command"] == "frame.create").unwrap()["result"]["id"], json!("ItemId"));
    let actions = vec![
        EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,300,100],"content":"text","text":"Header"}) },
        EngineAction { command: "type.char".into(), params: json!({"attrs":{"size":12}}) },
        EngineAction { command: "object.label".into(), params: json!({"ids":["$1.id"],"label":"wrong"}) },
    ];
    let error = validate_actions(&actions, &native_catalog, OutputMode::NativeOnly).unwrap_err();
    assert!(error.contains("type.char") && error.contains("id"), "{error}");
    let mut valid = actions.clone();
    valid[2].params["ids"] = json!(["$0.id"]);
    assert!(validate_actions(&valid, &native_catalog, OutputMode::NativeOnly).is_ok());
    let mut wrong_story =
        vec![actions[0].clone(), EngineAction { command: "story.setText".into(), params: json!({"story":"$0.story","text":"Body"}) }];
    wrong_story[0].params["content"] = json!("graphic");
    assert!(validate_actions(&wrong_story, &native_catalog, OutputMode::NativeOnly).is_err());
    wrong_story[0].params["content"] = json!("text");
    wrong_story[1].params["story"] = json!("$0.id");
    assert!(validate_actions(&wrong_story, &native_catalog, OutputMode::NativeOnly).is_err());
}

#[test]
fn production_creation_contracts_match_actual_native_results() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let commands: Vec<EngineAction> = serde_json::from_value(json!([
        {"command":"frame.create","params":{"spread":0,"rect":[60,60,300,180],"content":"text","text":"","caret":false}},
        {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":0}},
        {"command":"table.insert","params":{"rows":1,"cols":1,"width":100}},
        {"command":"frame.create","params":{"spread":0,"rect":[60,200,100,240],"content":"graphic"}},
        {"command":"frame.create","params":{"spread":0,"rect":[120,200,160,240],"content":"graphic"}},
        {"command":"object.group","params":{"ids":["$3.id","$4.id"]}},
        {"command":"path.create","params":{"spread":0,"anchors":[{"p":[60,300]},{"p":[100,340]}]}},
        {"command":"line.create","params":{"spread":0,"a":[60,400],"b":[100,400]}}
    ]))
    .unwrap();
    validate_actions(&commands, &designcraft_engine::cmd::production::catalog(), OutputMode::NativeOnly).unwrap();
    let outcome = session.execute("production.apply", &json!({"commands":commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
    for (command, result) in commands.iter().zip(outcome["results"].as_array().unwrap()) {
        let contract = native_result_shape(command);
        if let Some(fields) = contract.as_object() {
            for field in fields.keys() {
                assert!(result.get(field).is_some(), "{} omitted promised result {field}: {result}", command.command);
            }
        } else {
            assert!(result.is_null(), "{} must have a null result", command.command);
        }
    }
}

#[test]
fn late_validation_repairs_use_authored_response_without_host_font_actions() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct CaptureRepair {
        authored: String,
        requests: RefCell<Vec<String>>,
    }
    impl Provider for CaptureRepair {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.requests.borrow_mut().push(request.prompt.clone());
            if self.requests.borrow().len() == 1 {
                Ok(GenerationResponse {
                    text: self.authored.clone(),
                    usage: Usage { input_tokens: Some(100), output_tokens: Some(100), ..Default::default() },
                    error: None,
                })
            } else {
                Err("Synthetic repair captured".into())
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No images authorized".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let base = session.doc().unwrap().clone();
    let authored = json!({"title":"Preserve authored repair input","images":[],"commands":[
        {"command":"layout.documentSetup","params":{"width":610}},
        {"command":"frame.create","params":{"spread":0,"rect":[60,60,300,110],"content":"text","text":"Keep these original words","caret":false}}
    ]})
    .to_string();
    let provider = CaptureRepair { authored: authored.clone(), requests: RefCell::new(vec![]) };
    let settings = AiSettings {
        allow_paid_repairs: true,
        budget: BudgetSpec { token_limit: 500_000, max_requests: 2, repair_attempts: 1, ..Default::default() },
        ..Default::default()
    };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let request = TaskRequest {
        prompt: "**English Primary Font:** Source Sans 3\nKeep the actual opened document geometry.".into(),
        settings,
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    assert!(
        runner.production(request, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone()).is_err()
    );
    let requests = provider.requests.borrow();
    assert_eq!(requests.len(), 2, "The synthetic geometry failure must reach its repair");
    let repair: Value = serde_json::from_str(&requests[1]).unwrap();
    assert_eq!(repair["rejected_plan"].as_str().unwrap(), authored, "Host-expanded actions must never be echoed as provider-authored source");
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn command_validation_repairs_on_private_document_with_one_ledger() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let base = session.doc().unwrap().clone();
    let original = base.doc.clone();
    let catalog = session.execute("production.catalog", &json!({})).unwrap();
    let mut invalid = plan();
    invalid["commands"][0]["params"]["rect"] = json!([100, 36, 36, 200]);
    let response = |value: Value| {
        Ok(GenerationResponse {
            text: value.to_string(),
            usage: Usage { input_tokens: Some(100), output_tokens: Some(100), ..Default::default() },
            error: None,
        })
    };
    let provider = Mock { responses: RefCell::new(vec![response(plan()), response(invalid)]) };
    let mut ledger = UsageLedger::new(BudgetSpec { token_limit: 500_000, ..Default::default() }).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let request = TaskRequest {
        prompt: "Create an editable blank rectangle".into(),
        settings: AiSettings { allow_paid_repairs: true, budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() },
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let product = runner.production(request, json!({}), catalog, vec![], base, session.prefs.clone()).unwrap();
    assert!(matches!(product, Product::Plan(_)));
    assert_eq!(ledger.summary().requests, 2);
    assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
    assert_eq!(session.doc().unwrap().doc.spreads[0].items.len(), 0);
    assert!(provider.responses.borrow().is_empty());
}

#[test]
fn overset_layout_is_rejected_before_spending_on_images() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let base = session.doc().unwrap().clone();
    let original = base.doc.clone();
    let catalog = session.execute("production.catalog", &json!({})).unwrap();
    let invalid = json!({
        "title":"Overset proof", "commands":[
            {"command":"frame.create","params":{"rect":[54,54,70,60],"content":"text","text":"Text must remain fully visible","caret":false}},
            {"command":"type.char","params":{"attrs":{"fontFamily":"Source Sans 3","size":24}}}
        ],
        "images":[{"prompt":"A realistic plant","page":0,"rect":[54,220,300,466],"label":"plant"}]
    });
    let provider = Mock {
        responses: RefCell::new(vec![Ok(GenerationResponse {
            text: invalid.to_string(),
            usage: Usage { input_tokens: Some(100), output_tokens: Some(100), ..Default::default() },
            error: None,
        })]),
    };
    let budget = BudgetSpec { token_limit: 500_000, repair_attempts: 0, ..Default::default() };
    let mut ledger = UsageLedger::new(budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let request = TaskRequest {
        prompt: "Create an editable plant lesson".into(),
        settings: AiSettings {
            output_mode: OutputMode::NativeWithImages,
            image_model: ModelSelection { provider: ProviderKind::Gemini, model: "fixture-image".into(), endpoint: None },
            budget,
            ..Default::default()
        },
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let error = runner.production(request, json!({}), catalog, vec![], base, session.prefs.clone()).err().unwrap();
    assert!(error.contains("overset"), "{error}");
    assert_eq!(ledger.summary().requests, 1, "invalid typography must not launch an image request");
    assert!(Arc::ptr_eq(&original, &session.doc().unwrap().doc));
}

#[test]
fn failed_image_job_returns_no_partial_plan_and_records_request() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = Mock { responses: RefCell::new(vec![]) };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let mut value = plan();
    value["images"] = json!([{"prompt":"A tree illustration","page":0,"rect":[36,200,200,400],"label":"tree"}]);
    let plan = parse_plan(&value.to_string(), OutputMode::NativeWithImages, &catalog()).unwrap();
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Ollama, model: "mock".into(), endpoint: None },
        ..Default::default()
    };
    assert!(runner.expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, None).is_err());
    assert_eq!(ledger.summary().requests, 1);
    assert!(ledger.summary().has_unreported_usage);
}

// Existing CC0 Compact ICC Profiles fixture, attributed and pinned in ASSETS.md.
const TEST_SRGB_ICC: &[u8] = include_bytes!("../../../../assets/icc/sRGB-v2-magic.icc");

fn webp_fixture(profile: Option<&[u8]>) -> (providers::GeneratedImage, image::RgbaImage) {
    use image::ImageEncoder;
    let pixels = image::RgbaImage::from_fn(3, 2, |x, y| image::Rgba([20 + x as u8, 70 + y as u8, 130, 128]));
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
    if let Some(profile) = profile {
        encoder.set_icc_profile(profile.to_vec()).unwrap();
    }
    encoder.encode(pixels.as_raw(), 3, 2, image::ExtendedColorType::Rgba8).unwrap();
    (providers::GeneratedImage { bytes, filename: "comfy-output.webp".into(), mime_type: "image/webp".into() }, pixels)
}

#[test]
fn real_comfy_webp_is_normalized_cached_and_placed_as_native_graphic() {
    use image::ImageDecoder;
    struct ImageMock(RefCell<Option<providers::GeneratedImage>>);
    impl Provider for ImageMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("Text generation is not used".into())
        }
        fn images(&self, selection: &ModelSelection, _: Option<&str>, request: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            assert_eq!(selection.provider, ProviderKind::ComfyUi);
            assert!(request.comfy.is_some());
            assert!(request.prompt.contains("PINNED DESIGN STYLE"));
            assert!(request.prompt.contains("consistent proportions"));
            Ok(ImageResponse {
                images: vec![self.0.borrow_mut().take().ok_or("Image requested twice despite cache")?],
                usage: Usage { input_tokens: Some(10), output_tokens: Some(20), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("Model listing is not used".into())
        }
    }
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let (webp, pixels) = webp_fixture(Some(TEST_SRGB_ICC));
    let original_profile = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&webp.bytes)).unwrap().icc_profile().unwrap().unwrap();
    assert_eq!(original_profile, TEST_SRGB_ICC);
    let provider = ImageMock(RefCell::new(Some(webp)));
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::ComfyUi, model: "mock".into(), endpoint: None },
        comfy_workflow: Some(json!({"1":{"class_type":"SaveImage","inputs":{"width":3,"height":2,"batch_size":1}}})),
        ..Default::default()
    };
    let mut value = plan();
    value["images"] = json!([{"prompt":"A tree illustration","page":0,"rect":[120,220,270,360],"label":"tree"}]);
    let plan = parse_plan(&value.to_string(), OutputMode::NativeWithImages, &catalog()).unwrap();
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let mut cache = BTreeMap::new();
    let mut bytes_used = 0;
    let profile = design_profile_fixture();
    let expanded = runner.expand_images(plan.clone(), &settings, &mut cache, &mut bytes_used, Some(&profile)).unwrap();
    assert!(expanded.images.is_empty());
    let cached = cache.values().next().unwrap();
    assert_eq!(cached.mime_type, "image/png");
    assert_eq!(cached.filename, "generated.png");
    assert_eq!(bytes_used, cached.bytes.len(), "Count the placed PNG rather than the compressed WebP");
    assert_eq!(image::load_from_memory(&cached.bytes).unwrap().to_rgba8(), pixels);
    let converted_profile = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&cached.bytes)).unwrap().icc_profile().unwrap().unwrap();
    assert_eq!(converted_profile, original_profile, "Preserve the actual embedded ICC bytes through WebP to PNG conversion");
    let retried = runner.expand_images(plan, &settings, &mut cache, &mut bytes_used, Some(&profile)).unwrap();
    assert_eq!(retried.commands, expanded.commands);
    let mut layered = ProductionPlan {
        title: "Native speech stays above generated art".into(),
        commands: vec![
            EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[120,220,270,360],"content":"graphic"}) },
            EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":0,"rect":[130,230,260,260],"content":"text","text":"Editable speech"}),
            },
        ],
        images: vec![designcraft_ai_types::protocol::ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: Vec::new(),
            repeat_groups: Vec::new(),
            prompt: "A tree illustration".into(),
            page: 0,
            rect: [120.0, 220.0, 270.0, 360.0],
            label: "tree".into(),
        }],
    };
    let source = layered.commands.clone();
    layered = runner.expand_images(layered, &settings, &mut cache, &mut bytes_used, Some(&profile)).unwrap();
    assert_eq!(
        layered.commands.iter().find(|a| a.command == "file.place").unwrap().params["frame"],
        "$0.id",
        "Use the authored frame instead of painting over native text"
    );
    assert_eq!(layered.commands.len(), source.len() + 2);
    assert_eq!(&layered.commands[..source.len()], &source);
    assert_eq!(ledger.summary().requests, 1, "Reuse normalized images on repair without another provider call");
    assert_eq!(ledger.summary().input_tokens, 10);
    assert_eq!(ledger.summary().output_tokens, 20);
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    session.execute("production.apply", &json!({"commands":expanded.commands,"outputMode":"native_with_images"})).unwrap();
    let doc = &session.doc().unwrap().doc;
    assert_eq!(doc.assets.len(), 1);
    let asset = doc.assets.values().next().unwrap();
    assert_eq!(asset.mime, "image/png");
    assert_eq!(asset.pixels, Some((3, 2)));
    assert_eq!(image::load_from_memory(&asset.data).unwrap().to_rgba8(), pixels);
    assert_eq!(
        image::codecs::png::PngDecoder::new(std::io::Cursor::new(asset.data.as_slice())).unwrap().icc_profile().unwrap().unwrap(),
        original_profile
    );
    assert!(doc.spreads[0].items.iter().any(|item| item.label == "tree" && matches!(item.content, designcraft_engine::doc::Content::Graphic(_))));
}
#[test]
fn shared_art_views_use_one_request_one_asset_and_editable_native_crops() {
    struct ImageOnce(std::cell::Cell<u32>);
    impl Provider for ImageOnce {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("No text request".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.0.set(self.0.get() + 1);
            let (image, _) = webp_fixture(None);
            Ok(ImageResponse {
                images: vec![image],
                usage: Usage { input_tokens: Some(10), output_tokens: Some(20), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No listing".into())
        }
    }
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut profile = design_profile_fixture();
    profile.page_families[0].image_slots = vec![
        serde_json::from_value(json!({
            "id":"atlas", "purpose":"shared art", "rect":[60,180,160,280], "crop":[0,0,0.5,1], "source_aspect_ratio":1.5,
            "views":[{"rect":[200,180,300,280],"crop":[0.5,0,1,1]}]
        }))
        .unwrap(),
    ];
    let plan: ProductionPlan = serde_json::from_value(json!({"title":"Shared art","commands":[
            {"command":"frame.create","params":{"spread":0,"rect":[60,180,160,280],"content":"graphic"}},
            {"command":"frame.create","params":{"spread":0,"rect":[200,180,300,280],"content":"graphic"}}
        ],"images":[{"prompt":"Two objects","page":0,"rect":[60,180,160,280],"label":"shared art"}]}))
    .unwrap();
    let provider = ImageOnce(std::cell::Cell::new(0));
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "test".into(), endpoint: None },
        ..Default::default()
    };
    let expanded = runner.expand_images(plan, &settings, &mut BTreeMap::new(), &mut 0, Some(&profile)).unwrap();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    session.execute("production.apply", &json!({"commands":expanded.commands,"outputMode":"native_with_images"})).unwrap();
    let doc = &session.doc().unwrap().doc;
    let graphics: Vec<_> = doc.spreads[0]
        .items
        .iter()
        .filter_map(|item| match &item.content {
            designcraft_engine::doc::Content::Graphic(g) => Some(g),
            _ => None,
        })
        .collect();
    assert_eq!(graphics.len(), 2);
    assert_eq!(graphics[0].asset, graphics[1].asset);
    assert_ne!(graphics[0].crop, graphics[1].crop);
    assert_eq!(doc.assets.len(), 1);
    assert_eq!(provider.0.get(), 1);
    assert_eq!(ledger.summary().requests, 1);
}

#[test]
fn webp_normalization_rejects_malformed_oversized_chunks_and_pixel_bombs() {
    let (mut malformed, _) = webp_fixture(None);
    malformed.bytes.get_mut(16..20).unwrap().copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(prepare_generated_image(malformed).err().unwrap().contains("chunk"));
    let (mut bomb, _) = webp_fixture(None);
    // The fixture is a real VP8L image; replace only its 14-bit width/height header.
    assert_eq!(bomb.bytes.get(12..16).unwrap(), b"VP8L");
    bomb.bytes.get_mut(21..25).unwrap().copy_from_slice(&0x0fff_ffff_u32.to_le_bytes());
    assert!(prepare_generated_image(bomb).err().unwrap().contains("20 million pixels"));
    assert!(validate_image_dimensions(4096, 4096).is_ok());
    assert!(validate_image_dimensions(5001, 4000).is_err());
    let (mut oversized, _) = webp_fixture(None);
    oversized.bytes.resize(MAX_RASTER_BYTES + 1, 0);
    assert!(prepare_generated_image(oversized).is_err());
    let (mut animated, pixels) = webp_fixture(None);
    let frame_chunk = animated.bytes.get(12..).unwrap().to_vec();
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    // Valid 3x2 extended animation: alpha + animation flags, one lossless frame.
    bytes.extend_from_slice(b"VP8X\x0a\0\0\0\x12\0\0\0\x02\0\0\x01\0\0");
    bytes.extend_from_slice(b"ANIM\x06\0\0\0\0\0\0\0\0\0");
    bytes.extend_from_slice(b"ANMF");
    bytes.extend_from_slice(&u32::try_from(16 + frame_chunk.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 2, 0, 0, 1, 0, 0, 10, 0, 0, 0]);
    bytes.extend_from_slice(&frame_chunk);
    let riff_size = u32::try_from(bytes.len() - 8).unwrap();
    bytes.get_mut(4..8).unwrap().copy_from_slice(&riff_size.to_le_bytes());
    assert!(image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&bytes)).unwrap().has_animation());
    assert_eq!(image::load_from_memory(&bytes).unwrap().to_rgba8(), pixels);
    animated.bytes = bytes;
    assert!(prepare_generated_image(animated).err().unwrap().contains("static illustration"));
}

#[test]
fn normalized_png_writer_refuses_to_allocate_beyond_task_image_limit() {
    use std::io::Write;
    let mut output = BoundedImageBytes(vec![0; MAX_RASTER_BYTES - 1]);
    assert!(output.write_all(&[1, 2]).is_err());
    assert_eq!(output.0.len(), MAX_RASTER_BYTES - 1);
    output.write_all(&[1]).unwrap();
    assert_eq!(output.0.len(), MAX_RASTER_BYTES);
}

fn high_resolution_fixture() -> (providers::GeneratedImage, image::RgbImage) {
    let mut state = 0x6d2b_79f5_u32;
    let pixels = image::RgbImage::from_fn(1536, 1024, |_, _| {
        let mut pixel = [0_u8; 3];
        for channel in &mut pixel {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *channel = state as u8;
        }
        image::Rgb(pixel)
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let image = providers::GeneratedImage { bytes: bytes.into_inner(), filename: "high-resolution.png".into(), mime_type: "image/png".into() };
    assert!(image.bytes.len() > 4 * 1024 * 1024 && image.bytes.len() <= 16 * 1024 * 1024);
    (image, pixels)
}

#[test]
fn high_resolution_png_exceeding_model_json_limit_is_preserved_byte_for_byte() {
    let (image, _) = high_resolution_fixture();
    let original = image.bytes.clone();
    let result = prepare_generated_image(image).unwrap();
    assert_eq!(result.bytes, original);
    assert_eq!(designcraft_images::pixel_size(&result.bytes), Some((1536, 1024)));
    assert!(validate_image_dimensions(4096, 4096).is_ok());
    assert!(validate_image_dimensions(5056, 3392).is_ok());
    assert!(validate_image_dimensions(6336, 2688).is_ok());
    assert!(validate_image_dimensions(5000, 4000).is_ok());
    assert!(validate_image_dimensions(5001, 4000).is_err());
    assert!(validate_image_dimensions(0, 4000).is_err());
    let mut oversized = result;
    oversized.bytes.resize(MAX_RASTER_BYTES + 1, 0);
    assert!(prepare_generated_image(oversized).unwrap_err().contains("within 16 MiB"));
}

#[test]
fn high_resolution_lossless_webp_preserves_pixels_alpha_and_icc_when_normalized() {
    use image::{ImageDecoder, ImageEncoder};
    let (_, rgb) = high_resolution_fixture();
    let rgba = image::RgbaImage::from_fn(rgb.width(), rgb.height(), |x, y| {
        let [r, g, b] = rgb.get_pixel(x, y).0;
        image::Rgba([r, g, b, if x % 2 == 0 { 128 } else { 255 }])
    });
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
    encoder.set_icc_profile(TEST_SRGB_ICC.to_vec()).unwrap();
    encoder.encode(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8).unwrap();
    assert!(bytes.len() > 4 * 1024 * 1024 && bytes.len() <= 16 * 1024 * 1024);
    let expected = image::load_from_memory(&bytes).unwrap().to_rgba8();
    let normalized =
        prepare_generated_image(providers::GeneratedImage { bytes, filename: "print.webp".into(), mime_type: "image/webp".into() }).unwrap();
    assert_eq!(normalized.mime_type, "image/png");
    assert_eq!(image::load_from_memory(&normalized.bytes).unwrap().to_rgba8(), expected);
    assert_eq!(expected, rgba);
    assert_eq!(image::codecs::png::PngDecoder::new(std::io::Cursor::new(&normalized.bytes)).unwrap().icc_profile().unwrap().unwrap(), TEST_SRGB_ICC);
}

#[test]
fn high_resolution_shared_sources_have_separate_unique_and_inline_byte_budgets() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct LargeImage(providers::GeneratedImage, std::cell::Cell<u32>);
    impl Provider for LargeImage {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("No model text".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.1.set(self.1.get() + 1);
            Ok(ImageResponse { images: vec![self.0.clone()], usage: Usage::default(), error: None })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let (source, _) = high_resolution_fixture();
    let source_bytes = source.bytes.len();
    let provider = LargeImage(source, std::cell::Cell::new(0));
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 8, ..Default::default() },
        ..Default::default()
    };
    let placement = designcraft_ai_types::protocol::ImagePlacement {
        family_id: None,
        slot_id: None,
        repeat_rects: Vec::new(),
        repeat_groups: Vec::new(),
        prompt: "One original scene reused".into(),
        page: 0,
        rect: [60.0, 60.0, 160.0, 160.0],
        label: "Scene".into(),
    };
    let plan = ProductionPlan { title: "Large shared source".into(), commands: vec![], images: vec![placement.clone(); 3] };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let mut cache = BTreeMap::new();
    let mut unique_bytes = 0;
    let expanded = runner.expand_images(plan, &settings, &mut cache, &mut unique_bytes, None).unwrap();
    assert_eq!(provider.1.get(), 1, "Shared source should be generated once");
    assert_eq!(unique_bytes, source_bytes, "Unique source bytes are counted once");
    assert_eq!(expanded.commands.iter().filter(|action| action.command == "file.place").count(), 3);
    assert!(serde_json::to_vec(&expanded).unwrap().len() > 16 * 1024 * 1024);
    assert!(serde_json::to_vec(&expanded).unwrap().len() < 48 * 1024 * 1024);
    let repeated = ProductionPlan { title: "Bound repeated inline bytes".into(), commands: vec![], images: vec![placement; 8] };
    let error = runner.expand_images(repeated, &settings, &mut cache, &mut unique_bytes, None).unwrap_err();
    assert!(error.contains("including repeated views"), "{error}");
    assert_eq!(unique_bytes, source_bytes, "Repeated serialization must not recharge the unique source budget");
    assert_eq!(provider.1.get(), 1, "Oversized repetitions must use the already cached source");
}

#[test]
fn preflight_errors_are_repaired_before_returning_a_plan() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let base = session.doc().unwrap().clone();
    let catalog = session.execute("production.catalog", &json!({})).unwrap();
    let mut overset = plan();
    overset["commands"][0]["params"] = json!({"rect":[60,60,62,62],"content":"text","text":"Synthetic overflowing text. ".repeat(100),"caret":false});
    let invalid: ProductionPlan = serde_json::from_value(overset.clone()).unwrap();
    let error = simulate(&invalid, &base, &session.prefs, OutputMode::NativeOnly, &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("preflight"), "{error}");
    let mut overlap = plan();
    overlap["commands"] = json!([
        {"command":"frame.create","params":{"rect":[60,60,250,120],"content":"text","text":"First instruction","caret":false}},
        {"command":"frame.create","params":{"rect":[80,80,270,140],"content":"text","text":"Second instruction","caret":false}}
    ]);
    let invalid: ProductionPlan = serde_json::from_value(overlap.clone()).unwrap();
    let error = simulate(&invalid, &base, &session.prefs, OutputMode::NativeOnly, &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("textOverlap"), "{error}");
    let response = |value: Value| {
        Ok(GenerationResponse {
            text: value.to_string(),
            usage: Usage { input_tokens: Some(100), output_tokens: Some(100), ..Default::default() },
            error: None,
        })
    };
    let provider = Mock { responses: RefCell::new(vec![response(plan()), response(overlap)]) };
    let mut ledger = UsageLedger::new(BudgetSpec { token_limit: 500_000, ..Default::default() }).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let request = TaskRequest {
        prompt: "Create a clean native rectangle".into(),
        settings: AiSettings { allow_paid_repairs: true, budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() },
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    assert!(matches!(runner.production(request, json!({}), catalog, vec![], base, session.prefs.clone()), Ok(Product::Plan(_))));
    assert_eq!(ledger.summary().requests, 2);
}

#[test]
fn skill_requires_analysis_for_every_selected_uncached_digest() {
    let digest = "b".repeat(64);
    let references = json!([{"digest":digest,"cached":false}]);
    let draft = || SkillDraft {
        design_profile: None,
        page_templates: Vec::new(),
        skill_id: "native-layout".into(),
        description: "Native sample".into(),
        skill_md: "Review the sample.".into(),
        compatibility_notes: String::new(),
        sample_actions: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[0,0,100,100]}) }],
        analyses: vec![],
    };
    assert!(package_draft(draft(), &catalog(), &references, vec![]).is_err());
    let mut complete = draft();
    complete.analyses.push(AnalysisDraft { reference_sha256: digest, text: "Observed generous margins; source trim is unknown.".into() });
    let (_, analyses) = package_draft(complete, &catalog(), &references, vec![]).unwrap();
    assert_eq!(analyses.len(), 1);
    let mut substituted = draft();
    substituted.analyses.push(AnalysisDraft { reference_sha256: "c".repeat(64), text: "Unselected material".into() });
    assert!(package_draft(substituted, &catalog(), &references, vec![]).is_err());
}

struct ReviewMock {
    verdict: &'static str,
    images: std::cell::Cell<u32>,
    reviews: std::cell::Cell<u32>,
}
impl Provider for ReviewMock {
    fn generate(
        &self,
        selection: &ModelSelection,
        _: Option<&str>,
        request: &GenerationRequest,
        _: &AtomicBool,
    ) -> Result<GenerationResponse, String> {
        self.reviews.set(self.reviews.get() + 1);
        assert_eq!(selection.model, "gemini-3.8-flash");
        assert_eq!(request.images.len(), 1);
        assert!(image::load_from_memory(&request.images[0].bytes).is_ok());
        Ok(GenerationResponse {
            text: json!({"verdict":self.verdict,"reasons":["Synthetic visible-subject assessment"]}).to_string(),
            usage: Usage::default(),
            error: None,
        })
    }
    fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
        self.images.set(self.images.get() + 1);
        Ok(ImageResponse { images: vec![review_projection_image()?], usage: Usage::default(), error: None })
    }
    fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
        Err("No discovery".into())
    }
}
fn review_settings(max_requests: u32) -> AiSettings {
    AiSettings {
        verify_generated_images: true,
        output_mode: OutputMode::NativeWithImages,
        text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests, ..Default::default() },
        ..Default::default()
    }
}
fn review_plan() -> ProductionPlan {
    ProductionPlan {
        title: "Review scene".into(),
        commands: vec![],
        images: vec![designcraft_ai_types::protocol::ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: Vec::new(),
            repeat_groups: Vec::new(),
            prompt: "Five monkeys sitting in a tree".into(),
            page: 0,
            rect: [60.0, 60.0, 160.0, 160.0],
            label: "Monkeys".into(),
        }],
    }
}

#[test]
fn image_review_failure_blocks_cache_and_accepted_art_reuses_without_paid_reinspection() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    for verdict in ["fail", "uncertain", "pass"] {
        let directory = tempfile::tempdir().unwrap();
        let provider = ReviewMock { verdict, images: std::cell::Cell::new(0), reviews: std::cell::Cell::new(0) };
        let settings = review_settings(8);
        let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let mut runner = Runner {
            cache_root: Some(directory.path().to_path_buf()),
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        };
        let mut cache = BTreeMap::new();
        let mut bytes = 0;
        let result = runner.expand_images(review_plan(), &settings, &mut cache, &mut bytes, None);
        assert_eq!(provider.images.get(), 1);
        assert_eq!(provider.reviews.get(), 1);
        assert_eq!(runner.ledger.summary().requests, 2);
        if verdict == "fail" {
            let first = result.expect("advisory fail should soft-warn for human proof instead of hard-aborting the spread");
            assert!(!first.commands.is_empty());
            assert!(cache.is_empty(), "Flagged artwork must not enter memory cache");
            assert_eq!(bytes, 0);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0, "Flagged artwork must not enter the disk cache");
            let diagnostics: Vec<_> =
                receiver.try_iter().filter_map(|message| if let Message::Diagnostic(issue) = message { Some(issue) } else { None }).collect();
            assert!(
                diagnostics.iter().any(|issue| {
                    issue["kind"] == "illustrationSemanticReview" && issue["verdict"] == "advisory_fail" && issue["severity"] == "warning"
                }),
                "Expected advisory_fail warning diagnostic, got {diagnostics:?}"
            );
        } else {
            let first = result.unwrap();
            assert_eq!(cache.len(), 1);
            let reused = runner.expand_images(review_plan(), &settings, &mut cache, &mut bytes, None).unwrap();
            assert_eq!(reused.commands, first.commands);
            assert_eq!(provider.images.get(), 1);
            assert_eq!(provider.reviews.get(), 1);
            let diagnostics: Vec<_> =
                receiver.try_iter().filter_map(|message| if let Message::Diagnostic(issue) = message { Some(issue) } else { None }).collect();
            assert!(diagnostics.iter().any(|issue| issue["kind"] == "illustrationSemanticReview" && issue["verdict"] == verdict));
            if verdict == "uncertain" {
                assert!(diagnostics.iter().any(|issue| issue["severity"] == "warning"));
            }
        }
    }
}

#[test]
fn image_review_request_budget_is_admitted_before_buying_the_first_image() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let provider = ReviewMock { verdict: "pass", images: std::cell::Cell::new(0), reviews: std::cell::Cell::new(0) };
    let settings = review_settings(1);
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let error = runner.expand_images(review_plan(), &settings, &mut BTreeMap::new(), &mut 0, None).unwrap_err();
    assert!(error.contains("request limit"), "{error}");
    assert_eq!(runner.ledger.summary().requests, 0);
    assert_eq!(provider.images.get(), 0);
    assert_eq!(provider.reviews.get(), 0);
    let mut unsupported = settings.clone();
    unsupported.text_model.model = "gemini-embedding-001".into();
    assert!(
        runner.expand_images(review_plan(), &unsupported, &mut BTreeMap::new(), &mut 0, None).unwrap_err().contains("no provider or model fallback")
    );
}

#[test]
fn image_review_cache_identity_changes_with_policy_model_and_comfy_workflow() {
    let provider = ReviewMock { verdict: "pass", images: std::cell::Cell::new(0), reviews: std::cell::Cell::new(0) };
    let mut settings = review_settings(8);
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let placement = review_plan().images.remove(0);
    let original = image_memory_key(&placement, "style", &settings).unwrap();
    let disk = runner.image_disk_key(&placement, "style", None, &settings).unwrap();
    settings.verify_generated_images = false;
    assert_ne!(original, image_memory_key(&placement, "style", &settings).unwrap());
    assert_ne!(disk, runner.image_disk_key(&placement, "style", None, &settings).unwrap());
    settings.verify_generated_images = true;
    settings.text_model.model = "gemini-2.5-flash".into();
    assert_ne!(original, image_memory_key(&placement, "style", &settings).unwrap());
    assert_ne!(disk, runner.image_disk_key(&placement, "style", None, &settings).unwrap());
    settings = review_settings(8);
    settings.comfy_workflow = Some(json!({"seed":1}));
    assert_ne!(original, image_memory_key(&placement, "style", &settings).unwrap());
    assert_ne!(disk, runner.image_disk_key(&placement, "style", None, &settings).unwrap());
    assert_eq!(image_review_identity(&settings)["policy"], crate::image_review::POLICY_VERSION);
}

#[test]
fn generic_facing_request_includes_future_companion_as_context_only() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct InspectFacing(std::cell::Cell<u32>);
    impl Provider for InspectFacing {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.0.set(self.0.get() + 1);
            let value: Value = serde_json::from_str(&request.prompt).unwrap();
            let facing = &value["document"]["facing_page_context"];
            assert_eq!(facing["produce_only_pages"], json!([11]));
            assert_eq!(facing["pairs"][0]["human_pages"], json!([12, 13]));
            assert_eq!(facing["pairs"][0]["members"][1]["role"], "context_only");
            assert!(request.system.contains("Linking every facing pair is not required"));
            assert!(request.system.contains("never widen the mandatory batch scope"));
            Err("Facing context inspected before synthetic stop".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No artwork".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let provider = InspectFacing(std::cell::Cell::new(0));
    let prompt:String=(1..=14).map(|page|format!("### PAGE {page:02} — Ocean lesson\n* **English Print Copy (LTR):**\n > Lesson {page}\n* **Visual & Illustration Prompt (Art Direction):** Ocean scene\n")).collect();
    let settings = AiSettings { facing_pages: true, budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt,
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 14 },
        export_directory: None,
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":14,"facingPages":false})).unwrap();
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let error = runner
        .generic_production_plan(
            &request,
            &json!({}),
            &catalog(),
            &[],
            session.doc().unwrap(),
            &session.prefs,
            Some(BatchScope { start: 11, count: 1 }),
        )
        .unwrap_err();
    assert!(error.contains("Facing context inspected"), "{error}");
    assert_eq!(provider.0.get(), 1);
}

#[test]
fn generic_cached_geometry_is_checked_before_artwork_and_raw_cache_writes() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let directory = tempfile::tempdir().unwrap();
    let mut raw = review_plan();
    raw.commands = vec![EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[60,60,160,160],"content":"unassigned"}) }];
    raw.images[0].rect = [200.0, 60.0, 300.0, 160.0];
    let provider = Mock {
        responses: RefCell::new(vec![Ok(GenerationResponse { text: serde_json::to_string(&raw).unwrap(), usage: Usage::default(), error: None })]),
    };
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        budget: BudgetSpec { token_limit: 500_000, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Draw one illustrated lesson".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 1 },
        export_directory: None,
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let skills = vec![text_fit_style()];
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: Some(directory.path().to_path_buf()),
        print: None,
        planning_only: true,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    runner.generic_production_plan(&request, &json!({}), &catalog(), &skills, session.doc().unwrap(), &session.prefs, None).unwrap();
    let entry = std::fs::read_dir(directory.path()).unwrap().next().unwrap().unwrap().path();
    let key = entry.file_name().unwrap().to_str().unwrap().strip_suffix(".plan.json").unwrap();
    raw.commands[0].params["rect"] = json!([0, 0, 20, 20]);
    crate::production_cache::store_plan(directory.path(), key, &raw).unwrap();
    runner.planning_only = false;
    let error = runner.generic_production_plan(&request, &json!({}), &catalog(), &skills, session.doc().unwrap(), &session.prefs, None).unwrap_err();
    assert!(error.contains("safe margin"), "{error}");
    assert_eq!(runner.ledger.summary().requests, 1, "Cached geometry rejection must not buy artwork or another plan");
    assert!(provider.responses.borrow().is_empty());
    let fresh = tempfile::tempdir().unwrap();
    runner.cache_root = Some(fresh.path().to_path_buf());
    runner.planning_only = true;
    provider.responses.borrow_mut().push(Ok(GenerationResponse { text: serde_json::to_string(&raw).unwrap(), usage: Usage::default(), error: None }));
    let error = runner.generic_production_plan(&request, &json!({}), &catalog(), &skills, session.doc().unwrap(), &session.prefs, None).unwrap_err();
    assert!(error.contains("safe margin"), "{error}");
    let retained: Vec<_> = std::fs::read_dir(fresh.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(
        retained,
        [std::ffi::OsString::from("last-rejected-response.txt")],
        "Invalid fresh geometry may retain private failure evidence but never a reusable plan or image"
    );
}

#[test]
fn adaptive_explicit_script_omits_large_template_examples_but_keeps_exact_pinned_guidance() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let body = "AUTHORITATIVE_SHARED_REFERENCE ".repeat(900);
    let mut style = text_fit_style();
    style.instructions = "EXACT_ORIGINAL_STYLE_INSTRUCTION".into();
    style.revision = "immutable-style-revision".into();
    style.references.insert("references/style.md".into(), body.clone());
    style.reference_hashes.insert("references/style.md".into(), "original-reference-digest".into());
    style.design_profile.as_mut().unwrap().name = "ORIGINAL_PROFILE_IDENTITY".into();
    for actions in style.templates.values_mut() {
        for action in actions.iter_mut().filter(|action| action.command == "object.label") {
            action.params["label"] = json!("UNUSED_NATIVE_TEMPLATE_EXAMPLE".repeat(8));
        }
    }
    crate::template_plan::preflight(&style, BatchScope { start: 0, count: 1 }, OutputMode::NativeOnly).unwrap();
    let mut support = style.clone();
    support.skill_id = "support-guidance".into();
    support.revision = "immutable-support-revision".into();
    support.instructions = "EXACT_SUPPORT_INSTRUCTION".into();
    support.templates.clear();
    support.design_profile = None;
    let skills = vec![style, support];
    let original = serde_json::to_value(&skills).unwrap();
    assert!(serde_json::to_vec(&skills).unwrap().len() > 64 * 1024);
    assert!(compact_skill_guidance(&skills).unwrap().len() < 32 * 1024);
    struct InspectAdaptive(std::cell::Cell<u32>);
    impl Provider for InspectAdaptive {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.0.set(self.0.get() + 1);
            for preserved in [
                "EXACT_ORIGINAL_STYLE_INSTRUCTION",
                "EXACT_SUPPORT_INSTRUCTION",
                "ORIGINAL_PROFILE_IDENTITY",
                "immutable-style-revision",
                "immutable-support-revision",
                "original-reference-digest",
            ] {
                assert!(request.system.contains(preserved), "Lost pinned guidance: {preserved}");
            }
            assert_eq!(request.system.matches("AUTHORITATIVE_SHARED_REFERENCE").count(), 900, "Shared reference bodies must not be duplicated");
            assert!(!request.system.contains("UNUSED_NATIVE_TEMPLATE_EXAMPLE"));
            assert!(request.system.contains("CURRENT DOCUMENT GEOMETRY"));
            assert!(!request.system.contains("Preserve page_mm and margin_mm"));
            let input: Value = serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(input["document"]["pageWidth"].as_f64(), Some(720.0));
            assert_eq!(input["document"]["pageGeometry"][0]["margins"]["inside"].as_f64(), Some(51.0));
            assert_eq!(input["document"]["source_copy"]["segments"][0]["id"], "p1.en.1");
            assert_eq!(input["document"]["source_copy"]["segments"][0]["text"], "My book");
            assert!(request.json_schema["properties"]["copy_bindings"].is_object());
            Err("Adaptive context reached the selected provider".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No image generation".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let provider = InspectAdaptive(std::cell::Cell::new(0));
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request=TaskRequest{prompt:"### PAGE 01 — Ownership\n* **English Print Copy (LTR):**\n > My book\n### PAGE 02 — Guide\n* **English Print Copy (LTR):**\n > Teacher guide\n".into(),settings:settings.clone(),skill_ids:vec![],auto_select_skills:false,target:designcraft_ai_types::protocol::TaskTarget::NewA4{pages:2},export_directory:None};
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":720,"height":960,"pages":2,"facingPages":false,"margins":51})).unwrap();
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let error = runner
        .production(
            request,
            json!({"batch_scope":{"start":0,"count":2}}),
            catalog(),
            skills.clone(),
            session.doc().unwrap().clone(),
            session.prefs.clone(),
        )
        .err()
        .unwrap();
    assert!(error.contains("Adaptive context reached"), "{error}");
    assert_eq!(provider.0.get(), 1);
    assert_eq!(serde_json::to_value(&skills).unwrap(), original, "Host snapshots must remain immutable");
}

#[test]
fn ai21_resolved_print_fonts_reach_planner_before_geometry_is_authored() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let base = session.doc().unwrap().clone();
    let fonts = designcraft_fonts::FontDb::global().scoped(base.doc.font_scope);
    let family = "Source Sans 3";
    assert!(fonts.has_family(family), "Use the bundled document font, independent of system font discovery");
    let prompt = format!("**English Primary Font:** {family}\nCreate one editable heading.");
    let expected =
        serde_json::to_value(crate::print_contract::PrintContract::from_prompt(&prompt).unwrap().unwrap().select_fonts(&base).unwrap()).unwrap();
    struct InspectPrintFonts(Value);
    impl Provider for InspectPrintFonts {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let input: Value = serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(input["document"]["resolved_print_fonts"], self.0, "host-bound font metrics must be known before drawing text frames");
            Err("Resolved print fonts reached the planner".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            panic!("no images before a valid native plan")
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            panic!("no model discovery")
        }
    }
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt,
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = InspectPrintFonts(expected);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let error = runner.production(request, json!({}), catalog(), vec![], base, session.prefs.clone()).err().unwrap();
    assert!(error.contains("Resolved print fonts reached"), "{error}");
    assert_eq!(ledger.summary().requests, 1);
}

#[test]
fn ai20_opened_custom_book_continuation_adapts_style_commands_to_actual_geometry() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct InspectOpened;
    impl Provider for InspectOpened {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            assert!(request.system.contains("CURRENT DOCUMENT GEOMETRY"));
            assert!(!request.system.contains("Preserve page_mm and margin_mm"));
            let input: Value = serde_json::from_str(&request.prompt).unwrap();
            assert_eq!(input["document"]["pageWidth"].as_f64(), Some(720.0));
            assert_eq!(input["document"]["pageGeometry"][1]["margins"]["inside"].as_f64(), Some(51.0));
            Err("Opened continuation reached the selected provider".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No image generation".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let settings = AiSettings { budget: BudgetSpec { token_limit: 500_000, ..Default::default() }, ..Default::default() };
    let request = TaskRequest {
        prompt: "Continue the remaining editable pages of this book with a different activity.".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::ContinueBook,
        export_directory: None,
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"width":720,"height":960,"pages":2,"facingPages":false,"margins":51})).unwrap();
    session.execute("frame.create", &json!({"spread":0,"rect":[60,60,180,120]})).unwrap();
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &InspectOpened,
    };
    let error = runner
        .production(
            request,
            json!({"batch_scope":{"start":1,"count":1}}),
            catalog(),
            vec![text_fit_style()],
            session.doc().unwrap().clone(),
            session.prefs.clone(),
        )
        .err()
        .unwrap();
    assert!(error.contains("Opened continuation reached"), "{error}");
}

#[test]
fn incomplete_provider_generation_enters_repair_loop_and_records_partial_evidence() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    struct RecoverAfterIncomplete(std::cell::Cell<u32>);
    impl Provider for RecoverAfterIncomplete {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let call = self.0.get();
            self.0.set(call + 1);
            if call == 0 {
                Ok(GenerationResponse {
                    text: "{\"title\":\"partial truncated".into(),
                    usage: Usage { input_tokens: Some(120), output_tokens: Some(240), ..Default::default() },
                    error: Some("Gemini response did not finish normally".into()),
                })
            } else {
                let repair: Value = serde_json::from_str(&request.prompt).unwrap();
                assert_eq!(repair["validation_error"], "Gemini response did not finish normally");
                assert_eq!(repair["rejected_plan"], "{\"title\":\"partial truncated");
                assert!(repair["instruction"].as_str().unwrap_or_default().contains("compact single-line JSON"), "{repair}");
                Ok(GenerationResponse {
                        text: serde_json::to_string(&json!({
                            "title": "Recovered compact plan",
                            "commands": [
                                {"command":"frame.create","params":{"spread":0,"rect":[72.0,72.0,300.0,160.0],"content":"text","text":"Recovered heading"}}
                            ],
                            "images": []
                        }))
                        .unwrap(),
                        usage: Usage { input_tokens: Some(140), output_tokens: Some(90), ..Default::default() },
                        error: None,
                    })
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No image generation".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let settings = AiSettings {
        output_mode: OutputMode::NativeOnly,
        allow_paid_repairs: true,
        budget: BudgetSpec { token_limit: 500_000, repair_attempts: 2, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Create one heading frame on page 1.".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = RecoverAfterIncomplete(std::cell::Cell::new(0));
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let product = runner.production(request, json!({}), catalog(), vec![], session.doc().unwrap().clone(), session.prefs.clone()).unwrap();
    assert_eq!(provider.0.get(), 2);
    let Product::Plan(plan) = product else {
        panic!("expected single-scope production plan");
    };
    assert_eq!(plan.title, "Recovered compact plan");
    assert_eq!(ledger.summary().requests, 2);
    let bounded = repair_prompt("orig", &"x".repeat(12_000), "Gemini response did not finish normally").unwrap();
    let bounded_json: Value = serde_json::from_str(&bounded).unwrap();
    assert!(bounded_json["rejected_plan"].as_str().unwrap_or_default().ends_with("...[truncated 3808 bytes]"));
    assert!(bounded_json["instruction"].as_str().unwrap_or_default().contains("batch shared object.fill"));
}

#[test]
fn current_document_edits_existing_objects_and_replaces_graphic_frames_in_place() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let heading = session.execute("frame.create", &json!({"spread":0,"rect":[72.0,72.0,320.0,150.0],"content":"text","text":"Old Title"})).unwrap();
    let heading_id = heading["id"].as_u64().unwrap();
    let heading_story = heading["story"].as_u64().unwrap();
    let hero_graphic =
        session.execute("frame.create", &json!({"spread":0,"rect":[72.0,180.0,272.0,380.0],"content":"graphic"})).unwrap()["id"].as_u64().unwrap();
    let obsolete_id = session.execute("frame.create", &json!({"spread":0,"rect":[340.0,180.0,440.0,240.0],"content":"unassigned"})).unwrap()["id"]
        .as_u64()
        .unwrap();

    struct EditOpenedDoc {
        heading_id: u64,
        heading_story: u64,
        obsolete_id: u64,
    }
    impl Provider for EditOpenedDoc {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, request: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            assert!(request.system.contains("EXISTING DOCUMENT EDITING CONTRACT"));
            Ok(GenerationResponse {
                text: serde_json::to_string(&json!({
                    "title": "Edited Existing Document",
                    "commands": [
                        {"command":"edit.clear","params":{"ids":[self.obsolete_id]}},
                        {"command":"story.setText","params":{"story":self.heading_story,"text":"Updated Title"}},
                        {"command":"object.cornerOptions","params":{"ids":[self.heading_id],"shape":"rounded","size":8.0}},
                        {"command":"object.opacity","params":{"ids":[self.heading_id],"opacity":95.0}}
                    ],
                    "images": [
                        {"page":0,"rect":[72.0,180.0,272.0,380.0],"prompt":"Clean geometric emblem","label":"Hero Emblem"}
                    ]
                }))
                .unwrap(),
                usage: Usage { input_tokens: Some(120), output_tokens: Some(90), ..Default::default() },
                error: None,
            })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Ok(ImageResponse {
                images: vec![ai24_png(1024, 1024, 180)],
                usage: Usage { input_tokens: Some(50), output_tokens: Some(100), ..Default::default() },
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("No discovery".into())
        }
    }

    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        verify_generated_images: false,
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 16, ..Default::default() },
        ..Default::default()
    };
    let request = TaskRequest {
        prompt: "Delete the obsolete badge, update the heading text, round its corners, and place a new emblem into the existing hero graphic frame."
            .into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = EditOpenedDoc { heading_id, heading_story, obsolete_id };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let real_catalog = session.execute("production.catalog", &json!({})).unwrap();
    let product = runner.production(request, json!({}), real_catalog, vec![], session.doc().unwrap().clone(), session.prefs.clone()).unwrap();
    let Product::Plan(plan) = product else {
        panic!("expected single-scope production plan");
    };
    assert!(
        !plan.commands.iter().any(|action| action.command == "frame.create"),
        "existing graphic frame should be reused in place without creating a duplicate frame: {:?}",
        plan.commands
    );
    let place_action = plan.commands.iter().find(|action| action.command == "file.place").expect("file.place action");
    assert_eq!(place_action.params["frame"].as_u64(), Some(hero_graphic));
    session.execute("production.apply", &json!({"outputMode":"native_with_images","commands":plan.commands,"requirePreflight":true})).unwrap();
    let doc = &session.doc().unwrap().doc;
    assert!(doc.item(designcraft_engine::doc::ItemId(obsolete_id)).is_none());
    assert_eq!(doc.story(designcraft_engine::doc::StoryId(heading_story)).unwrap().text, "Updated Title");
    assert!((doc.item(designcraft_engine::doc::ItemId(heading_id)).unwrap().opacity - 0.95).abs() < 1e-4);
    assert!(doc.item(designcraft_engine::doc::ItemId(hero_graphic)).unwrap().graphic().is_some());
}

#[test]
fn parse_and_simulate_expanded_tools_aliases_and_opened_document_geometry_edits() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4"})).unwrap();
    let real_catalog = session.execute("production.catalog", &json!({})).unwrap();
    let raw = serde_json::to_string(&json!({
            "title": "Expanded Native Tools Plan",
            "commands": [
                {"command": "style.paragraph.create", "params": {"name": "LeadStyle", "para": {"listType": "bullets", "spaceAfter": 4.0}, "chars": {"fontFamily": "Tahoma", "fontStyle": "Regular", "size": 14.0, "leading": {"kind": "points", "value": 20.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 72.0, 260.0, 140.0], "content": "text", "text": "Initial", "caret": false}},
                {"command": "type.set", "params": {"story": "$1.story", "text": "Item A\nItem B"}},
                {"command": "style.paragraph.apply", "params": {"name": "LeadStyle", "clearOverrides": true}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [280.0, 72.0, 480.0, 140.0], "content": "text", "text": "", "caret": false}},
                {"command": "text.thread", "params": {"from": "$1.id", "to": "$4.id"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 160.0, 132.0, 220.0], "shape": "ellipse", "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [102.0, 160.0, 162.0, 220.0], "shape": "ellipse", "content": "unassigned"}},
                {"command": "object.pathfinder", "params": {"ids": ["$6.id", "$7.id"], "op": "add"}},
                {"command": "object.gradient", "params": {"ids": ["$8.id"], "kind": "linear", "angle": 45.0, "stops": [{"pos": 0.0, "color": "#1E3A8A"}, {"pos": 1.0, "color": "#D97706"}]}},
                {"command": "object.effects", "params": {"ids": ["$8.id"], "effects": {"outerGlow": {"on": true, "color": "[Paper]", "opacity": 0.4, "size": 4.0}}}},
                {"command": "edit.stepAndRepeat", "params": {"ids": ["$8.id"], "count": 1, "dx": 120.0, "dy": 0.0}},
                {"command": "object.qrCode", "params": {"spread": 0, "rect": [360.0, 160.0, 432.0, 232.0], "type": "url", "content": "https://designcraft.example"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 250.0, 480.0, 460.0], "content": "text", "text": "", "caret": true}},
                {"command": "table.insert", "params": {"rows": 2, "cols": 2, "width": 360.0}},
                {"command": "table.insertRow", "params": {"story": "$13.story", "table": "$14.id", "row": 1, "where": "below", "count": 1}},
                {"command": "table.mergeCells", "params": {"story": "$13.story", "table": "$14.id", "rows": [0, 0], "cols": [0, 1]}},
                {"command": "style.cell.create", "params": {"name": "HeaderCell", "fill": "[Paper]", "insets": 6.0, "vj": "center"}},
                {"command": "style.cell.apply", "params": {"name": "HeaderCell", "story": "$13.story", "table": "$14.id", "rows": [0, 0], "cols": [0, 1]}},
                {"command": "table.setCell", "params": {"story": "$13.story", "table": "$14.id", "row": 0, "col": 0, "text": "Header"}}
            ],
            "images": []
        }))
        .unwrap();
    let mut plan = parse_plan(&raw, OutputMode::NativeOnly, &real_catalog).unwrap();
    let cancel = AtomicBool::new(false);
    crate::native_layout::prepare(&mut plan, session.doc().unwrap(), &session.prefs, &cancel).unwrap();
    simulate_styled(
        &plan,
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeOnly,
        &cancel,
        None,
        GeometryCheck::OpenedDocument { allow_geometry_edits: false },
    )
    .unwrap();

    let page_add_raw = serde_json::to_string(&json!({
            "title": "Add Requested Page",
            "commands": [
                {"command": "layout.pages.insert", "params": {"count": 1}},
                {"command": "frame.create", "params": {"spread": 1, "rect": [72.0, 72.0, 300.0, 140.0], "content": "text", "text": "Second Page", "caret": false}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": "Tahoma", "fontStyle": "Regular", "size": 14.0, "leading": {"kind": "points", "value": 20.0}}}}
            ],
            "images": []
        }))
        .unwrap();
    let mut page_add_plan = parse_plan(&page_add_raw, OutputMode::NativeOnly, &real_catalog).unwrap();
    crate::native_layout::prepare(&mut page_add_plan, session.doc().unwrap(), &session.prefs, &cancel).unwrap();
    assert!(prompt_requests_document_geometry_edit("Please add a new page and place a heading on it."));
    assert!(
        simulate_styled(
            &page_add_plan,
            session.doc().unwrap(),
            &session.prefs,
            OutputMode::NativeOnly,
            &cancel,
            None,
            GeometryCheck::OpenedDocument { allow_geometry_edits: false },
        )
        .is_err()
    );
    simulate_styled(
        &page_add_plan,
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeOnly,
        &cancel,
        None,
        GeometryCheck::OpenedDocument { allow_geometry_edits: true },
    )
    .unwrap();

    let batch_raw = serde_json::to_string(&json!({
            "title": "Batch Gradient and Threading",
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 72.0, 240.0, 140.0], "content": "text", "text": "Batch Story", "caret": false}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": "Tahoma", "fontStyle": "Regular", "size": 14.0, "leading": {"kind": "points", "value": 20.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [260.0, 72.0, 440.0, 140.0], "content": "text", "text": "", "caret": false}},
                {"command": "text.thread", "params": {"from": "$0.id", "to": "$2.id"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 170.0, 240.0, 240.0], "content": "unassigned"}},
                {"command": "object.gradient", "params": {"ids": ["$4.id"], "kind": "linear", "stops": [{"pos": 0.0, "color": "#1E3A8A"}, {"pos": 1.0, "color": "#3B82F6"}]}}
            ],
            "images": []
        }))
        .unwrap();
    let mut batch_plan = parse_plan(&batch_raw, OutputMode::NativeOnly, &real_catalog).unwrap();
    crate::native_layout::prepare(&mut batch_plan, session.doc().unwrap(), &session.prefs, &cancel).unwrap();
    simulate_scoped(&batch_plan, session.doc().unwrap(), &session.prefs, OutputMode::NativeOnly, &cancel, Some(BatchScope { start: 0, count: 1 }))
        .unwrap();
}

#[test]
fn vocalized_script_contract_preserves_diacritics_despite_plain_meta_phrases_and_enforces_min_ppi() {
    let mut prompt = String::from("ملاحظة عامة: واجهة التعليمات بدون تشكيل خارج النص المعتمد.\n");
    prompt.push_str("# PAGE 1 — حَدِيقَةُ الحُرُوفِ\n* **Arabic Print Copy**: لَوِّنِ الحَرْفَ المَطْلُوبَ\n");
    let mut plan = ProductionPlan {
        title: "حَدِيقَةُ الحُرُوفِ".into(),
        commands: vec![EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": [72.0, 72.0, 320.0, 140.0], "content": "text", "text": "لَوِّنِ الحَرْفَ المَطْلُوبَ", "caret": false}),
        }],
        images: Vec::new(),
    };
    prepare_editorial_plan(&mut plan, &prompt, &json!({}), None).unwrap();
    assert!(plan.title.chars().any(arabic_vowel_mark), "vocalized script title must keep tashkeel");
    let body = plan.commands[0].params.get("text").and_then(Value::as_str).unwrap();
    assert!(body.chars().any(arabic_vowel_mark), "vocalized script copy must keep tashkeel");

    // Verify threaded frame creation on a second page satisfies simulate_group_scoped_with_ppi
    // and PrintContract min_ppi (300 PPI) rejects a low-resolution placed raster image.
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages": 2, "facingPages": false})).unwrap();
    let cancel = AtomicBool::new(false);
    let group_plan = ProductionPlan {
        title: "Two-page threaded chunk".into(),
        commands: vec![
            EngineAction {
                command: "frame.create".into(),
                params: json!({"spread": 0, "rect": [72.0, 72.0, 300.0, 140.0], "content": "text", "text": "Page 1", "caret": false}),
            },
            EngineAction { command: "text.thread".into(), params: json!({"from": "$0.id", "spread": 1, "rect": [72.0, 72.0, 300.0, 140.0]}) },
        ],
        images: Vec::new(),
    };
    simulate_group_scoped_with_ppi(
        std::slice::from_ref(&group_plan),
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeOnly,
        &cancel,
        BatchScope { start: 0, count: 2 },
        Some(300.0),
    )
    .unwrap();

    // Create a 300x300 px PNG placed into a 144x144 pt (2x2 in = 150 PPI) frame:
    // passes at 150 PPI, rejected at 300 PPI in both single-plan and review-group simulation!
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(300, 300, image::Rgba([40, 120, 200, 255])))
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .unwrap();
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let img_plan = ProductionPlan {
        title: "150 PPI image".into(),
        commands: vec![
            EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [72.0, 72.0, 216.0, 216.0], "content": "graphic"}) },
            EngineAction { command: "file.place".into(), params: json!({"frame": "$0.id", "name": "hero.png", "base64": encoded}) },
            EngineAction { command: "object.fit".into(), params: json!({"ids": ["$0.id"], "mode": "fillProportionally"}) },
        ],
        images: Vec::new(),
    };
    simulate_styled_with_ppi(
        &img_plan,
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeWithImages,
        &cancel,
        Some(BatchScope { start: 0, count: 1 }),
        GeometryCheck::Engine,
        Some(150.0),
    )
    .unwrap();
    let err = simulate_styled_with_ppi(
        &img_plan,
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeWithImages,
        &cancel,
        Some(BatchScope { start: 0, count: 1 }),
        GeometryCheck::Engine,
        Some(300.0),
    )
    .unwrap_err();
    assert!(err.to_ascii_lowercase().contains("ppi") || err.contains("300"), "err={err}");

    let group_err = simulate_group_scoped_with_ppi(
        std::slice::from_ref(&img_plan),
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeWithImages,
        &cancel,
        BatchScope { start: 0, count: 1 },
        Some(300.0),
    )
    .unwrap_err();
    assert!(group_err.to_ascii_lowercase().contains("ppi") || group_err.contains("300"), "group_err={group_err}");
}

#[test]
fn batch_plan_and_identifier_checks_accept_non_thread_from_to_last_and_normalized_ids() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 2, "facingPages": false})).unwrap();
    let cancel = AtomicBool::new(false);

    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Non-thread from/to, rect-less QR, stepAndRepeat, and $last",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 120.0, 240.0, 220.0], "content": "unassigned"}},
            {
                "command": "object.gradient",
                "params": {
                    "ids": "$0.id",
                    "kind": "linear",
                    "from": [72.0, 120.0],
                    "to": [240.0, 220.0],
                    "stops": [{"color": "#0F172A", "location": 0.0}, {"color": "#38BDF8", "location": 100.0}]
                }
            },
            {"command": "object.gradientFeather", "params": {"ids": ["$0.id"], "from": [72.0, 120.0], "to": [240.0, 220.0]}},
            {"command": "object.arrange", "params": {"ids": ["$0.id"], "to": "front"}},
            {"command": "object.align", "params": {"ids": ["$0.id"], "align": "center", "to": "margins"}},
            {"command": "frame.create", "params": {"spread": 1, "rect": [72.0, 80.0, 152.0, 160.0], "content": "unassigned"}},
            {"command": "object.qrCode", "params": {"type": "email", "to": "hello@example.org", "subject": "Check", "body": "OK"}},
            {"command": "transform.set", "params": {"ids": ["$last.id"], "width": 84.0, "height": 84.0}},
            {"command": "edit.stepAndRepeat", "params": {"ids": ["$5.id"], "count": 1, "dx": 110.0, "dy": 0.0}},
            {"command": "object.opacity", "params": {"ids": ["$8.ids"], "opacity": 92.0}}
        ]
    }))
    .unwrap();

    let _ = crate::native_layout::prepare(&mut plan, session.doc().unwrap(), &session.prefs, &cancel).unwrap();
    assert_eq!(plan.commands[1].params["ids"], json!(["$0.id"]));
    assert_eq!(plan.commands[4].params["edge"], json!("hcenter"));
    assert_eq!(plan.commands[9].params["ids"], json!("$8.ids"));

    validate_batch_plan(&plan, BatchScope { start: 0, count: 2 }).unwrap();
    simulate_group_scoped_with_ppi(
        std::slice::from_ref(&plan),
        session.doc().unwrap(),
        &session.prefs,
        OutputMode::NativeOnly,
        &cancel,
        BatchScope { start: 0, count: 2 },
        Some(300.0),
    )
    .unwrap();

    // Raw nested ItemId[] inside [...] or referencing null object.ungroup.ids must fail validation.
    let nested_err = validate_identifier_references(&[
        EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [72.0, 72.0, 140.0, 140.0]}) },
        EngineAction { command: "edit.stepAndRepeat".into(), params: json!({"ids": ["$0.id"], "count": 1, "dx": 80.0, "dy": 0.0}) },
        EngineAction { command: "object.opacity".into(), params: json!({"ids": ["$1.ids"], "opacity": 80.0}) },
    ])
    .unwrap_err();
    assert!(nested_err.contains("already an ItemId[] array"), "nested_err={nested_err}");

    let ungroup_err = validate_identifier_references(&[
        EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [72.0, 72.0, 120.0, 120.0]}) },
        EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [130.0, 72.0, 180.0, 120.0]}) },
        EngineAction { command: "object.group".into(), params: json!({"ids": ["$0.id", "$1.id"]}) },
        EngineAction { command: "object.ungroup".into(), params: json!({"ids": ["$2.id"]}) },
        EngineAction { command: "object.opacity".into(), params: json!({"ids": "$3.ids", "opacity": 80.0}) },
    ])
    .unwrap_err();
    assert!(ungroup_err.contains("object.ungroup"), "ungroup_err={ungroup_err}");
}

#[test]
fn editorial_vocalization_policy_scopes_script_tanween_honors_removal_and_remaps_utf8_selections() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();

    // Global instruction contains tanween ("أولاً", "كتاباً", "تعليمياً") plus "بدون تشكيل",
    // while the PAGE 1 copy is unvocalized. Hallucinated tashkeel must be stripped AND
    // subsequent UTF-8 byte offsets in text.select must remap cleanly to the second word!
    let script_prompt = "أولاً: صمّم كتاباً تعليمياً واضحاً بدون تشكيل.\n# PAGE 1 — الغلاف\nTITLE_AR: مرحبا بالعالم\n";
    let vocalized = "مَرْحَبًا بِالعَالَمِ";
    let word_start = vocalized.find("بِالعَالَمِ").unwrap();
    let word_end = vocalized.len();
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Unvocalized script with styled second word",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 72.0, 420.0, 160.0], "content": "text", "text": vocalized}},
            {"command": "text.select", "params": {"story": "$0.story", "anchor": word_start, "focus": word_end}},
            {"command": "type.char", "params": {"attrs": {"size": 22.0}}}
        ]
    }))
    .unwrap();

    prepare_editorial_plan(&mut plan, script_prompt, &json!({}), Some(BatchScope { start: 0, count: 1 })).unwrap();
    let stripped = plan.commands[0].params["text"].as_str().unwrap().to_string();
    assert_eq!(stripped, "مرحبا بالعالم");
    let new_anchor = plan.commands[1].params["anchor"].as_u64().unwrap() as usize;
    let new_focus = plan.commands[1].params["focus"].as_u64().unwrap() as usize;
    assert_eq!(&stripped[new_anchor..new_focus], "بالعالم");
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only"})).unwrap();
    let story = session.doc().unwrap().doc.stories.values().next().unwrap();
    assert_eq!(story.text, "مرحبا بالعالم");
    assert_eq!(story.format_after(0).over.size, None);
    assert_eq!(story.format_after(new_anchor.saturating_add(2)).over.size, Some(22.0));

    // Explicit removal ("أزل التشكيل") must strip tashkeel even when the opened-document context had vowels.
    let mut removal_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Explicit diacritic removal",
        "images": [],
        "commands": [
            {"command": "story.setText", "params": {"story": 1, "text": "عُنْوَانٌ جَدِيدٌ"}}
        ]
    }))
    .unwrap();
    prepare_editorial_plan(&mut removal_plan, "أزل التشكيل من العنوان واجعله بسيطاً", &json!({"existingText": "عُنْوَانٌ قَدِيمٌ"}), None).unwrap();
    assert_eq!(removal_plan.commands[0].params["text"], json!("عنوان جديد"));
}

#[test]
fn template_content_paid_repair_loop_recovers_slot_overset_only_when_opted_in() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();

    for allow_repairs in [false, true] {
        let skill = text_fit_style();
        let prompts_seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        struct RepairingMock<'a> {
            prompts: &'a RefCell<Vec<String>>,
        }
        impl Provider for RepairingMock<'_> {
            fn generate(&self, _: &ModelSelection, _: Option<&str>, req: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
                let mut seen = self.prompts.borrow_mut();
                seen.push(req.prompt.clone());
                let text = if seen.len() == 1 { "W".repeat(400) } else { "Write clearly.".to_owned() };
                Ok(GenerationResponse {
                    text: json!({
                        "title": "Rule",
                        "pages": [{"page": 0, "family": "practice", "text": {"grammar-rule-1": text}, "illustrations": []}]
                    })
                    .to_string(),
                    usage: Usage { input_tokens: Some(40), output_tokens: Some(25), ..Default::default() },
                    error: None,
                })
            }
            fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
                Err("No images".into())
            }
            fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
                Err("No discovery".into())
            }
        }

        let settings = AiSettings {
            allow_paid_repairs: allow_repairs,
            budget: BudgetSpec { token_limit: 500_000, max_requests: 8, repair_attempts: 2, ..Default::default() },
            ..Default::default()
        };
        let request = TaskRequest {
            prompt: "Create one clean grammar rule page".into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 1 },
            export_directory: None,
        };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let mut ledger = UsageLedger::new(settings.budget).unwrap();
        let (sender, receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let provider = RepairingMock { prompts: &prompts_seen };
        let result = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &provider,
        }
        .production(request, json!({}), catalog(), vec![skill], session.doc().unwrap().clone(), session.prefs.clone());

        if !allow_repairs {
            let err = result.err().expect("overset must fail when allow_paid_repairs is false");
            assert!(err.contains("overset"), "err={err}");
            assert_eq!(ledger.summary().requests, 1);
            assert_eq!(prompts_seen.borrow().len(), 1);
        } else {
            let Product::Plan(plan) = result.expect("opt-in paid repair should recover overset template copy") else {
                panic!("Expected Product::Plan");
            };
            assert_eq!(plan.commands[0].params["text"], "Write clearly.");
            assert_eq!(ledger.summary().requests, 2);
            let captured = prompts_seen.borrow();
            assert_eq!(captured.len(), 2);
            assert!(captured[1].contains("rejected_template_content") && captured[1].contains("validation_error"));
            assert!(captured[1].contains("overset"));
            let diagnostics: Vec<_> = receiver.try_iter().collect();
            assert!(!diagnostics.is_empty());
        }
    }
}

#[test]
fn gemini_discovery_excludes_non_generate_content_models_while_preserving_comfyui_and_unannotated_entries() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    struct DiscoveryMock;
    impl Provider for DiscoveryMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("No generation".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("No images".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Err("model_infos must be used by Work::Models".into())
        }
        fn model_infos(&self, selection: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<providers::ModelInfo>, String> {
            match selection.provider {
                ProviderKind::Gemini => Ok(vec![
                    providers::ModelInfo { id: "gemini-3.8-flash".into(), supports_generate_content: Some(true) },
                    providers::ModelInfo { id: "gemini-3.1-flash-image".into(), supports_generate_content: Some(true) },
                    providers::ModelInfo { id: "gemini-tuned-embed-only".into(), supports_generate_content: Some(false) },
                    providers::ModelInfo { id: "imagen-3.0-generate-002".into(), supports_generate_content: Some(false) },
                    providers::ModelInfo { id: "gemini-proxy-unannotated".into(), supports_generate_content: None },
                ]),
                ProviderKind::ComfyUi => {
                    Ok(vec![providers::ModelInfo { id: "sdxl_base_1.0.safetensors".into(), supports_generate_content: Some(false) }])
                }
                _ => Ok(vec![providers::ModelInfo { id: "gpt-4.1-mini".into(), supports_generate_content: None }]),
            }
        }
    }

    let run_discovery = |provider: ProviderKind| -> Vec<String> {
        let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let mock = DiscoveryMock;
        let outcome = Runner {
            cache_root: None,
            print: None,
            planning_only: false,
            ledger: &mut ledger,
            keys: &[],
            cancel: &cancel,
            sender: &sender,
            provider: &mock,
        }
        .run(Work::Models(ModelSelection { provider, model: "probe".into(), endpoint: None }))
        .expect("discovery should succeed");
        let Product::Models(models) = outcome else {
            panic!("Expected Product::Models");
        };
        models
    };

    assert_eq!(
        run_discovery(ProviderKind::Gemini),
        vec!["gemini-3.8-flash".to_owned(), "gemini-3.1-flash-image".to_owned(), "gemini-proxy-unannotated".to_owned()]
    );
    assert_eq!(run_discovery(ProviderKind::ComfyUi), vec!["sdxl_base_1.0.safetensors".to_owned()]);
    assert_eq!(run_discovery(ProviderKind::OpenAi), vec!["gpt-4.1-mini".to_owned()]);
}

fn assert_worker_sizing_and_thinking_margin_contracts() {
    // 1. Placed asset names preserve WebP, PNG, and JPEG extensions.
    assert_eq!(generated_asset_name("image/webp"), "generated.webp");
    assert_eq!(generated_asset_name("image/png"), "generated.png");
    assert_eq!(generated_asset_name("image/jpeg"), "generated.jpg");

    // 2. OpenAI and Gemini aspect-ratio sizing.
    let mk_place = |rect: [f64; 4]| designcraft_ai_types::protocol::ImagePlacement {
        family_id: None,
        slot_id: None,
        prompt: "hero".into(),
        page: 0,
        rect,
        label: "hero".into(),
        repeat_rects: vec![],
        repeat_groups: vec![],
    };
    let gpt_img = ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None };
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 300.0, 180.0]), None, &gpt_img).unwrap().as_deref(), Some("1536x1024"));
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 180.0, 300.0]), None, &gpt_img).unwrap().as_deref(), Some("1024x1536"));
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 200.0, 200.0]), None, &gpt_img).unwrap().as_deref(), Some("1024x1024"));

    let dalle3 = ModelSelection { provider: ProviderKind::OpenAi, model: "dall-e-3".into(), endpoint: None };
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 320.0, 180.0]), None, &dalle3).unwrap().as_deref(), Some("1792x1024"));
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 180.0, 320.0]), None, &dalle3).unwrap().as_deref(), Some("1024x1792"));
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 200.0, 200.0]), None, &dalle3).unwrap().as_deref(), Some("1024x1024"));

    let dalle2 = ModelSelection { provider: ProviderKind::OpenAi, model: "dall-e-2".into(), endpoint: None };
    assert_eq!(image_request_size(&mk_place([0.0, 0.0, 320.0, 180.0]), None, &dalle2).unwrap().as_deref(), Some("1024x1024"));

    assert_eq!(gemini_image_ratio("gemini-nano-banana-2.1", 4.0).0, "4:1");
    assert_eq!(gemini_image_ratio("gemini-3.1-flash-lite-image", 0.125).0, "1:8");
    assert_ne!(gemini_image_ratio("gemini-2.5-flash-image", 4.0).0, "4:1");

    // 3. Reasoning model thinking margin detection + template_content output allowance.
    for (provider, model, expected) in [
        (ProviderKind::Gemini, "gemini-4-argon", true),
        (ProviderKind::OpenAi, "gpt-5", true),
        (ProviderKind::OpenAi, "o3-mini", true),
        (ProviderKind::OpenAi, "gpt-4.1", false),
        (ProviderKind::Ollama, "qwen3:32b", true),
        (ProviderKind::Ollama, "library/deepseek-r1:14b", true),
        (ProviderKind::Ollama, "qwq:latest", true),
        (ProviderKind::Ollama, "llama3.3:70b", false),
    ] {
        assert_eq!(model_needs_thinking_margin(&ModelSelection { provider, model: model.into(), endpoint: None }), expected, "{model}");
    }
}

#[test]
fn ai28_worker_sizing_asset_names_thinking_margin_and_upfront_admission_guards() {
    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();

    assert_worker_sizing_and_thinking_margin_contracts();

    // 4. Upfront admission guards reject before spending Request #1.
    struct ZeroCallProvider {
        calls: std::cell::Cell<u32>,
    }
    impl Provider for ZeroCallProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err("Provider must not be called when upfront admission fails".into())
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err("Provider must not be called when upfront admission fails".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let mut session = Session::new();
    session.execute("file.new", &json!({"pages": 2, "width": 595.2756, "height": 841.8898})).unwrap();
    let base = session.doc().unwrap().clone();

    // 4a. Monetary limit with cloud image provider and missing image_rates fails with 0 calls.
    let mut settings = AiSettings {
        text_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-4.1".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None },
        output_mode: OutputMode::NativeWithImages,
        budget: BudgetSpec {
            max_cost_usd: Some(5.0),
            rates: Some(designcraft_ai_types::protocol::TokenRates {
                input_per_million: 2.0,
                cached_input_per_million: 0.5,
                output_per_million: 8.0,
                cache_write_per_million: None,
                image_per_request: None,
                image_reserve_usd: None,
            }),
            ..Default::default()
        },
        image_rates: None,
        ..Default::default()
    };
    let provider = ZeroCallProvider { calls: std::cell::Cell::new(0) };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let Err(err) = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(
        TaskRequest {
            prompt: "Design a 1-page poster.".into(),
            settings: settings.clone(),
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
            export_directory: None,
        },
        json!({}),
        designcraft_engine::cmd::production::catalog(),
        vec![],
        base.clone(),
        session.prefs.clone(),
    ) else {
        panic!("Expected upfront monetary image-rate rejection");
    };
    assert!(err.contains("rates"), "err={err}");
    assert_eq!(provider.calls.get(), 0);
    assert_eq!(ledger.summary().requests, 0);
    assert_eq!(gemini_image_ratio("MODELS/Gemini-3.1-Flash-Image", 0.25).0, "1:4");

    // 4a-ii. planning_only=true AND cached plan + cached disk illustration in NativeWithImages
    // with max_cost_usd=Some(5.0) and missing image_rates BOTH succeed without failing on missing image rates!
    struct SingleTextPlanProvider {
        plan_json: String,
        calls: std::cell::Cell<u32>,
    }
    impl Provider for SingleTextPlanProvider {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok(GenerationResponse { text: self.plan_json.clone(), usage: Usage::default(), error: None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("Image provider must not be called in planning_only or cached run".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }
    let cache_dir = tempfile::tempdir().unwrap();
    let placement = designcraft_ai_types::protocol::ImagePlacement {
        family_id: None,
        slot_id: None,
        page: 0,
        rect: [72.0, 120.0, 172.0, 220.0],
        prompt: "Clean geometric studio emblem".into(),
        label: "Hero".into(),
        repeat_rects: vec![],
        repeat_groups: vec![],
    };
    let planned_value = json!({
        "title": "Cached Poster",
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 260.0, 420.0, 340.0], "content": "text", "text": "Cached Headline"}}
        ],
        "images": [placement]
    });
    let text_provider = SingleTextPlanProvider { plan_json: planned_value.to_string(), calls: std::cell::Cell::new(0) };
    let poster_req = TaskRequest {
        prompt: "Design a 1-page poster.".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let cache_ctx = json!({"_production_cache_root": cache_dir.path().to_string_lossy()});
    let mut plan_ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let mut plan_runner = Runner {
        cache_root: Some(cache_dir.path().to_path_buf()),
        print: None,
        planning_only: true,
        ledger: &mut plan_ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &text_provider,
    };
    let preview_product = plan_runner
        .production(
            poster_req.clone(),
            cache_ctx.clone(),
            designcraft_engine::cmd::production::catalog(),
            vec![],
            base.clone(),
            session.prefs.clone(),
        )
        .expect("planning_only must succeed without image_rates");
    assert!(matches!(preview_product, Product::Plan(ref p) if p.images.len() == 1));
    assert_eq!(text_provider.calls.get(), 1);

    let mut png_bytes = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(300, 300, image::Rgb([220, 230, 240])).write_to(&mut png_bytes, image::ImageFormat::Png).unwrap();
    let cached_img = providers::GeneratedImage { bytes: png_bytes.into_inner(), filename: "cached.png".into(), mime_type: "image/png".into() };
    let style_ctx = illustration_style(None).unwrap();
    let disk_key = plan_runner.image_disk_key(&placement, &style_ctx, None, &settings).unwrap();
    crate::production_cache::store_image(cache_dir.path(), &disk_key, &cached_img).unwrap();

    let zero_provider = ZeroCallProvider { calls: std::cell::Cell::new(0) };
    let mut cached_ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let cached_product = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut cached_ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &zero_provider,
    }
    .production(poster_req, cache_ctx, designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone())
    .expect("cached plan + cached disk illustration must succeed with 0 provider calls");
    assert!(matches!(cached_product, Product::Plan(ref p) if p.images.is_empty() && p.commands.len() >= 3));
    assert_eq!(zero_provider.calls.get(), 0);
    assert_eq!(cached_ledger.summary().requests, 0);

    // 4b. Multi-chunk review group (2 pages, explicit script => capacity=1 => 2 chunks) with max_requests=1 fails with 0 calls.
    settings.output_mode = OutputMode::NativeOnly;
    settings.budget.max_cost_usd = None;
    settings.budget.max_requests = 1;
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let Err(err) = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    }
    .production(
        TaskRequest {
            prompt: "Trim Size: A4\n# PAGE 1: Cover\nHello\n# PAGE 2: Body\nWorld".into(),
            settings,
            skill_ids: vec![],
            auto_select_skills: false,
            target: designcraft_ai_types::protocol::TaskTarget::NewA4 { pages: 2 },
            export_directory: None,
        },
        json!({"batch_scope": {"start": 0, "count": 2}}),
        designcraft_engine::cmd::production::catalog(),
        vec![],
        base,
        session.prefs.clone(),
    ) else {
        panic!("Expected upfront request-limit rejection");
    };
    assert!(err.contains("AI request limit reached"), "err={err}");
    assert_eq!(provider.calls.get(), 0);
    assert_eq!(ledger.summary().requests, 0);
}

#[test]
fn wave3_schema_sync_rtl_slot_resolution_and_serde_path_diagnostics() {
    // 1. design_profile_schema is synced with DesignProfile limits and shared-atlas fields.
    let dp_schema = design_profile_schema();
    assert_eq!(dp_schema["properties"]["keywords"]["maxItems"], json!(48));
    let fam_props = &dp_schema["properties"]["page_families"]["items"]["properties"];
    assert_eq!(fam_props["text_slots"]["maxItems"], json!(24));
    let img_props = &fam_props["image_slots"]["items"]["properties"];
    assert!(img_props.get("crop").is_some());
    assert!(img_props.get("source_aspect_ratio").is_some());
    assert!(img_props.get("views").is_some());

    // 2. RTL-mirrored placement resolves declared_image_slot via explicit (family_id, slot_id).
    let mut profile = design_profile_fixture();
    profile.page_families[0].image_slots = vec![
        serde_json::from_value(json!({
            "id": "atlas",
            "purpose": "shared art",
            "rect": [60.0, 180.0, 160.0, 280.0],
            "crop": [0.0, 0.0, 0.5, 1.0],
            "source_aspect_ratio": 1.5,
            "views": [{"rect": [200.0, 180.0, 300.0, 280.0], "crop": [0.5, 0.0, 1.0, 1.0]}]
        }))
        .unwrap(),
    ];
    let page_w = profile.page_mm[0] * 72.0 / 25.4;
    let mirrored_primary = [page_w - 160.0, 180.0, page_w - 60.0, 280.0];
    let mirrored_view = [page_w - 300.0, 180.0, page_w - 200.0, 280.0];
    let rtl_placement = designcraft_ai_types::protocol::ImagePlacement {
        family_id: Some(profile.page_families[0].id.clone()),
        slot_id: Some("atlas".into()),
        repeat_rects: vec![],
        repeat_groups: vec![],
        prompt: "Mirrored two-part illustration".into(),
        page: 0,
        rect: mirrored_primary,
        label: "adapted purpose".into(),
    };
    let resolved = declared_image_slot(&rtl_placement, Some(&profile)).unwrap();
    assert_eq!(resolved.id, "atlas");
    let mapped_view = resolved_slot_view_rect(&rtl_placement, resolved, Some(&profile), resolved.views[0].rect);
    assert!((mapped_view[0] - mirrored_view[0]).abs() < 1e-6 && (mapped_view[2] - mirrored_view[2]).abs() < 1e-6);

    // 3. serde_path_to_error surfaces exact JSON paths across worker, image_review, and script_contract.
    let catalog = designcraft_engine::cmd::production::catalog();
    let bad_plan = json!({
        "title": "Bad Plan",
        "commands": [{"command": "frame.create", "params": {"rect": [0, 0, 100, 100]}}],
        "images": [{"prompt": "art", "page": 0, "rect": [10.0, 10.0, "not-a-number", 50.0], "label": "art"}]
    });
    let plan_err = parse_plan(&bad_plan.to_string(), OutputMode::NativeWithImages, &catalog).unwrap_err();
    assert!(plan_err.contains("images[0].rect[2]"), "plan_err={plan_err}");

    let review_err = crate::image_review::parse(r#"{"verdict":"pass","reasons":[123]}"#).unwrap_err();
    assert!(review_err.contains("reasons[0]"), "review_err={review_err}");

    let native_err = crate::script_contract::compile_native_content(
        r#"{"title":"T","commands":[{"command":99,"params":{}}]}"#,
        BatchScope { start: 0, count: 1 },
        None,
    )
    .unwrap_err();
    assert!(native_err.contains("commands[0].command"), "native_err={native_err}");
}

#[test]
fn local_comfy_image_releases_local_admission_before_cloud_review_runs() {
    struct HybridReviewProbe {
        held_during_images: AtomicBool,
        free_during_cloud_review: AtomicBool,
    }
    impl Provider for HybridReviewProbe {
        fn generate(&self, sel: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            assert_eq!(sel.provider, ProviderKind::Gemini);
            let probe = crate::resources::LocalRequestGuard::acquire(
                &ModelSelection { provider: ProviderKind::Ollama, model: "llama3.2:latest".into(), endpoint: None },
                &designcraft_ai_types::LocalResourcePolicy::default(),
            );
            self.free_during_cloud_review.store(probe.is_ok(), Ordering::SeqCst);
            drop(probe);
            Ok(GenerationResponse { text: r#"{"verdict":"pass","reasons":[]}"#.into(), usage: Usage::default(), error: None })
        }
        fn images(&self, sel: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            assert_eq!(sel.provider, ProviderKind::ComfyUi);
            let probe = crate::resources::LocalRequestGuard::acquire(
                &ModelSelection { provider: ProviderKind::Ollama, model: "llama3.2:latest".into(), endpoint: None },
                &designcraft_ai_types::LocalResourcePolicy::default(),
            );
            self.held_during_images.store(probe.is_err(), Ordering::SeqCst);
            let mut png = Vec::new();
            let img = image::RgbaImage::from_pixel(32, 32, image::Rgba([20, 80, 140, 255]));
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { bytes: png, filename: "probe.png".into(), mime_type: "image/png".into() }],
                usage: Usage::default(),
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _healthy = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let plan = ProductionPlan {
        title: "Hybrid Probe".into(),
        commands: vec![EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "content": "graphic", "rect": [72.0, 72.0, 172.0, 172.0]}),
        }],
        images: vec![designcraft_ai_types::protocol::ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: vec![],
            repeat_groups: vec![],
            prompt: "Blue square emblem".into(),
            page: 0,
            rect: [72.0, 72.0, 172.0, 172.0],
            label: "Emblem".into(),
        }],
    };
    let settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        verify_generated_images: true,
        text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::ComfyUi, model: "sdxl.safetensors".into(), endpoint: None },
        comfy_workflow: Some(json!({"1": {"class_type": "KSampler", "inputs": {"steps": 10}}})),
        ..Default::default()
    };
    let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
    let (sender, _rx) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let probe = HybridReviewProbe { held_during_images: AtomicBool::new(false), free_during_cloud_review: AtomicBool::new(false) };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &probe,
    };
    let mut cache = BTreeMap::new();
    let mut bytes_used = 0usize;
    runner.expand_images_with_geometry(plan, &settings, &mut cache, &mut bytes_used, None, None, Some(&base)).unwrap();
    assert!(probe.held_during_images.load(Ordering::SeqCst), "local admission must be active during ComfyUI image generation");
    assert!(probe.free_during_cloud_review.load(Ordering::SeqCst), "local admission must be released before cloud Gemini review starts");
}

#[test]
fn ai31_image_review_fail_retries_single_image_when_paid_repairs_allowed() {
    struct RetryReviewMock {
        images: std::cell::Cell<u32>,
        reviews: std::cell::Cell<u32>,
        second_prompt: std::cell::RefCell<String>,
    }
    impl Provider for RetryReviewMock {
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            let n = self.reviews.get() + 1;
            self.reviews.set(n);
            let text = if n == 1 {
                r#"{"verdict":"fail","reasons":["Child uses left hand instead of right hand"]}"#
            } else {
                r#"{"verdict":"pass","reasons":["Child clearly uses right hand"]}"#
            };
            Ok(GenerationResponse { text: text.into(), usage: Usage::default(), error: None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, req: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            let n = self.images.get() + 1;
            self.images.set(n);
            if n == 2 {
                *self.second_prompt.borrow_mut() = req.prompt.clone();
            }
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(400, 400, image::Rgba([30, 120, 200, 255])))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { filename: "retry.png".into(), mime_type: "image/png".into(), bytes: png }],
                usage: Usage::default(),
                error: None,
            })
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let directory = tempfile::tempdir().unwrap();
    let provider =
        RetryReviewMock { images: std::cell::Cell::new(0), reviews: std::cell::Cell::new(0), second_prompt: std::cell::RefCell::new(String::new()) };
    let mut settings = review_settings(8);
    settings.allow_paid_repairs = true;
    settings.budget.repair_attempts = 2;
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _receiver) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mut runner = Runner {
        cache_root: Some(directory.path().to_path_buf()),
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let mut cache = BTreeMap::new();
    let mut bytes = 0;
    let expanded = runner.expand_images(review_plan(), &settings, &mut cache, &mut bytes, None).unwrap();
    assert!(!expanded.commands.is_empty());
    assert_eq!(provider.images.get(), 2, "expected 1 corrective retry");
    assert_eq!(provider.reviews.get(), 2, "expected re-review after corrective retry");
    assert!(provider.second_prompt.borrow().contains("CRITICAL VISUAL CORRECTION REQUIRED"));
    assert_eq!(cache.len(), 1, "repaired pass artwork enters cache");
}

#[test]
fn agent_loop_multi_turn_inspects_and_applies_relational_document_edits_in_one_undo() {
    struct MultiTurnAgentMock {
        turn: std::cell::Cell<usize>,
    }
    impl Provider for MultiTurnAgentMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("unexpected single-shot generate".into())
        }
        fn supports_agent_loop(&self) -> bool {
            true
        }
        fn agent_turn(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            req: &providers::AgentTurnRequest,
            _: &AtomicBool,
        ) -> Result<providers::AgentTurnResponse, String> {
            let n = self.turn.get();
            self.turn.set(n + 1);
            match n {
                0 => {
                    assert!(matches!(req.messages.first(), Some(providers::AgentMessage::User { text, .. }) if text.contains("documentOverview")));
                    Ok(providers::AgentTurnResponse {
                        text: String::new(),
                        tool_calls: vec![
                            providers::ToolCall { id: "c1".into(), name: "inspect_pages".into(), arguments: json!({"start_page": 1, "end_page": 4}) },
                            providers::ToolCall {
                                id: "c2".into(), name: "search_document".into(), arguments: json!({"query": "السَّلَامُ"})
                            },
                        ],
                        raw_provider_turn: Some(json!({"parts": [{"thoughtSignature": "sig-1"}]})),
                        usage: Usage::default(),
                        error: None,
                    })
                }
                1 => {
                    assert_eq!(req.messages.len(), 3, "user + assistant(2 calls) + batched ToolResults");
                    assert!(matches!(req.messages.get(1), Some(providers::AgentMessage::Assistant { raw_provider_turn: Some(_), .. })));
                    Ok(providers::AgentTurnResponse {
                        text: String::new(),
                        tool_calls: vec![providers::ToolCall {
                            id: "c3".into(),
                            name: "apply_engine_commands".into(),
                            arguments: json!({
                                "summary": "Strip diacritics, upgrade Arabic font, and establish parent spread",
                                "commands": [
                                    {"command": "type.diacritics", "params": {"mode": "strip", "scope": "document"}},
                                    {"command": "font.replace", "params": {"script": "arabic", "toFamily": "auto", "redefineStyles": true}},
                                    {"command": "variables.define", "params": {"name": "RunningUnit", "type": "custom", "text": "الوحدة 1"}},
                                    {"command": "layout.parents.new", "params": {"prefix": "B", "name": "Unit Master"}},
                                    {"command": "layout.pages.applyParent", "params": {"pages": [1, 2], "parent": "B"}},
                                    {"command": "story.fitOverset", "params": {"minSize": 8.0}}
                                ]
                            }),
                        }],
                        raw_provider_turn: None,
                        usage: Usage::default(),
                        error: None,
                    })
                }
                _ => Ok(providers::AgentTurnResponse {
                    text: String::new(),
                    tool_calls: vec![providers::ToolCall {
                        id: "c4".into(),
                        name: "complete_task".into(),
                        arguments: json!({"title": "تحسين الكتاب", "summary": "تمت إزالة التشكيل وتحديث الخط والترقيم"}),
                    }],
                    raw_provider_turn: None,
                    usage: Usage::default(),
                    error: None,
                }),
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("unexpected image call".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":4,"facingPages":true,"binding":"rightToLeft"})).unwrap();
    if designcraft_fonts::FontDb::global().scoped(session.doc().unwrap().doc.font_scope).preferred_arabic_font().is_none() {
        return;
    }
    session
        .execute("frame.create", &json!({"spread":0,"rect":[80.0,80.0,450.0,160.0],"content":"text","text":"السَّلَامُ عَلَيْكُمْ English 123","caret":false}))
        .unwrap();
    let undo_before = session.doc().unwrap().history.undo.len();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings {
        text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None },
        ..Default::default()
    };
    let req = TaskRequest {
        prompt: "أزل التشكيل من جميع النصوص وحدث الخط العربي وأنشئ صفحة رئيسية للوحدة".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (sender, _rx) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let provider = MultiTurnAgentMock { turn: std::cell::Cell::new(0) };
    let mut runner = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger,
        keys: &[],
        cancel: &cancel,
        sender: &sender,
        provider: &provider,
    };
    let product = runner.production(req, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base, session.prefs.clone()).unwrap();
    let Product::Agentic { title, chunks, deferred_images, .. } = product else {
        panic!("expected Product::Agentic");
    };
    assert_eq!(title, "تحسين الكتاب");
    assert!(deferred_images.is_empty());
    assert_eq!(chunks.len(), 1);
    session
        .execute("production.apply", &json!({"commands": chunks[0], "outputMode": "native_only", "label": title, "requirePreflight": true}))
        .unwrap();
    assert_eq!(session.doc().unwrap().history.undo.len(), undo_before + 1, "must commit in 1 Undo step");
    let (_, story) = session.doc().unwrap().doc.stories.iter().next().unwrap();
    assert_eq!(story.text, "السلام عليكم English 123");
}

#[test]
fn agent_loop_gates_paid_cloud_images_unless_explicitly_requested_or_free_comfyui() {
    struct ImageQueueAgentMock {
        turn: std::cell::Cell<usize>,
        image_calls: std::cell::Cell<usize>,
    }
    impl Provider for ImageQueueAgentMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("unexpected single-shot generate".into())
        }
        fn supports_agent_loop(&self) -> bool {
            true
        }
        fn agent_turn(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            _: &providers::AgentTurnRequest,
            _: &AtomicBool,
        ) -> Result<providers::AgentTurnResponse, String> {
            let n = self.turn.get();
            self.turn.set(n + 1);
            if n == 0 {
                Ok(providers::AgentTurnResponse {
                    text: String::new(),
                    tool_calls: vec![
                        providers::ToolCall {
                            id: "t1".into(),
                            name: "apply_engine_commands".into(),
                            arguments: json!({
                                "commands": [
                                    {"command": "type.diacritics", "params": {"mode": "strip", "scope": "document"}}
                                ]
                            }),
                        },
                        providers::ToolCall {
                            id: "t2".into(),
                            name: "queue_image_generations".into(),
                            arguments: json!({
                                "summary": "Split composite strip into 2 clean animal cards",
                                "companion_commands": [
                                    {"command": "frame.create", "params": {"spread": 0, "rect": [80.0, 220.0, 220.0, 360.0], "content": "graphic"}},
                                    {"command": "frame.create", "params": {"spread": 0, "rect": [260.0, 220.0, 400.0, 360.0], "content": "graphic"}}
                                ],
                                "images": [
                                    {"spread": 0, "rect": [80.0, 220.0, 220.0, 360.0], "prompt": "Isolated friendly lion cub illustration, white background, no text", "label": "أسد"},
                                    {"spread": 0, "rect": [260.0, 220.0, 400.0, 360.0], "prompt": "Isolated friendly rabbit illustration, white background, no text", "label": "أرنب"}
                                ]
                            }),
                        },
                    ],
                    raw_provider_turn: None,
                    usage: Usage::default(),
                    error: None,
                })
            } else {
                Ok(providers::AgentTurnResponse {
                    text: String::new(),
                    tool_calls: vec![providers::ToolCall {
                        id: "t3".into(),
                        name: "complete_task".into(),
                        arguments: json!({"title": "مراجعة البطاقات", "summary": "تم تحديث النص وجدولة الصور"}),
                    }],
                    raw_provider_turn: None,
                    usage: Usage::default(),
                    error: None,
                })
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.image_calls.set(self.image_calls.get() + 1);
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(512, 512, image::Rgba([40, 140, 220, 255])))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { filename: "card.png".into(), mime_type: "image/png".into(), bytes: png }],
                usage: Usage::default(),
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
    if designcraft_fonts::FontDb::global().scoped(session.doc().unwrap().doc.font_scope).preferred_arabic_font().is_none() {
        return;
    }
    session.execute("frame.create", &json!({"spread":0,"rect":[80.0,80.0,450.0,160.0],"content":"text","text":"أَسَدٌ وَأَرْنَبٌ","caret":false})).unwrap();
    session.execute("font.replace", &json!({"script":"arabic","toFamily":"auto","redefineStyles":true})).unwrap();
    let base = session.doc().unwrap().clone();

    // Case 1: Paid Gemini image model + prompt does NOT explicitly request image regeneration -> deferred for confirmation, 0 image calls!
    let paid_settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 12, ..Default::default() },
        ..Default::default()
    };
    let implicit_req = TaskRequest {
        prompt: "أزل التشكيل وتأكد من تطابق الكلمات والبطاقات".into(),
        settings: paid_settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger1 = UsageLedger::new(paid_settings.budget.clone()).unwrap();
    let (tx1, _rx1) = mpsc::channel();
    let cancel1 = AtomicBool::new(false);
    let mock1 = ImageQueueAgentMock { turn: std::cell::Cell::new(0), image_calls: std::cell::Cell::new(0) };
    let mut runner1 = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger1,
        keys: &[],
        cancel: &cancel1,
        sender: &tx1,
        provider: &mock1,
    };
    let prod1 = runner1
        .production(implicit_req, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone())
        .unwrap();
    let Product::Agentic { chunks: chunks1, deferred_images: def1, .. } = prod1 else { panic!("expected Agentic") };
    assert_eq!(mock1.image_calls.get(), 0, "paid cloud image quota must not be spent without explicit prompt or confirmation");
    assert_eq!(chunks1.len(), 1);
    assert_eq!(def1.len(), 1);
    assert_eq!(def1[0].images.len(), 2);

    // Case 2: Paid Gemini image model + prompt EXPLICITLY asks to regenerate damaged images -> auto-executed in 1 Undo!
    let explicit_req = TaskRequest {
        prompt: "أزل التشكيل وأعد توليد الصور المعطوبة لكل كلمة".into(),
        settings: paid_settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger2 = UsageLedger::new(paid_settings.budget.clone()).unwrap();
    let (tx2, _rx2) = mpsc::channel();
    let cancel2 = AtomicBool::new(false);
    let mock2 = ImageQueueAgentMock { turn: std::cell::Cell::new(0), image_calls: std::cell::Cell::new(0) };
    let mut runner2 = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger2,
        keys: &[],
        cancel: &cancel2,
        sender: &tx2,
        provider: &mock2,
    };
    let prod2 = runner2
        .production(explicit_req.clone(), json!({}), designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone())
        .unwrap();
    let Product::Agentic { chunks: chunks2, deferred_images: def2, .. } = prod2 else { panic!("expected Agentic") };
    assert_eq!(mock2.image_calls.get(), 2, "explicit image regeneration prompt executes immediately");
    assert!(def2.is_empty());
    assert_eq!(chunks2.len(), 2, "text chunk + expanded image chunk ready for single-Undo applyGroup");

    // Case 3: When allow_image_queue is false (no valid image model configured), queue_image_generations is rejected by DocToolWorkspace.
    let mut ws_disallowed = crate::doc_tools::DocToolWorkspace::new(&base, &session.prefs, designcraft_engine::cmd::production::catalog(), 150.0);
    ws_disallowed.allow_image_queue = false;
    let (res_err, is_err) = ws_disallowed.execute_tool(
        "queue_image_generations",
        &json!({"images":[{"spread":0,"rect":[80.0,220.0,240.0,380.0],"prompt":"lion"}]}),
        &explicit_req,
        4,
    );
    assert!(is_err, "queue_image_generations must fail when allow_image_queue is false: {res_err}");
    assert!(res_err["error"].as_str().unwrap_or_default().contains("No valid image generation model"));
}

#[test]
fn agent_loop_local_json_fallback_repairs_and_completes_without_tool_calls() {
    struct LocalJsonOllamaMock {
        turn: std::cell::Cell<usize>,
    }
    impl Provider for LocalJsonOllamaMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("unexpected single-shot generate".into())
        }
        fn supports_agent_loop(&self) -> bool {
            true
        }
        fn agent_turn(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            _: &providers::AgentTurnRequest,
            _: &AtomicBool,
        ) -> Result<providers::AgentTurnResponse, String> {
            let n = self.turn.get();
            self.turn.set(n + 1);
            let text = if n == 0 {
                r#"{"title":"Bad","commands":[{"command":"forbidden.cmd","params":{}}]}"#
            } else {
                r#"{"title":"Local Ollama Edit","commands":[{"command":"type.diacritics","params":{"mode":"strip","scope":"document"}}]}"#
            };
            Ok(providers::AgentTurnResponse { text: text.into(), tool_calls: vec![], raw_provider_turn: None, usage: Usage::default(), error: None })
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("no images".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    if designcraft_fonts::FontDb::global().scoped(session.doc().unwrap().doc.font_scope).preferred_arabic_font().is_none() {
        return;
    }
    session.execute("frame.create", &json!({"spread":0,"rect":[80.0,80.0,450.0,160.0],"content":"text","text":"مَرْحَبًا","caret":false})).unwrap();
    session.execute("font.replace", &json!({"script":"arabic","toFamily":"auto","redefineStyles":true})).unwrap();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings {
        text_model: ModelSelection { provider: ProviderKind::Ollama, model: "qwen2.5:14b".into(), endpoint: None },
        ..Default::default()
    };
    let req = TaskRequest {
        prompt: "Strip diacritics".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget.clone()).unwrap();
    let (tx, _rx) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mock = LocalJsonOllamaMock { turn: std::cell::Cell::new(0) };
    let mut runner =
        Runner { cache_root: None, print: None, planning_only: false, ledger: &mut ledger, keys: &[], cancel: &cancel, sender: &tx, provider: &mock };
    let prod = runner.production(req, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base, session.prefs.clone()).unwrap();
    let Product::Agentic { title, chunks, .. } = prod else { panic!("expected Agentic") };
    assert_eq!(title, "Local Ollama Edit");
    assert_eq!(chunks.len(), 1);
    assert_eq!(mock.turn.get(), 2, "repaired on second turn");
}

#[test]
fn open_doc_native_only_verify_generated_images_auto_executes_comfyui_and_preflights_multi_batch_quota() {
    struct MultiScenarioMock {
        turn: std::cell::Cell<usize>,
        image_calls: std::cell::Cell<usize>,
    }
    impl Provider for MultiScenarioMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("unexpected single-shot generate".into())
        }
        fn supports_agent_loop(&self) -> bool {
            true
        }
        fn agent_turn(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            _: &providers::AgentTurnRequest,
            _: &AtomicBool,
        ) -> Result<providers::AgentTurnResponse, String> {
            let n = self.turn.get();
            self.turn.set(n + 1);
            if n == 0 {
                Ok(providers::AgentTurnResponse {
                    text: String::new(),
                    tool_calls: vec![
                        providers::ToolCall {
                            id: "c1".into(),
                            name: "apply_engine_commands".into(),
                            arguments: json!({
                                "commands": [
                                    {"command": "type.diacritics", "params": {"mode": "none"}}
                                ]
                            }),
                        },
                        providers::ToolCall {
                            id: "c2".into(),
                            name: "queue_image_generations".into(),
                            arguments: json!({
                                "summary": "Regenerate 2 cards cleanly",
                                "companion_commands": [
                                    {"command": "frame.create", "params": {"spread": 0, "rect": [80.0, 220.0, 220.0, 360.0], "content": "graphic"}},
                                    {"command": "frame.create", "params": {"spread": 0, "rect": [260.0, 220.0, 400.0, 360.0], "content": "graphic"}}
                                ],
                                "images": [
                                    {"spread": 0, "rect": [80.0, 220.0, 220.0, 360.0], "prompt": "Single pomegranate illustration, white background, no text", "label": "رمان"},
                                    {"spread": 0, "rect": [260.0, 220.0, 400.0, 360.0], "prompt": "Single olive branch illustration, white background, no text", "label": "زيتون"}
                                ]
                            }),
                        },
                    ],
                    raw_provider_turn: None,
                    usage: Usage::default(),
                    error: None,
                })
            } else {
                Ok(providers::AgentTurnResponse {
                    text: String::new(),
                    tool_calls: vec![providers::ToolCall {
                        id: "c3".into(),
                        name: "complete_task".into(),
                        arguments: json!({"title": "ضبط الكتاب", "summary": "تم الضبط وتوليد الصور"}),
                    }],
                    raw_provider_turn: None,
                    usage: Usage::default(),
                    error: None,
                })
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            self.image_calls.set(self.image_calls.get() + 1);
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(512, 512, image::Rgba([60, 150, 90, 255])))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            Ok(ImageResponse {
                images: vec![providers::GeneratedImage { filename: "card.png".into(), mime_type: "image/png".into(), bytes: png }],
                usage: Usage::default(),
                error: None,
            })
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
    if designcraft_fonts::FontDb::global().scoped(session.doc().unwrap().doc.font_scope).preferred_arabic_font().is_none() {
        return;
    }
    session.execute("frame.create", &json!({"spread":0,"rect":[80.0,80.0,450.0,160.0],"content":"text","text":"رُمَّانٌ وَزَيْتُونٌ","caret":false})).unwrap();
    // Also place a running header on parent spread 0 and verify doc_tools inspects markers and searches parent spreads:
    let parent_frame = session
        .execute(
            "frame.create",
            &json!({
                "spread": {"kind": "parent", "index": 0},
                "rect": [72.0, 36.0, 320.0, 60.0],
                "content": "text",
                "text": format!("الوحدة الأولى · {}", designcraft_engine::doc::story::PAGE_NUMBER),
                "caret": false
            }),
        )
        .unwrap();
    let parent_frame_id = parent_frame.get("id").and_then(Value::as_u64).unwrap();
    session.execute("font.replace", &json!({"script":"arabic","toFamily":"auto","redefineStyles":true})).unwrap();
    let base = session.doc().unwrap().clone();

    // Check doc_tools overview specialMarkers & parent-spread search location:
    let mut ws = crate::doc_tools::DocToolWorkspace::new(&base, &session.prefs, designcraft_engine::cmd::production::catalog(), 150.0);
    let dummy_req = TaskRequest {
        prompt: "افحص الكتاب".into(),
        settings: AiSettings::default(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let ov = ws.inspect_document_overview(&dummy_req).unwrap();
    assert_eq!(ov["specialMarkers"]["currentPageNumber"].as_str(), Some("\u{E000}"));
    let (search_res, is_err) = ws.execute_tool("search_document", &json!({"query": "الوحدة الأولى"}), &dummy_req, 4);
    assert!(!is_err);
    assert_eq!(search_res["matches"][0]["parentSpread"].as_u64(), Some(0));
    assert_eq!(search_res["matches"][0]["frameId"].as_u64(), Some(parent_frame_id));

    // 1) Local Ollama + ComfyUI under OutputMode::NativeOnly + verify_generated_images: true + tight max_requests: 2
    // Must NOT fail on Gemini-only review and must auto-execute free local ComfyUI images even after exhausting 2 text turns!
    let local_settings = AiSettings {
        output_mode: OutputMode::NativeOnly,
        verify_generated_images: true,
        text_model: ModelSelection { provider: ProviderKind::Ollama, model: "qwen3:14b".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: None },
        comfy_workflow: Some(json!({"1": {"class_type": "KSampler", "inputs": {"text": "{{PROMPT}}"}}})),
        budget: BudgetSpec { token_limit: 500_000, max_requests: 2, ..Default::default() },
        ..Default::default()
    };
    let local_req = TaskRequest {
        prompt: "راجع الكتاب واضبط التشكيل والبطاقات".into(),
        settings: local_settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger_local = UsageLedger::new(local_settings.budget.clone()).unwrap();
    let (tx_l, _rx_l) = mpsc::channel();
    let cancel_l = AtomicBool::new(false);
    let mock_local = MultiScenarioMock { turn: std::cell::Cell::new(0), image_calls: std::cell::Cell::new(0) };
    let mut runner_local = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger_local,
        keys: &[],
        cancel: &cancel_l,
        sender: &tx_l,
        provider: &mock_local,
    };
    let prod_local = runner_local
        .production(local_req, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone())
        .unwrap();
    let Product::Agentic { chunks: local_chunks, deferred_images: local_def, .. } = prod_local else { panic!("expected Agentic") };
    assert_eq!(mock_local.image_calls.get(), 2, "free local ComfyUI auto-executes under NativeOnly");
    assert!(local_def.is_empty());
    assert_eq!(local_chunks.len(), 2);

    // 2) OpenAI under OutputMode::NativeOnly + verify_generated_images: true + explicit image regeneration prompt -> auto-executes cleanly!
    let openai_settings = AiSettings {
        output_mode: OutputMode::NativeOnly,
        verify_generated_images: true,
        text_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 8, ..Default::default() },
        ..Default::default()
    };
    let openai_req = TaskRequest {
        prompt: "أزل التشكيل وأعد توليد الصور المعطوبة".into(),
        settings: openai_settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger_oa = UsageLedger::new(openai_settings.budget.clone()).unwrap();
    let (tx_oa, _rx_oa) = mpsc::channel();
    let cancel_oa = AtomicBool::new(false);
    let mock_oa = MultiScenarioMock { turn: std::cell::Cell::new(0), image_calls: std::cell::Cell::new(0) };
    let mut runner_oa = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger_oa,
        keys: &[],
        cancel: &cancel_oa,
        sender: &tx_oa,
        provider: &mock_oa,
    };
    let prod_oa = runner_oa
        .production(openai_req, json!({}), designcraft_engine::cmd::production::catalog(), vec![], base.clone(), session.prefs.clone())
        .unwrap();
    let Product::Agentic { chunks: oa_chunks, deferred_images: oa_def, .. } = prod_oa else { panic!("expected Agentic") };
    assert_eq!(mock_oa.image_calls.get(), 2, "explicit image prompt under NativeOnly + OpenAI auto-executes");
    assert!(oa_def.is_empty());
    assert_eq!(oa_chunks.len(), 2);

    // 3) Multi-batch deferred image preflight: if batch 0 + batch 1 exceed max_cost_usd, ZERO images in batch 0 are purchased!
    let capped_settings = AiSettings {
        output_mode: OutputMode::NativeWithImages,
        verify_generated_images: false,
        text_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None },
        image_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None },
        image_rates: Some(designcraft_ai_types::protocol::TokenRates {
            input_per_million: 0.0,
            cached_input_per_million: 0.0,
            output_per_million: 0.0,
            cache_write_per_million: None,
            image_per_request: Some(0.04),
            image_reserve_usd: None,
        }),
        budget: BudgetSpec {
            token_limit: 500_000,
            max_requests: 16,
            max_cost_usd: Some(0.06), // admits 1 image ($0.04), NOT 2 images across 2 batches ($0.08)!
            rates: Some(designcraft_ai_types::protocol::TokenRates {
                input_per_million: 1.0,
                cached_input_per_million: 0.0,
                output_per_million: 1.0,
                cache_write_per_million: None,
                image_per_request: None,
                image_reserve_usd: None,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let two_batches = vec![
        crate::doc_tools::DeferredImagePlan {
            companion_commands: vec![EngineAction {
                command: "frame.create".into(),
                params: json!({"spread": 0, "rect": [80.0, 220.0, 220.0, 360.0], "content": "graphic"}),
            }],
            images: vec![ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: vec![],
                repeat_groups: vec![],
                prompt: "First batch paid card".into(),
                page: 0,
                rect: [80.0, 220.0, 220.0, 360.0],
                label: "Card 1".into(),
            }],
            summary: "Batch 1".into(),
        },
        crate::doc_tools::DeferredImagePlan {
            companion_commands: vec![EngineAction {
                command: "frame.create".into(),
                params: json!({"spread": 1, "rect": [80.0, 220.0, 220.0, 360.0], "content": "graphic"}),
            }],
            images: vec![ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: vec![],
                repeat_groups: vec![],
                prompt: "Second batch paid card".into(),
                page: 1,
                rect: [80.0, 220.0, 220.0, 360.0],
                label: "Card 2".into(),
            }],
            summary: "Batch 2".into(),
        },
    ];
    let mut ledger_cap = UsageLedger::new(capped_settings.budget.clone()).unwrap();
    let (tx_c, _rx_c) = mpsc::channel();
    let cancel_c = AtomicBool::new(false);
    let mock_cap = MultiScenarioMock { turn: std::cell::Cell::new(0), image_calls: std::cell::Cell::new(0) };
    let mut runner_cap = Runner {
        cache_root: None,
        print: None,
        planning_only: false,
        ledger: &mut ledger_cap,
        keys: &[],
        cancel: &cancel_c,
        sender: &tx_c,
        provider: &mock_cap,
    };
    let mut scratch = Session::new();
    scratch.prefs = session.prefs.clone();
    scratch.add_document(base);
    let err = runner_cap.expand_deferred_image_batches("Two Batches", &two_batches, &capped_settings, None, &mut scratch, 150.0);
    assert!(err.is_err(), "combined 2-batch cost ($0.08) must fail $0.06 cap upfront");
    assert_eq!(mock_cap.image_calls.get(), 0, "batch 0 must not spend paid quota when batch 1 exceeds budget");
}

#[test]
fn agent_loop_render_visual_check_multimodal_and_turn_pruning() {
    struct VisionLoopMock {
        turn: std::cell::Cell<usize>,
    }
    impl Provider for VisionLoopMock {
        fn generate(&self, _: &ModelSelection, _: Option<&str>, _: &GenerationRequest, _: &AtomicBool) -> Result<GenerationResponse, String> {
            Err("unexpected single-shot generate".into())
        }
        fn supports_agent_loop(&self) -> bool {
            true
        }
        fn agent_turn(
            &self,
            _: &ModelSelection,
            _: Option<&str>,
            req: &providers::AgentTurnRequest,
            _: &AtomicBool,
        ) -> Result<providers::AgentTurnResponse, String> {
            let t = self.turn.get();
            self.turn.set(t + 1);
            match t {
                0 => {
                    assert!(
                        req.tools.iter().any(|tool| tool.name == "render_visual_check"),
                        "vision-capable model must receive render_visual_check tool"
                    );
                    Ok(providers::AgentTurnResponse {
                        text: String::new(),
                        tool_calls: vec![providers::ToolCall {
                            id: "vc1".into(),
                            name: "render_visual_check".into(),
                            arguments: json!({"pages": [1, 2], "scale": 1.0}),
                        }],
                        raw_provider_turn: None,
                        usage: Usage::default(),
                        error: None,
                    })
                }
                1 => {
                    let tool_msgs: Vec<_> = req
                        .messages
                        .iter()
                        .filter_map(|m| match m {
                            providers::AgentMessage::ToolResults { results } => Some(results),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(tool_msgs.len(), 1);
                    assert_eq!(tool_msgs[0][0].images.len(), 2, "latest render_visual_check must attach 2 PNGs");
                    for img in &tool_msgs[0][0].images {
                        assert_eq!(img.mime_type, "image/png");
                        assert!(img.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
                    }
                    Ok(providers::AgentTurnResponse {
                        text: String::new(),
                        tool_calls: vec![
                            providers::ToolCall {
                                id: "cmd1".into(),
                                name: "apply_engine_commands".into(),
                                arguments: json!({
                                    "summary": "Add atomic captioned graphic card",
                                    "commands": [
                                        {"command": "frame.create", "params": {"spread": 1, "rect": [72.0, 200.0, 220.0, 320.0], "content": "graphic"}},
                                        {"command": "object.altText", "params": {"ids": ["$0.id"], "text": "بطاقة الفهم البصري"}},
                                        {"command": "object.caption", "params": {"ids": ["$0.id"], "text": "{altText}", "live": true, "group": true}},
                                        {"command": "font.replace", "params": {"script": "arabic", "toFamily": "auto", "redefineStyles": true}}
                                    ]
                                }),
                            },
                            providers::ToolCall {
                                id: "vc2".into(),
                                name: "render_visual_check".into(),
                                arguments: json!({"pages": [2], "scale": 1.0}),
                            },
                        ],
                        raw_provider_turn: None,
                        usage: Usage::default(),
                        error: None,
                    })
                }
                2 => {
                    let tool_msgs: Vec<_> = req
                        .messages
                        .iter()
                        .filter_map(|m| match m {
                            providers::AgentMessage::ToolResults { results } => Some(results),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(tool_msgs.len(), 2);
                    assert!(tool_msgs[0][0].images.is_empty(), "prior turn PNG payloads must be pruned from history");
                    assert_eq!(tool_msgs[1][0].output["ok"], json!(true), "apply_engine_commands check");
                    assert_eq!(tool_msgs[1][1].images.len(), 1, "current turn PNG payload must remain attached");
                    assert!(tool_msgs[1][1].images[0].bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
                    Ok(providers::AgentTurnResponse {
                        text: String::new(),
                        tool_calls: vec![providers::ToolCall {
                            id: "done".into(),
                            name: "complete_task".into(),
                            arguments: json!({"title": "Visual Check Verified", "summary": "Rendered, captioned, and verified"}),
                        }],
                        raw_provider_turn: None,
                        usage: Usage::default(),
                        error: None,
                    })
                }
                other => Err(format!("unexpected extra turn {other}")),
            }
        }
        fn images(&self, _: &ModelSelection, _: Option<&str>, _: &ImageRequest, _: &AtomicBool) -> Result<ImageResponse, String> {
            Err("unexpected image generation".into())
        }
        fn models(&self, _: &ModelSelection, _: Option<&str>, _: &AtomicBool) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    let _serial = LOCAL_TEST_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _memory = crate::resources::TestAdmissionGuard::healthy();
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 2, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let settings = AiSettings {
        output_mode: OutputMode::NativeOnly,
        text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None },
        budget: BudgetSpec { token_limit: 500_000, max_requests: 8, ..Default::default() },
        ..Default::default()
    };
    let req = TaskRequest {
        prompt: "تفقد الصفحتين بصرياً ثم أضف بطاقة مع تسمية حية مجمعة في الصفحة الثانية".into(),
        settings: settings.clone(),
        skill_ids: vec![],
        auto_select_skills: false,
        target: designcraft_ai_types::protocol::TaskTarget::CurrentDocument,
        export_directory: None,
    };
    let mut ledger = UsageLedger::new(settings.budget).unwrap();
    let (tx, _rx) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    let mock = VisionLoopMock { turn: std::cell::Cell::new(0) };
    let mut runner =
        Runner { cache_root: None, print: None, planning_only: false, ledger: &mut ledger, keys: &[], cancel: &cancel, sender: &tx, provider: &mock };
    let product = crate::agent_loop::run_open_document_agent_loop(
        &mut runner,
        &req,
        &json!({}),
        &designcraft_engine::cmd::production::catalog(),
        &[],
        &base,
        &session.prefs,
    )
    .expect("agent loop with render_visual_check and grouped live caption");
    let Product::Agentic { title, chunks, deferred_images, .. } = product else {
        panic!("expected Product::Agentic from open-document agent loop");
    };
    assert_eq!(title, "Visual Check Verified");
    assert!(deferred_images.is_empty());
    assert_eq!(mock.turn.get(), 3);
    let mut verify = Session::new();
    verify.prefs = session.prefs.clone();
    verify.add_document(base);
    let group_chunks: Vec<_> = chunks.iter().map(|c| json!({"commands": c, "label": "Agent step"})).collect();
    verify
        .execute("production.applyGroup", &json!({"chunks": group_chunks, "outputMode": "native_only", "requirePreflight": true}))
        .expect("apply agent loop chunks");
    let applied_doc = &verify.doc().unwrap().doc;
    let mut found_grouped_live_caption = false;
    for top in &applied_doc.spreads[1].items {
        if !top.children().is_empty() {
            top.walk(&mut |it| {
                if it.live_caption.is_some() {
                    found_grouped_live_caption = true;
                }
            });
        }
    }
    assert!(found_grouped_live_caption, "atomic grouped LiveCaption must exist on spread 1");
}
