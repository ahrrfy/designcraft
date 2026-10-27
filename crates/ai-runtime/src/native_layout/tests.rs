use super::*;

#[test]
fn ai24_formatting_range_recovery_is_utf8_exact_idempotent_and_preserves_editing_ranges() {
    let copy = "الأعداد ١٥ و١٦";
    let base: ProductionPlan = serde_json::from_value(json!({"title":"Arabic header","images":[],"commands":[
        {"command":"frame.create","params":{"spread":0,"content":"text","rect":[50,50,400,100],"text":copy}},
        {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":0}},
        {"command":"type.char","params":{"attrs":{"size":15}}}
    ]}))
    .unwrap();
    let mut recovered = base.clone();
    assert_eq!(format_populated_new_stories(&mut recovered).unwrap().len(), 1);
    assert_eq!(recovered.commands[1].params["focus"], copy.len());
    assert!(format_populated_new_stories(&mut recovered).unwrap().is_empty());
    assert_eq!(recovered.commands[0].params["text"], copy);
    for change in ["insert", "replace", "external", "midstory", "range", "prior_range"] {
        let mut plan = base.clone();
        match change {
            "insert" => plan.commands.push(EngineAction { command: "text.insert".into(), params: json!({"text":"A"}) }),
            "replace" => plan.commands.push(EngineAction { command: "story.setText".into(), params: json!({"story":"$0.story","text":"B"}) }),
            "external" => plan.commands[1].params["story"] = json!(123),
            "midstory" => {
                plan.commands[1].params["anchor"] = json!(2);
                plan.commands[1].params["focus"] = json!(2);
            }
            "prior_range" => {
                plan.commands.insert(1, EngineAction { command: "text.select".into(), params: json!({"story":"$0.story","anchor":0,"focus":2}) });
            }
            _ => plan.commands[1].params["focus"] = json!(2),
        }
        let before = serde_json::to_value(&plan).unwrap();
        assert!(format_populated_new_stories(&mut plan).unwrap().is_empty(), "{change}");
        assert_eq!(serde_json::to_value(&plan).unwrap(), before, "{change}");
    }
}

#[test]
fn ai24_new_story_caret_formatting_targets_existing_copy_without_changing_engine_caret_semantics() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Header formatting","images":[],"commands":[
        {"command":"frame.create","params":{"spread":0,"content":"text","rect":[54,76,280,110],"text":"Numbers 15 & 16: Ocean Treasure Counting"}},
        {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":0}},
        {"command":"type.char","params":{"attrs":{"size":9.5}}}
    ]}))
    .unwrap();
    let notes = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(plan.commands[1].params["focus"], plan.commands[0].params["text"].as_str().unwrap().len());
    assert!(notes.iter().any(|note| note["kind"] == "nativeFormattingRange"));
    let result = session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let story = session.doc().unwrap().doc.story(StoryId(result["results"][0]["story"].as_u64().unwrap())).unwrap();
    let doc = &session.doc().unwrap().doc;
    let (_, base) = doc.styles.resolve_para(&story.paras[0]);
    for byte in [0, story.text.len() - 1] {
        assert_eq!(doc.styles.resolve_char(&base, story.char_format_of(byte)).size, 9.5);
    }
    // The general engine retains insertion-only semantics at a real caret.
    session.execute("text.select", &json!({"story":story.id.0,"anchor":0,"focus":0})).unwrap();
    session.execute("type.char", &json!({"attrs":{"size":30}})).unwrap();
    let doc = &session.doc().unwrap().doc;
    let story = doc.story(StoryId(result["results"][0]["story"].as_u64().unwrap())).unwrap();
    assert_eq!(doc.styles.resolve_char(&base, story.char_format_of(0)).size, 9.5);
}

#[test]
fn ai24_each_repeated_cell_is_a_pre_image_margin_and_text_obstacle() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"pages":1,"facingPages":false})).unwrap();
    for (cell, expected) in [([220.0, 180.0, 260.0, 220.0], "overlap native text"), ([1.0, 180.0, 41.0, 220.0], "safe margin")] {
        let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Units","commands":[
                {"command":"frame.create","params":{"spread":0,"rect":[215,180,360,225],"content":"text","text":"Keep this copy"}}
            ],"images":[{"prompt":"One fish","page":0,"rect":[60,180,100,220],"label":"Fish","repeat_rects":[cell]}]}))
        .unwrap();
        let before = serde_json::to_value(&plan).unwrap();
        let error = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert_eq!(serde_json::to_value(&plan).unwrap(), before);
        assert!(session.doc().unwrap().doc.spreads[0].items.is_empty());
    }
}
use designcraft_engine::Session;
use serde_json::json;

fn fixture() -> (Session, ProductionPlan) {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false,"margins":42.52})).unwrap();
    let plan = serde_json::from_value(json!({"title":"Measured text", "images":[], "commands":[
            {"command":"frame.create","params":{"spread":0,"rect":[60,60,225,82],"content":"text","text":"Count the flowers carefully. Write each number. Compare your answer with your partner.","caret":false}},
            {"command":"type.char","params":{"attrs":{"size":12,"leading":{"kind":"points","value":17}}}},
            {"command":"frame.create","params":{"spread":0,"rect":[60,180,225,280],"content":"graphic"}}
        ]})).unwrap();
    (session, plan)
}

fn contained_growth_fixture() -> (Session, ProductionPlan, Rect) {
    let (session, mut plan) = fixture();
    plan.commands.insert(2, EngineAction { command: "object.textFrameOptions".into(), params: json!({"ids":["$0.id"],"inset":[2,3,2,3]}) });
    let mut measured = Session::new();
    measured.prefs = session.prefs.clone();
    measured.add_document(session.doc().unwrap().clone());
    let applied =
        measured.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
    let id = applied["results"][0]["id"].as_u64().unwrap();
    measured.execute("object.fit", &json!({"ids":[id],"mode":"fitFrameToContent","axis":"height"})).unwrap();
    let fitted = measured.doc().unwrap().doc.item(ItemId(id)).unwrap().bounds();
    let old_bottom = fitted.y1 - 4.4;
    plan.commands[0].params["rect"][3] = json!(old_bottom);
    let background = Rect::new(54.0, 54.0, 231.0, old_bottom + 1.0);
    plan.commands.insert(
        0,
        EngineAction {
            command: "frame.create".into(),
            params: json!({"spread":0,"content":"unassigned","shape":"rectangle","rect":[background.x0,background.y0,background.x1,background.y1]}),
        },
    );
    plan.commands[3].params["ids"] = json!(["$1.id"]);
    (session, plan, background)
}

#[test]
fn ai21_new_decorative_container_grows_with_tiny_text_reflow_and_keeps_padding_and_insets() {
    let (session, mut plan, old_background) = contained_growth_fixture();
    let authored = plan.clone();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|note| note["kind"] == "nativeBackgroundReflow"));
    assert_eq!(plan.commands[1].params["text"], authored.commands[1].params["text"]);
    assert_eq!(plan.commands[2].params, authored.commands[2].params, "Font size and leading must stay authored");
    assert_eq!(plan.commands[3].params, authored.commands[3].params, "Explicit text insets must never change");
    let mut proof = Session::new();
    proof.prefs = session.prefs.clone();
    proof.add_document(base.clone());
    let result = proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let doc = &proof.doc().unwrap().doc;
    let background = doc.item(ItemId(result["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
    let text = doc.item(ItemId(result["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!((background.y1 - text.y1 - 1.0).abs() < 0.02, "Bottom padding must survive local reflow");
    assert!(background.y1 > old_background.y1 && background.y1 - old_background.y1 < 5.0);
    assert_eq!((background.x0, background.y0, background.x1), (old_background.x0, old_background.y0, old_background.x1));
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn ai21_container_growth_refuses_real_and_inherited_obstacles_in_its_padding_band() {
    for inherited in [false, true] {
        let (mut session, mut plan, background) = contained_growth_fixture();
        let obstacle = json!({"spread":if inherited {json!({"kind":"parent","index":0})} else {json!(0)},
                "content":"graphic","rect":[54,background.y1+1.0,59,background.y1+3.0]});
        if inherited {
            session.execute("frame.create", &obstacle).unwrap();
        } else {
            plan.commands.push(EngineAction { command: "frame.create".into(), params: obstacle });
        }
        let authored = serde_json::to_value(&plan).unwrap();
        let base = session.doc().unwrap().clone();
        assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err());
        assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

#[test]
fn ai21_existing_inherited_graphic_and_nonrectangular_containers_remain_fixed() {
    for kind in ["existing", "inherited", "graphic", "ellipse"] {
        let (mut session, mut plan, _) = contained_growth_fixture();
        if kind == "existing" || kind == "inherited" {
            let mut background = plan.commands.remove(0).params;
            if kind == "inherited" {
                background["spread"] = json!({"kind":"parent","index":0});
            }
            session.execute("frame.create", &background).unwrap();
            plan.commands[2].params["ids"] = json!(["$0.id"]);
        } else if kind == "graphic" {
            plan.commands[0].params["content"] = json!("graphic");
        } else {
            plan.commands[0].params["shape"] = json!("ellipse");
        }
        let authored = serde_json::to_value(&plan).unwrap();
        let base = session.doc().unwrap().clone();
        assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must not be resized by local recovery");
        assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

fn fitted_footer_fixture() -> (Session, ProductionPlan, Rect) {
    let (session, mut plan, _) = contained_growth_fixture();
    plan.commands.remove(0);
    plan.commands.pop();
    plan.commands[2].params["ids"] = json!(["$0.id"]);
    let safe = session.doc().unwrap().doc.spreads[0].pages[0].margin_rect();
    let old_bottom = plan.commands[0].params["rect"][3].as_f64().unwrap();
    let dy = safe.y1 - old_bottom;
    plan.commands[0].params["rect"][1] = json!(60.0 + dy);
    plan.commands[0].params["rect"][3] = json!(safe.y1);
    (session, plan, Rect::new(60.0, 60.0 + dy, 225.0, safe.y1))
}

#[test]
fn ai21_tiny_overset_footer_moves_up_with_unchanged_copy_fonts_and_explicit_insets() {
    let (session, mut plan, original) = fitted_footer_fixture();
    let authored = plan.clone();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|note| note["kind"] == "nativeTextTranslation"));
    assert_eq!(plan.commands[0].params["text"], authored.commands[0].params["text"]);
    assert_eq!(plan.commands[1].params, authored.commands[1].params);
    assert_eq!(plan.commands[2].params, authored.commands[2].params);
    let mut proof = Session::new();
    proof.prefs = session.prefs.clone();
    proof.add_document(base.clone());
    let result = proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let doc = &proof.doc().unwrap().doc;
    let bounds = doc.item(ItemId(result["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(contains(doc.spreads[0].pages[0].margin_rect(), bounds));
    assert!((bounds.y1 - original.y1).abs() < 0.02);
    assert!(original.y0 - bounds.y0 > 4.0 && original.y0 - bounds.y0 < 5.0);
    assert_eq!((bounds.x0, bounds.x1), (original.x0, original.x1));
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn ai21_footer_growth_refuses_upward_obstruction_and_reports_bounded_source_identity() {
    let (session, mut plan, original) = fitted_footer_fixture();
    plan.commands.push(EngineAction {
        command: "frame.create".into(),
        params: json!({"spread":0,"content":"graphic","rect":[60,original.y0-3.0,225,original.y0-1.0]}),
    });
    let authored = serde_json::to_value(&plan).unwrap();
    let base = session.doc().unwrap().clone();
    let error = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("source $0") && error.contains("item ") && error.contains("rect ["), "{error}");
    assert!(error.contains("Count the flowers") && error.contains("4.4 pt"), "{error}");
    assert!(error.len() < 700, "Overset diagnostics must remain bounded");
    assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

fn margin_fixture() -> (Session, ProductionPlan) {
    let mut session = Session::new();
    session
        .execute(
            "file.new",
            &json!({"preset":"A4","pages":1,"facingPages":false,
            "margins":{"top":42.51968503937008,"bottom":42.51968503937008,"inside":42.51968503937008,"outside":51.0236220472441}}),
        )
        .unwrap();
    let plan = serde_json::from_value(json!({"title":"Exact footer recovery","images":[],"commands":[
        {"command":"frame.create","params":{"spread":0,"rect":[48,800,260,816],"content":"text","text":"Teacher guide","caret":false}},
        {"command":"type.char","params":{"attrs":{"size":9,"leading":{"kind":"points","value":11}}}},
        {"command":"frame.create","params":{"spread":0,"rect":[528,801,547,817],"content":"text","text":"3","caret":false}},
        {"command":"type.char","params":{"attrs":{"size":9,"leading":{"kind":"points","value":11}}}}
    ]}))
    .unwrap();
    (session, plan)
}

#[test]
fn footer_translation_preserves_size_copy_references_and_live_document() {
    let (session, mut plan) = margin_fixture();
    let original = plan.clone();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|note| note["kind"] == "nativeTextTranslation"));
    for (before, after) in original.commands.iter().zip(&plan.commands) {
        assert_eq!(before.command, after.command);
        assert_eq!(before.params, after.params, "Authored commands and result indices must remain stable");
    }
    let mut proof = Session::new();
    proof.add_document(base.clone());
    proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let spread = &proof.doc().unwrap().doc.spreads[0];
    let safe = spread.pages[0].margin_rect();
    for (item, expected_width) in spread.items.iter().zip([212.0, 19.0]) {
        let bounds = item.bounds();
        assert!(contains(safe, bounds));
        assert!((bounds.width() - expected_width).abs() < 0.01);
        assert!((bounds.height() - 16.0).abs() < 0.01);
    }
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn footer_translation_refuses_existing_and_inherited_blockers_atomically() {
    for inherited in [false, true] {
        let (mut session, mut plan) = margin_fixture();
        let spread = if inherited { json!({"kind":"parent","index":0}) } else { json!(0) };
        session.execute("frame.create", &json!({"spread":spread,"rect":[48,780,260,799],"content":"graphic"})).unwrap();
        let base = session.doc().unwrap().clone();
        let original = serde_json::to_value(&plan).unwrap();
        assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err());
        assert_eq!(serde_json::to_value(&plan).unwrap(), original);
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

#[test]
fn empty_placeholder_geometry_is_preserved_without_translation() {
    let (session, mut plan) = margin_fixture();
    plan.commands = vec![EngineAction {
        command: "frame.create".into(),
        params: json!({"spread":0,"rect":[0,0,595,842],"content":"text","text":"  \n ","caret":false}),
    }];
    let original = serde_json::to_value(&plan).unwrap();
    let notes = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.is_empty());
    assert_eq!(serde_json::to_value(&plan).unwrap(), original);
}

fn stacked_header_fixture(card: bool, overlap: f64) -> (Session, ProductionPlan, usize, usize) {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false,"margins":42.52})).unwrap();
    let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Stacked unit header","commands":[],"images":[]})).unwrap();
    if card {
        plan.commands
            .push(EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[290,44,548,112],"content":"unassigned"}) });
    }
    let upper = plan.commands.len();
    for (rect, text) in [([300.0, 50.0, 540.0, 72.0 + overlap], "Unit Two"), ([300.0, 72.0, 540.0, 98.0], "Numbers and Patterns")] {
        let index = plan.commands.len();
        plan.commands.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread":0,"rect":rect,"content":"text","text":text,"caret":false}),
        });
        plan.commands.push(EngineAction { command: "type.char".into(), params: json!({"attrs":{"size":10,"leading":{"kind":"points","value":12}}}) });
        plan.commands
            .push(EngineAction { command: "object.textFrameOptions".into(), params: json!({"ids":[format!("${index}.id")],"inset":[1,2,1,2]}) });
    }
    (session, plan, upper, upper + 3)
}

#[test]
fn ai22_stacked_header_translation_keeps_copy_fonts_size_and_containing_card() {
    for (card, overlap) in [(true, 2.0), (false, 2.0), (true, 12.0)] {
        let (session, mut plan, upper, lower) = stacked_header_fixture(card, overlap);
        let authored = plan.clone();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands.len(), authored.commands.len() + 1, "Only the lower header may move");
        for (before, after) in authored.commands.iter().zip(&plan.commands) {
            assert_eq!(before.command, after.command);
            assert_eq!(before.params, after.params, "Authored copy, font size, insets, rectangles and source indices must remain unchanged");
        }
        let movement = plan.commands.last().unwrap();
        assert_eq!(movement.command, "transform.move");
        assert_eq!(movement.params["ids"], json!([format!("${lower}.id")]));
        assert_eq!(movement.params["dx"].as_f64().unwrap(), 0.0);
        assert!((movement.params["dy"].as_f64().unwrap() - overlap).abs() < 0.02, "Use the minimal downward translation");
        assert_eq!(notes.iter().filter(|note| note["kind"] == "nativeTextTranslation").count(), 1);
        let mut proof = Session::new();
        proof.prefs = session.prefs.clone();
        proof.add_document(base.clone());
        let applied =
            proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
        let doc = &proof.doc().unwrap().doc;
        let upper_bounds = doc.item(ItemId(applied["results"][upper]["id"].as_u64().unwrap())).unwrap().bounds();
        let lower_bounds = doc.item(ItemId(applied["results"][lower]["id"].as_u64().unwrap())).unwrap().bounds();
        assert_eq!(upper_bounds, Rect::new(300.0, 50.0, 540.0, 72.0 + overlap));
        assert!(!overlaps(upper_bounds, lower_bounds));
        assert_eq!((lower_bounds.x0, lower_bounds.x1, lower_bounds.height()), (300.0, 540.0, 26.0));
        assert!((lower_bounds.y0 - 72.0 - overlap).abs() < 0.02);
        assert!(contains(doc.spreads[0].pages[0].margin_rect(), lower_bounds));
        if card {
            let card_bounds = doc.item(ItemId(applied["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
            assert_eq!(card_bounds, Rect::new(290.0, 44.0, 548.0, 112.0));
            assert!(contains(card_bounds, upper_bounds) && contains(card_bounds, lower_bounds));
        }
        assert_eq!(doc.story(StoryId(applied["results"][upper]["story"].as_u64().unwrap())).unwrap().text, "Unit Two");
        assert_eq!(doc.story(StoryId(applied["results"][lower]["story"].as_u64().unwrap())).unwrap().text, "Numbers and Patterns");
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

#[test]
fn ai22_stacked_header_translation_refuses_real_inherited_image_and_new_body_obstructions() {
    for kind in ["existing", "inherited", "image", "new_body", "new_artwork", "card_edge"] {
        let (mut session, mut plan, _, _) = stacked_header_fixture(true, 2.0);
        let obstacle = json!({"spread":if kind == "inherited" {json!({"kind":"parent","index":0})} else {json!(0)},"rect":[300,99,540,125],"content":"unassigned"});
        match kind {
            "existing" | "inherited" => {
                session.execute("frame.create", &obstacle).unwrap();
            }
            "image" => plan.images.push(designcraft_ai_types::protocol::ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: Vec::new(),
                repeat_groups: Vec::new(),
                prompt: "Synthetic obstacle".into(),
                page: 0,
                rect: [300.0, 99.0, 540.0, 125.0],
                label: "Fixed illustration".into(),
            }),
            "new_body" => {
                plan.commands.push(EngineAction {
                    command: "frame.create".into(),
                    params: json!({"spread":0,"rect":[300,99,540,125],"content":"text","text":"Practice below","caret":false}),
                });
                plan.commands
                    .push(EngineAction { command: "type.char".into(), params: json!({"attrs":{"size":10,"leading":{"kind":"points","value":12}}}) });
            }
            "new_artwork" => plan.commands.push(EngineAction { command: "frame.create".into(), params: obstacle }),
            "card_edge" => plan.commands[0].params["rect"][3] = json!(98.5),
            _ => panic!("Unexpected obstruction fixture: {kind}"),
        }
        let authored = serde_json::to_value(&plan).unwrap();
        let base = session.doc().unwrap().clone();
        assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must block header recovery");
        assert_eq!(serde_json::to_value(&plan).unwrap(), authored, "{kind} must leave the authored plan intact");
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

#[test]
fn ai22_stacked_header_translation_rejects_mixed_duplicate_ambiguous_large_and_body_crossings() {
    for kind in ["mixed", "identical", "partial_column", "large_overlap", "body"] {
        let (session, mut plan, upper, lower) = stacked_header_fixture(false, 2.0);
        match kind {
            "mixed" => plan.commands[lower].params["text"] = json!("Numbers ب"),
            "identical" => plan.commands[lower].params["rect"] = plan.commands[upper].params["rect"].clone(),
            "partial_column" => plan.commands[lower].params["rect"][0] = json!(310),
            "large_overlap" => plan.commands[upper].params["rect"][3] = json!(84.1),
            "body" => {
                plan.commands[upper].params["rect"] = json!([300, 122, 540, 146]);
                plan.commands[lower].params["rect"] = json!([300, 144, 540, 170]);
            }
            _ => panic!("Unexpected header fixture: {kind}"),
        }
        let authored = serde_json::to_value(&plan).unwrap();
        let base = session.doc().unwrap().clone();
        assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must remain an explicit layout error");
        assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }
}

#[test]
fn bilingual_header_columns_move_coherently_without_resizing_or_retyping() {
    let fonts = designcraft_fonts::FontDb::global();
    let Some(family) =
        ["Arial", "DejaVu Sans", "Tahoma", "Cairo"].into_iter().find(|family| fonts.has_family(family) && fonts.face(family, "Regular").covers('ب'))
    else {
        return;
    };
    let (session, mut plan) = margin_fixture();
    plan.commands.clear();
    for (rect, text) in [([60, 48, 300, 72], "Header"), ([60, 72, 300, 100], "Guide"), ([290, 46, 535, 74], "باب"), ([290, 74, 535, 100], "باب")]
    {
        plan.commands.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread":0,"rect":rect,"content":"text","text":text,"caret":false}),
        });
        plan.commands.push(EngineAction {
            command: "type.char".into(),
            params: json!({"attrs":{"fontFamily":family,"size":10,"leading":{"kind":"points","value":12}}}),
        });
    }
    let original = plan.clone();
    let base = session.doc().unwrap().clone();
    prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    for (before, after) in original.commands.iter().zip(&plan.commands) {
        assert_eq!(before.params, after.params);
    }
    let mut proof = Session::new();
    proof.add_document(base);
    proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let spread = &proof.doc().unwrap().doc.spreads[0];
    let bounds: Vec<_> = spread.items.iter().map(|item| item.bounds()).collect();
    assert!((bounds[0].x0 - bounds[1].x0).abs() < 0.01);
    assert!((bounds[2].x0 - bounds[3].x0).abs() < 0.01);
    assert!((bounds[0].width() - 240.0).abs() < 0.01 && (bounds[2].width() - 245.0).abs() < 0.01);
    assert!(bounds[0].x1 + 1.0 <= bounds[2].x0);
}

#[test]
fn overset_recovers_into_free_space_without_changing_copy_size_or_live_document() {
    let (session, mut plan) = fixture();
    let original = plan.clone();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(!notes.is_empty(), "A measured frame expansion must be reported");
    assert_eq!(plan.commands[0].params["text"], original.commands[0].params["text"]);
    assert_eq!(plan.commands[1].params["attrs"], original.commands[1].params["attrs"]);
    let mut scratch = Session::new();
    scratch.add_document(base.clone());
    scratch.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
    let bounds = scratch.doc().unwrap().doc.spreads[0].items[0].bounds();
    assert!(bounds.y1 > 82.0 && bounds.y1 < 180.0);
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn inherited_parent_art_is_not_free_space_for_overset_recovery() {
    let (mut session, mut plan) = fixture();
    session.execute("frame.create", &json!({"spread":{"kind":"parent","index":0},"rect":[60,83,225,183],"content":"graphic"})).unwrap();
    let original = serde_json::to_value(&plan).unwrap();
    let base = session.doc().unwrap().clone();
    let error = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("no safe free space"), "{error}");
    assert_eq!(serde_json::to_value(&plan).unwrap(), original);
    assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
}

#[test]
fn no_space_never_cuts_copy_or_overlaps_the_next_activity() {
    let (session, mut plan) = fixture();
    plan.commands[2].params["rect"] = json!([60, 83, 225, 183]);
    let original = serde_json::to_value(&plan).unwrap();
    let result = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false));
    assert!(result.is_err(), "Unsafe fitting must fail before a paid image request");
    assert_eq!(serde_json::to_value(plan).unwrap(), original);
}

#[test]
fn unregistered_inline_cmyk_and_bare_swatch_names_resolve_to_existing_swatches() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Inline swatch recovery",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 200, 120], "content": "unassigned"}},
            {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "C=0 M=15 Y=95 K=0", "tint": 0.15}},
            {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "Paper", "weight": 1.0}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 140, 200, 200], "content": "unassigned"}},
            {"command": "object.fill", "params": {"ids": ["$3.id"], "swatch": "C:85 M:75 Y:0 K:0", "tint": 1.0}},
            {"command": "object.stroke", "params": {"ids": ["$3.id"], "swatch": "Transparent", "weight": 0.0}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 220, 200, 280], "content": "unassigned"}},
            {"command": "object.fill", "params": {"ids": ["$6.id"], "swatch": "Sunshine Yellow", "tint": 0.2}},
            {"command": "object.stroke", "params": {"ids": ["$6.id"], "swatch": "#FF0", "weight": 1.0}}
        ]
    }))
    .unwrap();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(plan.commands[1].params["swatch"], "C=0 M=0 Y=100 K=0");
    assert_eq!(plan.commands[2].params["swatch"], "[Paper]");
    assert_eq!(plan.commands[4].params["swatch"], "C=100 M=90 Y=10 K=0");
    assert_eq!(plan.commands[5].params["swatch"], "[None]");
    assert_eq!(plan.commands[7].params["swatch"], "C=0 M=0 Y=100 K=0");
    assert_eq!(plan.commands[8].params["swatch"], "C=0 M=0 Y=100 K=0");
    assert_eq!(notes.iter().filter(|n| n["kind"] == "nativeSwatchFallback").count(), 6);
}

#[test]
fn command_aliases_including_type_set_normalize_to_vetted_engine_commands() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Command alias normalization",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 120], "content": "text", "text": "Initial", "caret": false}},
                {"command": "type.set", "params": {"id": "$0.story", "content": "Updated Copy"}},
                {"command": "style.para.create", "params": {"name": "TitleStyle", "chars": {"size": 16}}},
                {"command": "style.para.apply", "params": {"name": "TitleStyle"}},
                {"command": "lists.bulleted", "params": {}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 150, 140, 230], "shape": "ellipse", "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [100, 150, 180, 230], "shape": "ellipse", "content": "unassigned"}},
                {"command": "pathfinder.add", "params": {"ids": ["$5.id", "$6.id"]}},
                {"command": "object.cornerRadius", "params": {"ids": ["$7.id"], "radius": 8}},
                {"command": "object.move", "params": {"ids": ["$7.id"], "dx": 4, "dy": 4}},
                {"command": "object.gradientFill", "params": {"ids": ["$7.id"], "kind": "linear", "stops": [{"pos": 0.0, "color": "#1E3A8A"}, {"pos": 1.0, "color": "#3B82F6"}]}},
                {"command": "qr.create", "params": {"type": "url", "content": "https://example.com", "rect": [360, 150, 440, 230]}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 260, 520, 460], "content": "text", "text": "", "caret": true}},
                {"command": "table.insert", "params": {"rows": 2, "cols": 2, "width": 440}},
                {"command": "table.insertCol", "params": {"story": "$12.story", "table": "$13.id", "col": 0, "where": "right", "count": 1}},
                {"command": "table.rowHeight", "params": {"story": "$12.story", "table": "$13.id", "row": 0, "height": 26}},
                {"command": "table.columnWidth", "params": {"story": "$12.story", "table": "$13.id", "col": 0, "width": 120}},
                {"command": "table.setHeaderFooterRows", "params": {"story": "$12.story", "table": "$13.id", "headerRows": 1, "repeatHeader": "everyTextColumn", "altRows": {"everyA": 1, "colorA": "Paper", "everyB": 1, "colorB": "Paper"}}}
            ]
        }))
        .unwrap();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(plan.commands[1].command, "story.setText");
    assert_eq!(plan.commands[1].params["story"], "$0.story");
    assert_eq!(plan.commands[1].params["text"], "Updated Copy");
    assert_eq!(plan.commands[2].command, "style.paragraph.create");
    assert_eq!(plan.commands[3].command, "style.paragraph.apply");
    assert_eq!(plan.commands[4].command, "type.para");
    assert_eq!(plan.commands[4].params["attrs"]["listType"], "bullets");
    assert_eq!(plan.commands[7].command, "object.pathfinder");
    assert_eq!(plan.commands[7].params["op"], "add");
    assert_eq!(plan.commands[8].command, "object.cornerOptions");
    assert_eq!(plan.commands[8].params["size"], 8);
    assert_eq!(plan.commands[8].params["shape"], "rounded");
    assert_eq!(plan.commands[9].command, "transform.move");
    assert_eq!(plan.commands[10].command, "object.gradient");
    assert_eq!(plan.commands[10].params["stops"][0]["location"], 0.0);
    assert_eq!(plan.commands[10].params["stops"][1]["location"], 100.0);
    assert_eq!(plan.commands[11].command, "object.qrCode");
    assert_eq!(plan.commands[14].command, "table.insertColumn");
    assert_eq!(plan.commands[14].params["table"], "$13.table");
    assert_eq!(plan.commands[15].command, "table.setRowHeight");
    assert_eq!(plan.commands[16].command, "table.setColumnWidth");
    assert_eq!(plan.commands[17].command, "table.options");
    assert_eq!(plan.commands[17].params["repeatHeader"], true);
    assert_eq!(plan.commands[17].params["altRows"]["first"], 1);
    assert_eq!(plan.commands[17].params["altRows"]["firstColor"], "[Paper]");
    assert!(notes.iter().any(|n| n["kind"] == "commandAliasNormalized"));
}

#[test]
fn implicit_frame_text_content_and_post_create_height_overrides_recover_without_overset() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let copy =
        "Line one of a multi-line callout box that requires more than twenty-four points of vertical height to compose cleanly without any overset.";
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Implicit text and post-create transform",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 220, 84], "text": copy, "caret": false}},
            {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}},
            {"command": "transform.set", "params": {"ids": ["$0.id"], "height": 24}}
        ]
    }))
    .unwrap();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(plan.commands[0].params["content"], "text");
    assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
}

#[test]
fn overset_frame_blocked_below_shifts_upward_into_verified_free_space() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let copy = "This caption sits directly above an illustration divider and needs two extra lines, while generous free space remains above it.";
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Upward mid-page fit",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 220, 220, 244], "content": "text", "text": copy, "caret": false}},
            {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 250, 220, 360], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let base = session.doc().unwrap().clone();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|n| n["kind"] == "nativeTextTranslation"));
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
}

#[test]
fn opened_document_story_edit_recovers_newly_induced_overset_into_free_space() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
    let created = session.execute("frame.create", &json!({"spread":0,"rect":[60,60,220,86],"content":"text","text":"Short","caret":false})).unwrap();
    let story_id = created["story"].as_u64().unwrap();
    let base = session.doc().unwrap().clone();
    let expanded_copy =
        "Expanded editorial paragraph replacing a short title in an opened document where ample vertical room exists below the frame.";
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Opened document story edit",
        "images": [],
        "commands": [
            {"command": "story.setText", "params": {"story": story_id, "text": expanded_copy}},
            {"command": "text.select", "params": {"story": story_id, "anchor": 0, "focus": expanded_copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}}
        ]
    }))
    .unwrap();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
}

#[test]
fn overset_frame_followed_by_alignment_or_scaling_avoids_double_geometry_mutation() {
    let copy = "This footer paragraph requires three lines at 13 pt so the initial 22 pt box overflows before bottom-margin alignment runs.";
    for mode in ["align_bottom", "scale_center"] {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let base = session.doc().unwrap().clone();
        let post_action = if mode == "align_bottom" {
            json!({"command": "object.align", "params": {"ids": "$0.id", "align": "bottom", "to": "margins"}})
        } else {
            json!({"command": "transform.scale", "params": {"ids": ["$0.id"], "sx": 1.05, "sy": 1.05}})
        };
        let mut plan: ProductionPlan = serde_json::from_value(json!({
                "title": "Post-create geometry parity",
                "images": [],
                "commands": [
                    {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 200.0, 320.0, 222.0], "content": "text", "text": copy, "caret": false}},
                    {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                    {"command": "type.char", "params": {"attrs": {"size": 13, "leading": {"kind": "points", "value": 17}}}},
                    post_action
                ]
            }))
            .unwrap();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"), "{mode}");
        assert_eq!(plan.commands[0].params["rect"][3], json!(222.0), "{mode}: original creation height must stay intact before post-create geometry");
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }
}

#[test]
fn honorific_and_symbol_glyph_fallbacks_preserve_bold_and_preflight_cleanly_in_new_and_opened_stories() {
    let db = designcraft_fonts::FontDb::global();
    let Some(primary) = ["Tahoma", "Cairo", "DejaVu Sans", "Arial"]
        .into_iter()
        .find(|f| db.has_family(f) && ["Regular", "Bold"].iter().all(|s| db.styles(f).iter().any(|st| st == s)))
    else {
        return;
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let existing = session
        .execute("frame.create", &json!({"spread": 0, "rect": [60, 320, 520, 400], "content": "text", "text": "Initial", "caret": false}))
        .unwrap();
    let existing_story = existing["story"].as_u64().unwrap();
    let base = session.doc().unwrap().clone();
    let scoped = db.scoped(base.doc.font_scope);
    for ch in ['\u{fdfa}', '\u{fdfd}', '★', '✓', '⭐'] {
        if !scoped.face(primary, "Regular").covers(ch) && resolve_fallback_face(&scoped, ch, false, false).is_none() {
            return;
        }
    }
    let new_copy = "صلى الله عليه وسلم ﷺ وبسم الله ﷽ ★ ✓ ⭐";
    let bold_end = new_copy.find(" وبسم").unwrap();
    let edited_copy = "مراجعة الدرس ✓ ونجمة التفوق ⭐ ﷺ";
    let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Honorific and symbol glyph fallback",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 520, 140], "content": "text", "text": new_copy, "caret": false}},
                {"command": "type.para", "params": {"attrs": {"direction": "rightToLeft", "align": "right"}}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Regular", "size": 15, "leading": {"kind": "points", "value": 22}}}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": bold_end}},
                {"command": "type.char", "params": {"attrs": {"fontStyle": "Bold"}}},
                {"command": "story.setText", "params": {"story": existing_story, "text": edited_copy}},
                {"command": "text.select", "params": {"story": existing_story, "anchor": 0, "focus": edited_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Bold", "size": 15, "leading": {"kind": "points", "value": 22}}}}
            ]
        }))
        .unwrap();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    if !scoped.face(primary, "Regular").covers('\u{fdfa}') || !scoped.face(primary, "Regular").covers('⭐') {
        assert!(notes.iter().any(|n| n["kind"] == "symbolFontFallback"), "notes={notes:?}");
    }
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
}

#[test]
fn container_enclosed_header_and_wrapped_short_badge_recover_without_overset_or_collision() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();

    // Case A: Subtitle [54, 83, 280, 96] (13 pt tall) inside a non-growable graphic header banner [44, 44, 550, 98]
    // needing ~4 pt more height shifts upward into the >= 8 pt top clearance inside the banner.
    let subtitle = "Unit Three: Patterns";
    let mut header_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Container-enclosed header upward shift",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [44, 44, 550, 96.5], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [54, 83, 280, 96], "content": "text", "text": subtitle, "caret": false}},
            {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": subtitle.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 19}}}}
        ]
    }))
    .unwrap();
    let header_notes = prepare(&mut header_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(header_notes.iter().any(|n| n["kind"] == "nativeTextTranslation"), "header_notes={header_notes:?}");
    let mut proof_a = Session::new();
    proof_a.add_document(base.clone());
    proof_a.execute("production.apply", &json!({"commands": header_plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

    // Case B: Short single-line badge label wrapped into 2 lines inside a vertical sandwich (blocked above and below)
    // recovers via bounded horizontal widening into free safe space, while horizontal obstruction fails atomically.
    let badge_copy = "Rectangle • Shape";
    let mut badge_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Wrapped short badge horizontal widening",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [200, 200, 380, 239], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [240, 241, 322, 261], "content": "text", "text": badge_copy, "caret": false}},
            {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": badge_copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 12, "leading": {"kind": "points", "value": 15}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [200, 263, 380, 310], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let badge_notes = prepare(&mut badge_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(badge_notes.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "badge_notes={badge_notes:?}");
    let mut proof_b = Session::new();
    proof_b.add_document(base.clone());
    proof_b.execute("production.apply", &json!({"commands": badge_plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

    // Blocking both left and right horizontal space must reject atomically without mutating the plan.
    let mut blocked_badge: ProductionPlan = serde_json::from_value(json!({
        "title": "Blocked badge",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [200, 200, 380, 239], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [240, 241, 322, 261], "content": "text", "text": badge_copy, "caret": false}},
            {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": badge_copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 12, "leading": {"kind": "points", "value": 15}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [200, 263, 380, 310], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [180, 241, 238, 261], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [324, 241, 390, 261], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let blocked_before = serde_json::to_value(&blocked_badge).unwrap();
    assert!(prepare(&mut blocked_badge, &base, &session.prefs, &AtomicBool::new(false)).is_err());
    assert_eq!(serde_json::to_value(&blocked_badge).unwrap(), blocked_before);
}

#[test]
fn null_formatting_and_selection_step_references_normalize_to_target_producers() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Null formatting step references",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 110], "content": "text", "text": "Header box", "caret": false}},
            {"command": "type.para", "params": {"attrs": {"align": "center"}}},
            {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.0}},
            {"command": "selection.set", "params": {"ids": ["$0.id"]}},
            {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": 6}},
            {"command": "type.char", "params": {"attrs": {"size": 14}}}
        ]
    }))
    .unwrap();
    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(plan.commands[2].params["ids"], json!(["$0.id"]));
    assert_eq!(plan.commands[4].params["story"], json!("$0.story"));
    assert!(notes.iter().any(|n| n["kind"] == "commandAliasNormalized"));
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

    // Off-by-one forward non-text creation after an implicit formatting step must resolve to the next non-text item.
    let mut forward_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Off-by-one forward non-text",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 110], "content": "text", "text": "Title", "caret": false}},
            {"command": "type.para", "params": {"attrs": {"align": "center"}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 130, 180, 210], "content": "unassigned"}},
            {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.5}}
        ]
    }))
    .unwrap();
    normalize_command_aliases(&mut forward_plan);
    assert_eq!(forward_plan.commands[3].params["ids"], json!(["$2.id"]));
}

#[test]
fn rtl_facing_spread_step_and_repeat_group_pathfinder_and_gradient_apply_cleanly() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 3, "facingPages": true})).unwrap();
    session.execute("layout.documentSetup", &json!({"binding": "rightToLeft"})).unwrap();
    let rtl_base = session.doc().unwrap().clone();
    assert!(rtl_base.doc.settings.facing_pages && rtl_base.doc.settings.right_to_left_binding);
    let canonical = crate::facing_pages::canonicalize(&rtl_base, &session.prefs).unwrap();

    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "RTL spread vector composition",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 1, "rect": [72.0, 90.0, 140.0, 150.0], "content": "unassigned"}},
            {"command": "edit.stepAndRepeat", "params": {"ids": ["$0.id"], "count": 2, "dx": 90.0, "dy": 0.0}},
            {"command": "object.group", "params": {"ids": "$1.ids"}},
            {"command": "object.gradient", "params": {
                "ids": "$1.ids",
                "kind": "linear",
                "from": [162.0, 90.0],
                "to": [320.0, 150.0],
                "stops": [{"color": "#0F172A", "location": 0.0}, {"color": "#38BDF8", "location": 100.0}]
            }},
            {"command": "frame.create", "params": {"spread": 2, "rect": [90.0, 120.0, 180.0, 210.0], "shape": "ellipse", "content": "unassigned"}},
            {"command": "frame.create", "params": {"spread": 2, "rect": [140.0, 120.0, 230.0, 210.0], "shape": "ellipse", "content": "unassigned"}},
            {"command": "object.pathfinder", "params": {"op": "add", "ids": ["$4.id", "$5.id"]}},
            {"command": "object.gradient", "params": {
                "ids": ["$6.id"],
                "kind": "linear",
                "from": [90.0, 120.0],
                "to": [230.0, 210.0],
                "stops": [{"color": "#1E3A8A", "location": 0.0}, {"color": "#F59E0B", "location": 100.0}]
            }}
        ]
    }))
    .unwrap();

    prepare(&mut plan, &canonical, &session.prefs, &AtomicBool::new(false)).unwrap();
    let mapped = crate::facing_pages::map_plan(&plan, &rtl_base).unwrap();
    session.execute("production.apply", &json!({"commands": mapped.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    crate::facing_pages::validate_mapped_result(&rtl_base, session.doc().unwrap(), crate::worker::BatchScope { start: 1, count: 2 }).unwrap();
}

#[test]
fn opened_document_simultaneous_frame_resize_and_story_edit_recovers_cleanly() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let created = session
        .execute("frame.create", &json!({"spread": 0, "rect": [60, 60, 200, 90], "content": "text", "text": "Old title", "caret": false}))
        .unwrap();
    let frame_id = created["id"].as_u64().unwrap();
    let story_id = created["story"].as_u64().unwrap();
    let base = session.doc().unwrap().clone();

    let updated_copy =
        "Expanded callout paragraph in an opened document whose width and copy are edited together, requiring additional vertical height below.";
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Simultaneous frame resize and story edit",
        "images": [],
        "commands": [
            {"command": "transform.set", "params": {"ids": [frame_id], "width": 220.0, "height": 34.0, "ref": 0}},
            {"command": "story.setText", "params": {"story": story_id, "text": updated_copy}},
            {"command": "text.select", "params": {"story": story_id, "anchor": 0, "focus": updated_copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}}
        ]
    }))
    .unwrap();

    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    let after = session.doc().unwrap();
    assert!(!session.cache.get(&after.doc, StoryId(story_id), None).is_overset());
    assert!((after.doc.item(ItemId(frame_id)).unwrap().bounds().width() - 220.0).abs() < 0.05);
}

#[test]
fn run_069_subtitle_widen_and_partial_banner_upward_shift_recover_without_overset() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();

    // Exact Run [069] geometry: 40-char, 7-word subtitle [54, 76, 260, 96] inside banner [46, 44, 548, 98],
    // blocked above by title [54, 48, 280, 75] and with 54 pt free horizontal space before [314, 76, 540, 96].
    let subtitle = "Numbers 15 & 16: Ocean Treasure Counting";
    let mut run_069_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Run 069 subtitle horizontal widening",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [46, 44, 548, 98], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [54, 48, 280, 75], "content": "text", "text": "UNIT FOUR", "caret": false}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [54, 76, 260, 96], "content": "text", "text": subtitle, "caret": false}},
            {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": subtitle.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 12.5, "leading": {"kind": "points", "value": 16.0}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [314, 76, 540, 96], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let widen_notes = prepare(&mut run_069_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(widen_notes.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "notes={widen_notes:?}");
    let mut proof_069 = Session::new();
    proof_069.add_document(base.clone());
    proof_069
        .execute("production.apply", &json!({"commands": run_069_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();

    // Partial upward shift inside a fixed banner: downward growth (~93.5 pt) exceeds fixed graphic banner [46, 44, 548, 92],
    // full upward shift (-5.5 pt -> y0=54.5) is blocked by upper accent rule at y1=56.5, while partial shift (-2.76 pt -> y0=57.24, y1=90.76) fits cleanly.
    let banner_heading = "Lesson 12: Exploring Coral Reefs\nCounting & Comparing";
    let mut partial_banner_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Partial banner upward shift",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46, 44, 548, 92], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 48, 220, 56.5], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 60, 340, 88], "content": "text", "text": banner_heading, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": banner_heading.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 19}}}}
            ]
        }))
        .unwrap();
    let shift_notes = prepare(&mut partial_banner_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(shift_notes.iter().any(|n| n["kind"] == "nativeTextTranslation"), "shift_notes={shift_notes:?}");
    let mut proof_banner = Session::new();
    proof_banner.add_document(base);
    proof_banner
        .execute("production.apply", &json!({"commands": partial_banner_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
}

#[test]
fn enclosed_numerals_ornate_parens_and_variation_selectors_survive_print_fonts_and_fallback() {
    let db = designcraft_fonts::FontDb::global();
    let Some(primary) = ["Tahoma", "Cairo", "DejaVu Sans", "Arial"]
        .into_iter()
        .find(|f| db.has_family(f) && ["Regular", "Bold"].iter().all(|s| db.styles(f).iter().any(|st| st == s)))
    else {
        return;
    };
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let scoped = db.scoped(base.doc.font_scope);
    let copy = "① ⅓ ﴾بِسْمِ اللَّهِ﴿ ﷺ ﷽ ⭐\u{FE0F} ✓\u{FE0F}";
    for ch in ['①', '⅓', '﴾', '﴿', '\u{FDFA}', '\u{FDFD}', '⭐', '✓'] {
        if !scoped.face(primary, "Regular").covers(ch) && !has_fallback_glyph(&scoped, ch) {
            return;
        }
    }
    let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Extended symbol & honorific fallback with print fonts",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 520, 140], "content": "text", "text": copy, "caret": false}},
                {"command": "type.para", "params": {"attrs": {"direction": "rightToLeft", "align": "right"}}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Regular", "size": 15, "leading": {"kind": "points", "value": 22}}}}
            ]
        }))
        .unwrap();
    let contract =
        crate::print_contract::PrintContract { english_fonts: vec![primary.into()], arabic_fonts: vec![primary.into()], ..Default::default() };
    if contract.select_fonts(&base).is_ok() {
        crate::native_print_fonts::apply(&mut plan, &base, &session.prefs, &contract).unwrap();
    }
    prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
}

#[test]
fn run_006_non_story_references_extended_null_steps_text_thread_and_edit_clear_normalize_cleanly() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();

    // Case 1: Paired container + inner text frame where text.select references container $0.story (Run [006] pattern A).
    let mut paired_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Paired container story ref",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 320, 130], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "[Paper]"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [68, 68, 312, 122], "content": "text", "text": "Card heading", "caret": false}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": 12}},
                {"command": "type.char", "params": {"attrs": {"size": 14}}}
            ]
        }))
        .unwrap();
    prepare(&mut paired_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(paired_plan.commands[3].params["story"], json!("$2.story"));
    let mut s1 = Session::new();
    s1.add_document(base.clone());
    s1.execute("production.apply", &json!({"commands": paired_plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

    // Case 2: frame.create omitting content="text" followed by story.setText & text.select targeting $0.story (Run [006] pattern B).
    let mut omitted_content_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Omitted content=text frame",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 150, 320, 220]}},
            {"command": "story.setText", "params": {"story": "$0.story", "text": "Populated story"}},
            {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": 9}},
            {"command": "type.char", "params": {"attrs": {"size": 13}}}
        ]
    }))
    .unwrap();
    prepare(&mut omitted_content_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert_eq!(omitted_content_plan.commands[0].params["content"], json!("text"));
    let mut s2 = Session::new();
    s2.add_document(base.clone());
    s2.execute("production.apply", &json!({"commands": omitted_content_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();

    // Case 3: Extended null steps (transform.rotate, object.dropShadow) in text.thread from/to + edit.clear invalidation.
    let mut thread_and_clear_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Extended null steps in text.thread and edit.clear invalidation",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 240, 120], "content": "text", "text": "Column A", "caret": false}},
            {"command": "object.dropShadow", "params": {"ids": ["$0.id"], "on": true, "distance": 2.0, "size": 4.0}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [260, 60, 440, 120], "content": "text", "text": "", "caret": false}},
            {"command": "transform.rotate", "params": {"ids": ["$2.id"], "angle": 0.0}},
            {"command": "text.thread", "params": {"from": "$1.id", "to": "$3.id"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [60, 200, 140, 250], "content": "unassigned"}},
            {"command": "object.fill", "params": {"ids": ["$5.id"], "swatch": "[Black]"}},
            {"command": "edit.clear", "params": {"ids": ["$5.id"]}},
            {"command": "object.stroke", "params": {"ids": ["$6.id"], "swatch": "[Black]", "weight": 1.0}}
        ]
    }))
    .unwrap();
    normalize_command_aliases(&mut thread_and_clear_plan);
    assert_eq!(thread_and_clear_plan.commands[4].params["from"], json!("$0.id"));
    assert_eq!(thread_and_clear_plan.commands[4].params["to"], json!("$2.id"));
    assert_eq!(thread_and_clear_plan.commands[8].params["ids"], json!(["$6.id"]), "deleted $5.id must not stay active after edit.clear");
}

#[test]
fn tight_container_insets_and_run_098_snug_pill_badges_recover_without_overset() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();

    // 1. Tight 4 pt top-inset card header [52, 102, 286, 115] inside [46, 98, 292, 246]
    // with obstacle at y0=115.6 requiring combined partial upward + downward gap fit.
    let card_header = "Identify 2D & 3D Shapes";
    let mut tight_top_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Tight 4pt top-inset card header",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [46, 98, 292, 246], "content": "unassigned"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [52, 102, 286, 115], "content": "text", "text": card_header, "caret": false}},
            {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": card_header.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 14.0, "leading": {"kind": "points", "value": 19.0}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [52, 115.6, 286, 220], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let notes_top = prepare(&mut tight_top_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes_top.iter().any(|n| n["kind"] == "nativeTextTranslation"), "notes_top={notes_top:?}");
    let mut s_top = Session::new();
    s_top.add_document(base.clone());
    s_top.execute("production.apply", &json!({"commands": tight_top_plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

    // 2. Tight 4 pt left-inset frame [58, 558, 136, 578] inside [54, 554, 210, 756], blocked vertically, widens rightward.
    let side_copy = "Count & Circle Shapes";
    let mut tight_side_plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Tight 4pt side-inset widening",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [54, 554, 210, 756], "content": "unassigned"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [58, 558, 136, 578], "content": "text", "text": side_copy, "caret": false}},
            {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": side_copy.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 12.0, "leading": {"kind": "points", "value": 15.0}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [58, 579, 206, 700], "content": "graphic"}}
        ]
    }))
    .unwrap();
    let notes_side = prepare(&mut tight_side_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes_side.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "notes_side={notes_side:?}");
    let mut s_side = Session::new();
    s_side.add_document(base.clone());
    s_side
        .execute("production.apply", &json!({"commands": tight_side_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();

    // 3. Exact Run [098] badge [429, 243, 535, 261] ("مستطيل • Rectangle") enclosed in snug pill [425, 241, 539, 263]
    // inside outer card [408, 98, 548, 360], blocked below at y0=264.0 with free upward space.
    let db = designcraft_fonts::FontDb::global();
    let ar_font = ["Cairo", "Tahoma", "Arial", "DejaVu Sans"].into_iter().find(|f| db.has_family(f)).unwrap_or("DejaVu Sans");
    let run_098_copy = "مستطيل • Rectangle";
    let mut run_098_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Run 098 snug pill badge upward recovery",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [408, 98, 548, 360], "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [425, 241, 539, 263], "content": "unassigned"}},
                {"command": "object.cornerOptions", "params": {"ids": ["$1.id"], "shape": "rounded", "size": 8.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [429, 243, 535, 261], "content": "text", "text": run_098_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": run_098_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": ar_font, "size": 12.5, "leading": {"kind": "points", "value": 16.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [416, 264, 540, 348], "content": "graphic"}}
            ]
        }))
        .unwrap();
    prepare(&mut run_098_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    let mut s_098 = Session::new();
    s_098.add_document(base.clone());
    let applied_098 = s_098
        .execute("production.apply", &json!({"commands": run_098_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
    let doc_098 = &s_098.doc().unwrap().doc;
    let card_b = doc_098.item(ItemId(applied_098["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
    let pill_b = doc_098.item(ItemId(applied_098["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
    let text_b = doc_098.item(ItemId(applied_098["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(contains(card_b, pill_b) && contains(pill_b, text_b), "card={card_b:?} pill={pill_b:?} text={text_b:?}");

    // 4. Snug 44x14 pill badge [275, 726, 319, 740] ("Cylinder") inside snug pill [271, 724, 323, 742],
    // blocked vertically above & below, widens horizontally together with its pill background.
    let cyl = "Cylinder";
    let mut snug_pill_widen: ProductionPlan = serde_json::from_value(json!({
        "title": "Snug 44x14 pill horizontal widening",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [250, 680, 344, 723], "content": "graphic"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [271, 724, 323, 742], "content": "unassigned"}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [275, 726, 319, 740], "content": "text", "text": cyl, "caret": false}},
            {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": cyl.len()}},
            {"command": "type.char", "params": {"attrs": {"size": 11.5, "leading": {"kind": "points", "value": 13.0}}}},
            {"command": "frame.create", "params": {"spread": 0, "rect": [250, 743, 344, 770], "content": "graphic"}}
        ]
    }))
    .unwrap();
    prepare(&mut snug_pill_widen, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    let mut s_cyl = Session::new();
    s_cyl.add_document(base);
    let applied_cyl = s_cyl
        .execute("production.apply", &json!({"commands": snug_pill_widen.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
    let doc_cyl = &s_cyl.doc().unwrap().doc;
    let cyl_pill = doc_cyl.item(ItemId(applied_cyl["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
    let cyl_text = doc_cyl.item(ItemId(applied_cyl["results"][2]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(contains(cyl_pill, cyl_text), "cyl_pill={cyl_pill:?} cyl_text={cyl_text:?}");
}

#[test]
fn omitted_content_stroked_containers_pills_and_multiline_tight_inset_cards_recover_cleanly() {
    let mut session = Session::new();
    session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let db = designcraft_fonts::FontDb::global();
    let ar_font = ["Cairo", "Tahoma", "Arial", "DejaVu Sans"].into_iter().find(|f| db.has_family(f)).unwrap_or("DejaVu Sans");

    // 1. Real LLM plans omit `"content"` on card/pill `frame.create` actions and apply strokes.
    // Run [098] badge [429, 243, 535, 261] inside omitted-content stroked pill [425, 241, 539, 263]
    // and omitted-content stroked card [408, 98, 548, 360].
    let run_098_copy = "مستطيل • Rectangle";
    let mut omitted_pill_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Omitted-content stroked Run 098 pill",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [408.0, 98.0, 548.0, 360.0]}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "[Black]", "weight": 1.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [425.0, 241.0, 539.0, 263.0]}},
                {"command": "object.stroke", "params": {"ids": ["$2.id"], "swatch": "[Black]", "weight": 1.5}},
                {"command": "object.cornerOptions", "params": {"ids": ["$2.id"], "shape": "rounded", "size": 8.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [429.0, 243.0, 535.0, 261.0], "content": "text", "text": run_098_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$5.story", "anchor": 0, "focus": run_098_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": ar_font, "size": 12.5, "leading": {"kind": "points", "value": 16.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [416.0, 265.0, 540.0, 348.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
    prepare(&mut omitted_pill_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    let mut s1 = Session::new();
    s1.add_document(base.clone());
    let r1 = s1
        .execute("production.apply", &json!({"commands": omitted_pill_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
    let d1 = &s1.doc().unwrap().doc;
    let card_v = d1.item(ItemId(r1["results"][0]["id"].as_u64().unwrap())).unwrap().visible_bounds();
    let pill_v = d1.item(ItemId(r1["results"][2]["id"].as_u64().unwrap())).unwrap().visible_bounds();
    let text_v = d1.item(ItemId(r1["results"][5]["id"].as_u64().unwrap())).unwrap().visible_bounds();
    let gfx_v = d1.item(ItemId(r1["results"][8]["id"].as_u64().unwrap())).unwrap().visible_bounds();
    assert!(contains(card_v, pill_v) && contains(pill_v, text_v) && !overlaps(pill_v, gfx_v), "pill={pill_v:?} text={text_v:?} gfx={gfx_v:?}");

    // 2. Omitted-content 44x14 pill widening horizontally together with its enclosed short label.
    let cyl = "Cylinder";
    let mut omitted_widen_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Omitted-content snug pill horizontal widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [250.0, 680.0, 344.0, 722.0], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [271.0, 724.0, 323.0, 742.0]}},
                {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [275.0, 726.0, 319.0, 740.0], "content": "text", "text": cyl, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": cyl.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 11.5, "leading": {"kind": "points", "value": 13.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [250.0, 744.0, 344.0, 770.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
    prepare(&mut omitted_widen_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    let mut s2 = Session::new();
    s2.add_document(base.clone());
    let r2 = s2
        .execute("production.apply", &json!({"commands": omitted_widen_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
    let d2 = &s2.doc().unwrap().doc;
    let p2 = d2.item(ItemId(r2["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
    let t2 = d2.item(ItemId(r2["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(contains(p2, t2), "p2={p2:?} t2={t2:?}");

    // 3. Multi-line (42 pt tall, > 36 pt) prompt frame [314, 256, 540, 298] with 4 pt top inset
    // inside omitted-content card [46, 252, 548, 486] blocked below by graphic [54, 299.5, 540, 480] (last-rejected-response.txt $62/$69/$73).
    let multiline_prompt = "Trace the dashed symmetry line carefully, complete the missing half of each shape, and color both sides evenly.";
    let mut card_multiline_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Multi-line 42pt frame inside 4pt-inset authored card",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46.0, 252.0, 548.0, 486.0]}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "[Black]", "weight": 1.5}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [314.0, 256.0, 540.0, 298.0], "content": "text", "text": multiline_prompt, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": multiline_prompt.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 11.0, "leading": {"kind": "points", "value": 15.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54.0, 298.1, 540.0, 480.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
    let notes3 = prepare(&mut card_multiline_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes3.iter().any(|n| n["kind"] == "nativeTextTranslation"), "notes3={notes3:?}");
    let mut s3 = Session::new();
    s3.add_document(base);
    let r3 = s3
        .execute("production.apply", &json!({"commands": card_multiline_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
        .unwrap();
    let d3 = &s3.doc().unwrap().doc;
    let card3 = d3.item(ItemId(r3["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
    let text3 = d3.item(ItemId(r3["results"][2]["id"].as_u64().unwrap())).unwrap().bounds();
    let gfx3 = d3.item(ItemId(r3["results"][5]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(contains(card3, text3) && text3.y0 >= card3.y0 + 1.25 && !overlaps(text3, gfx3), "card3={card3:?} text3={text3:?} gfx3={gfx3:?}");

    // 4. Single-text container omitting `"content"` grows downward when clear, and refuses padding-band obstacles without crushing top padding.
    let (s_cfg, mut omitted_wrap, bg_wrap) = contained_growth_fixture();
    omitted_wrap.commands[0].params.as_object_mut().unwrap().remove("content");
    let mut blocked_wrap = omitted_wrap.clone();
    blocked_wrap.commands.push(EngineAction {
        command: "frame.create".into(),
        params: json!({"spread": 0, "content": "graphic", "rect": [54.0, bg_wrap.y1 + 1.0, 59.0, bg_wrap.y1 + 3.0]}),
    });
    let notes4 = prepare(&mut omitted_wrap, s_cfg.doc().unwrap(), &s_cfg.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes4.iter().any(|n| n["kind"] == "nativeBackgroundReflow"));
    assert!(prepare(&mut blocked_wrap, s_cfg.doc().unwrap(), &s_cfg.prefs, &AtomicBool::new(false)).is_err());
}

#[test]
fn ai28_grouped_rounded_pill_expands_on_overset_and_combining_mark_fallback_keeps_cluster() {
    let copy = "نَشَاطٌ تَعْلِيمِيٌّ مُشَكَّلٌ: اِخْتَرِ الْحَرْفَ الصَّحِيحَ وَضَعْ دَائِرَةً حَوْلَ الإِجَابَةِ";
    let all_chars: Vec<char> = copy.chars().collect();
    let base_only_chars: Vec<char> = copy.chars().filter(|&ch| !is_combining_arabic_mark(ch)).collect();
    let fonts = designcraft_fonts::FontDb::global();
    fonts.add_font(designcraft_fonts::testing::font_with("DC AI28 Pill Base", &base_only_chars).unwrap());
    fonts.add_font(designcraft_fonts::testing::font_with("DC AI28 Pill Full", &all_chars).unwrap());

    let mut session = Session::new();
    session.execute("file.new", &json!({"width": 595, "height": 842, "pages": 1, "facingPages": false, "margins": [36, 36, 36, 36]})).unwrap();
    let base = session.doc().unwrap().clone();

    let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Grouped rounded pill overset expansion",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 100.0, 320.0, 130.0]}},
                {"command": "object.cornerOptions", "params": {"ids": ["$0.id"], "shape": "rounded", "size": 12.0}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "[Paper]"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [82.0, 104.0, 310.0, 126.0], "content": "text", "text": copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": "DC AI28 Pill Base", "size": 12.0, "leading": {"kind": "points", "value": 18.0}}}},
                {"command": "object.group", "params": {"ids": ["$0.id", "$3.id"]}}
            ]
        }))
        .unwrap();

    let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
    assert!(notes.iter().any(|n| n["kind"] == "symbolFontFallback"), "expected combining-mark cluster fallback: {notes:?}");
    assert!(
        plan.commands.iter().skip(7).any(|cmd| cmd.command == "text.select" && cmd.params["anchor"] == 0),
        "first combining-mark fallback must include leading base letter byte 0: {:?}",
        &plan.commands[7..]
    );
    assert!(notes.iter().any(|n| n["kind"] == "nativeBackgroundReflow"), "expected grouped rounded pill to expand: {notes:?}");
    assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"), "expected grouped text frame to expand: {notes:?}");

    let mut verify = Session::new();
    verify.add_document(base);
    let applied =
        verify.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    let doc = &verify.doc().unwrap().doc;
    let pill = doc.item(ItemId(applied["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
    let text = doc.item(ItemId(applied["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
    assert!(pill.height() > 30.0 && text.height() > 22.0 && contains(pill, text), "pill={pill:?} text={text:?}");
}

#[test]
fn ai28_parse_inline_color_and_swatch_recovery_do_not_panic_on_multibyte_hash_strings() {
    assert!(parse_inline_color("#aأ").is_none());
    assert!(parse_inline_color("#aأbcd").is_none());
    assert!(parse_inline_color("#أب").is_none());
    assert!(parse_inline_color("#1a2b3c").is_some());
    assert!(parse_inline_color("#abc").is_some());

    let mut session = Session::new();
    session.execute("file.new", &json!({"pages": 1, "facingPages": false})).unwrap();
    let base = session.doc().unwrap().clone();
    let mut plan: ProductionPlan = serde_json::from_value(json!({
        "title": "Multibyte swatch safety",
        "images": [],
        "commands": [
            {"command": "frame.create", "params": {"spread": 0, "rect": [36.0, 36.0, 120.0, 120.0]}},
            {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "#aأ"}},
            {"command": "object.stroke", "params": {"ids": ["$0.id"], "color": "#aأbcd"}}
        ]
    }))
    .unwrap();
    let notes = recover_unregistered_swatches(&mut plan, &base).unwrap();
    assert!(notes.is_empty());
    assert_eq!(plan.commands[1].params["swatch"], "#aأ");
    assert_eq!(plan.commands[2].params["color"], "#aأbcd");
}
