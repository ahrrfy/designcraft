//! Original executable page families. No publisher pages, artwork or fonts are bundled.
use crate::{NativeManifest, PackageFiles, Result};
use designcraft_ai_types::{DesignCorrection, DesignImageSlot, DesignProfile, DesignTextSlot, EngineAction, PageFamily};
use serde_json::json;

pub(crate) fn package() -> Result<PackageFiles> {
    let id = "early-years-clear-style";
    let mut profile = DesignProfile {
        name: "Clear early-years / رياض الأطفال الواضح".into(),
        keywords: ["رياض الأطفال", "رياض الاطفال", "روضة", "kindergarten", "kg1", "kg2", "early years"].map(String::from).into(),
        age_range: [4, 6], page_mm: [210.0, 297.0], margin_mm: 15.0,
        palette: ["C=100 M=90 Y=10 K=0", "C=15 M=100 Y=100 K=0", "C=75 M=5 Y=100 K=0", "[Black]", "[Paper]"].map(String::from).into(),
        typography: vec!["Use installed Tahoma or an explicitly verified Arabic/Latin font. Keep Arabic and English in independent native frames.".into(), "Body 18–22 pt; titles 26–34 pt; footer 11–12 pt; explicit leading at least size + 7 pt. Short instructions; no emoji or unsupported decorative glyphs.".into()],
        layout: vec!["A4, 15 mm safe margins, aligned native header/footer. Content and pictures stay within the safe area. No full-bleed backgrounds in this style.".into(), "Choose a page family for its objective. Several related tasks and independent illustrations may share a page. Preserve generous answer areas and vary composition without changing the identity.".into()],
        pedagogy: vec!["Use the stated age and curriculum objectives; move from observation to guided practice and independent response. Each activity has one short, specific instruction.".into(), "Use picture comparison, counting, matching, classification and drawing as appropriate. Record the learning objective. Human educator review is required; do not claim publisher or standards accreditation.".into()],
        illustration: vec!["Friendly, clear, fully colored educational illustrations with consistent proportions and palette; detailed enough to recognize the subject. Use realistic imagery if the brief requests it.".into(), "Keep characters' colors, clothing and proportions consistent. Native captions remain outside image pixels. A verbal style guide alone cannot guarantee identical generated characters.".into()],
        page_families: [
            ("cover", "Opening page with title, clear visual and pupil name", "templates/cover.json"),
            ("compare-practice", "Compare two examples, then complete an independent drawing task", "templates/compare-practice.json"),
            ("review", "Review through counting, matching and a short reflection", "templates/review.json"),
        ].map(|(id, purpose, template)| PageFamily { id: id.into(), purpose: purpose.into(), template: template.into(), text_slots:vec![],image_slots:vec![] }).into(),
        corrections: vec![DesignCorrection { before: "Every page contains one picture and one repeated activity".into(), after: "Choose the number of related activities and pictures from the learning objective, with clear response space".into(), reason: "Preserve variation, useful practice and visual clarity".into() }],
    };
    let mut cover = Vec::new();
    header(&mut cover, "أكتشف وأتعلم", "EXPLORE & LEARN");
    text(&mut cover, "رحلة الاكتشاف", [54.0, 160.0, 540.0, 222.0], 30.0, true);
    text(&mut cover, "ألاحظ وأقارن وأجرب", [54.0, 235.0, 540.0, 287.0], 20.0, true);
    // A synthetic native illustration. It demonstrates editable construction, not AI output.
    block(&mut cover, [175.0, 350.0, 420.0, 530.0], "C=75 M=5 Y=100 K=0");
    text(&mut cover, "اسمي:", [54.0, 650.0, 540.0, 700.0], 20.0, true);
    let mut compare = Vec::new();
    header(&mut compare, "ألاحظ وأقارن", "LOOK & COMPARE");
    text(&mut compare, "أقارن المجموعتين", [54.0, 120.0, 540.0, 172.0], 26.0, true);
    text(&mut compare, "أعد الأشكال، ثم أرسم مجموعة أخرى.", [54.0, 182.0, 540.0, 234.0], 18.0, true);
    for x in [90.0, 165.0, 350.0, 425.0] {
        block(&mut compare, [x, 285.0, x + 50.0, 335.0], "C=15 M=100 Y=100 K=0");
    }
    text(&mut compare, "أكمل الرسم في المساحة التالية:", [54.0, 460.0, 540.0, 513.0], 18.0, true);
    let line = compare.len();
    compare.push(EngineAction { command: "line.create".into(), params: json!({"spread":0,"a":[70,660],"b":[525,660]}) });
    compare.push(EngineAction {
        command: "object.stroke".into(),
        params: json!({"ids":[format!("${line}.id")],"swatch":"C=100 M=90 Y=10 K=0","weight":1}),
    });
    let mut review = Vec::new();
    header(&mut review, "أراجع تعلمي", "MY REVIEW");
    text(&mut review, "أعد وأصل وأرسم", [54.0, 120.0, 540.0, 178.0], 26.0, true);
    for (y, instruction) in [(220.0, "أعد الأشكال وأكتب العدد."), (395.0, "أصل الأشياء المتشابهة."), (565.0, "أرسم شيئاً تعلمته اليوم.")]
    {
        text(&mut review, instruction, [54.0, y, 540.0, y + 48.0], 18.0, true);
        block(&mut review, [70.0, y + 65.0, 105.0, y + 100.0], "C=75 M=5 Y=100 K=0");
    }
    let templates = [("templates/cover.json", cover), ("templates/compare-practice.json", compare), ("templates/review.json", review)];
    for family in &mut profile.page_families {
        if let Some((_, actions)) = templates.iter().find(|(path, _)| *path == family.template) {
            family.text_slots = actions
                .iter()
                .enumerate()
                .filter_map(|(index, action)| {
                    let example = action.params.get("text")?.as_str()?;
                    if action.command != "frame.create" || example.contains("{{page}}") {
                        return None;
                    }
                    let size = actions
                        .get(index.saturating_add(1))
                        .and_then(|a| a.params.pointer("/attrs/size"))
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(18.0);
                    let max_chars = if size >= 26.0 {
                        24
                    } else if size <= 14.0 {
                        28
                    } else {
                        42
                    };
                    Some(DesignTextSlot {
                        id: format!("text-{index}"),
                        purpose: format!("Short editable text; use brief language. Example: {example}"),
                        command: index,
                        max_chars,
                    })
                })
                .collect();
        }
        family.image_slots = match family.id.as_str() {
            "cover" => vec![DesignImageSlot {
                id: "main-picture".into(),
                purpose: "Optional cover subject illustration, no text".into(),
                rect: [150.0, 320.0, 445.0, 570.0],
            }],
            "compare-practice" => vec![
                DesignImageSlot {
                    id: "left-picture".into(),
                    purpose: "First independently generated comparison illustration, no text".into(),
                    rect: [70.0, 260.0, 275.0, 435.0],
                },
                DesignImageSlot {
                    id: "right-picture".into(),
                    purpose: "Second independently generated comparison illustration, no text".into(),
                    rect: [320.0, 260.0, 525.0, 435.0],
                },
            ],
            _ => vec![],
        };
    }
    let manifest = NativeManifest {
        schema_version: 1, skill_id: id.into(), required_commands: templates.iter().flat_map(|(_, actions)| actions.iter().map(|a| a.command.clone())).collect::<std::collections::BTreeSet<_>>().into_iter().collect(),
        references: vec!["references/style-guide.md".into()], templates: templates.iter().map(|(path, _)| (*path).into()).collect(),
        sample_template: Some("templates/compare-practice.json".into()), external_tools: Vec::new(),
        compatibility_notes: "Original synthetic examples, not extracted publisher layouts. Requires installed Tahoma. All templates are checked locally before activation; educational and aesthetic acceptance remains human.".into(),
        design_profile: Some(profile),
    };
    let mut files = PackageFiles::from([
        ("SKILL.md".into(), format!("---\nname: {id}\ndescription: Original clear bilingual early-years educational design style. نمط تعليمي أصلي واضح لرياض الأطفال وكتيبات العلوم والرياضيات.\n---\nUse the structured design_profile and choose a declared native page family for each learning objective. Adapt content, not the print geometry. Never render a whole page as an image. All text, headers, footers, answer areas and shapes use native engine commands. Keep several activities or pictures when useful. Apply explicit recorded corrections. Human approval is required between production batches.\n").into_bytes()),
        ("designcraft.json".into(), serde_json::to_vec_pretty(&manifest)?),
        ("references/style-guide.md".into(), b"# Original educational style\n\nRules are executable planning data, not model weight training. These synthetic examples demonstrate native editing and safe A4 geometry. Verify learning objectives, developmental suitability, spelling, visual balance and printing with a human. Match illustration descriptions across images; no identity guarantee is implied. Do not claim alignment with a publisher.\n".to_vec()),
    ]);
    for (path, actions) in templates {
        files.insert(path.into(), serde_json::to_vec_pretty(&actions)?);
    }
    Ok(files)
}

fn text(actions: &mut Vec<EngineAction>, value: &str, rect: [f64; 4], size: f64, rtl: bool) {
    actions
        .push(EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":rect,"content":"text","text":value,"caret":false}) });
    actions.push(EngineAction {
        command: "type.char".into(),
        params: json!({"attrs":{"fontFamily":"Tahoma","fontStyle":"Regular","size":size,"leading":{"kind":"points","value":size + 8.0}}}),
    });
    actions.push(EngineAction {
        command: "type.para".into(),
        params: json!({"attrs":{"direction":if rtl {"rightToLeft"} else {"leftToRight"},"align":if rtl {"right"} else {"left"},"hyphenate":false}}),
    });
}
fn header(actions: &mut Vec<EngineAction>, ar: &str, en: &str) {
    text(actions, en, [54.0, 54.0, 300.0, 92.0], 12.0, false);
    text(actions, ar, [310.0, 54.0, 540.0, 92.0], 14.0, true);
    text(actions, "اكتشاف وتعلم | {{page}}", [54.0, 745.0, 540.0, 788.0], 12.0, true);
}
fn block(actions: &mut Vec<EngineAction>, rect: [f64; 4], swatch: &str) {
    let index = actions.len();
    actions.push(EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":rect,"content":"unassigned"}) });
    actions.push(EngineAction { command: "object.fill".into(), params: json!({"ids":[format!("${index}.id")],"swatch":swatch,"tint":0.2}) });
}
