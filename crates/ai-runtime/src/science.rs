//! Original, editable classroom fixture. Coordinates are millimetres converted to points;
//! every object is made by a vetted native command, never a flattened illustration.
use designcraft_ai_types::{EngineAction, protocol::ProductionPlan};
use designcraft_engine::Session;
use serde_json::{Value, json};
use std::path::Path;

const PT: f64 = 72.0 / 25.4;

#[derive(Default)]
struct Book {
    commands: Vec<EngineAction>,
}

impl Book {
    fn action(&mut self, command: &str, params: Value) -> usize {
        let index = self.commands.len();
        self.commands.push(EngineAction { command: command.into(), params });
        index
    }

    fn label(&mut self, index: usize, label: &str) {
        self.action("object.label", json!({"ids":[format!("${index}.id")],"label":label}));
    }

    fn text(&mut self, page: usize, rect: [f64; 4], text: &str, size: f64, arabic: bool, label: &str) {
        let index = self.action("frame.create", json!({"spread":page,"rect":rect.map(|x|x*PT),"content":"text","text":text,"caret":false}));
        self.action(
            "type.char",
            json!({"attrs":{"fontFamily":"Tahoma","fontStyle":"Regular","size":size,"fill":"[Black]",
                "language":if arabic {"Arabic"} else {"English: USA"},"leading":{"kind":"points","value":size*1.35}}}),
        );
        self.action(
            "type.para",
            json!({"attrs":{"align":if arabic {"right"} else {"left"},
                "direction":if arabic {"rightToLeft"} else {"leftToRight"},"hyphenate":false}}),
        );
        self.label(index, label);
    }

    fn shape(&mut self, page: usize, rect: [f64; 4], ellipse: bool, label: &str) -> usize {
        let index = self.action(
            "frame.create",
            json!({"spread":page,"rect":rect.map(|x|x*PT),"shape":if ellipse {"ellipse"} else {"rectangle"},"content":"unassigned"}),
        );
        self.label(index, label);
        index
    }

    fn fill(&mut self, index: usize, swatch: &str) {
        self.action("object.fill", json!({"ids":[format!("${index}.id")],"swatch":swatch}));
    }

    fn line(&mut self, page: usize, a: [f64; 2], b: [f64; 2], label: &str) {
        let index = self.action("line.create", json!({"spread":page,"a":a.map(|x|x*PT),"b":b.map(|x|x*PT)}));
        self.label(index, label);
    }

    /// Original hand-drawn Bezier geometry; each tuple is an anchor and its handles.
    fn curve(&mut self, page: usize, points: &[[f64; 6]], closed: bool, label: &str) {
        let anchors: Vec<Value> = points.iter().map(|p| json!({"p":[p[0]*PT,p[1]*PT],"in":[p[2]*PT,p[3]*PT],"out":[p[4]*PT,p[5]*PT]})).collect();
        let index = self.action("path.create", json!({"spread":page,"anchors":anchors,"closed":closed}));
        self.label(index, label);
    }

    fn header(&mut self, page: usize, title: &str, term: &str) {
        self.text(page, [15.0, 16.0, 195.0, 31.0], title, 26.0, true, "lesson-title-ar");
        self.text(page, [15.0, 33.0, 195.0, 43.0], term, 17.0, false, "lesson-term-en");
        self.line(page, [15.0, 47.0], [195.0, 47.0], "header-rule");
        self.text(page, [15.0, 274.0, 195.0, 282.0], &format!("العلوم • حواسي الخمس • {}", page + 1), 10.0, true, "page-number");
    }

    fn answer(&mut self, page: usize, prompt: &str) {
        self.text(page, [15.0, 171.0, 195.0, 188.0], prompt, 18.0, true, "activity-prompt-ar");
        self.shape(page, [16.0, 192.0, 194.0, 251.0], false, "student-drawing-space");
    }

    fn eye(&mut self, page: usize) {
        self.curve(
            page,
            &[[64.0, 88.0, 64.0, 88.0, 81.0, 62.0], [146.0, 88.0, 129.0, 62.0, 129.0, 114.0], [64.0, 88.0, 81.0, 114.0, 64.0, 88.0]],
            true,
            "eye-outline",
        );
        self.shape(page, [90.0, 73.0, 120.0, 103.0], true, "eye-iris");
        let pupil = self.shape(page, [99.0, 82.0, 111.0, 94.0], true, "eye-pupil");
        self.fill(pupil, "[Black]");
    }

    fn ear(&mut self, page: usize) {
        self.curve(
            page,
            &[
                [88.0, 102.0, 88.0, 102.0, 73.0, 85.0],
                [96.0, 62.0, 75.0, 62.0, 127.0, 53.0],
                [129.0, 86.0, 131.0, 60.0, 129.0, 103.0],
                [114.0, 113.0, 117.0, 99.0, 113.0, 127.0],
                [96.0, 115.0, 99.0, 127.0, 94.0, 109.0],
            ],
            false,
            "ear-outer",
        );
        self.curve(
            page,
            &[[93.0, 88.0, 93.0, 88.0, 86.0, 70.0], [116.0, 78.0, 110.0, 65.0, 122.0, 91.0], [102.0, 97.0, 107.0, 87.0, 98.0, 105.0]],
            false,
            "ear-inner",
        );
    }

    fn nose(&mut self, page: usize) {
        self.curve(
            page,
            &[
                [103.0, 63.0, 103.0, 63.0, 101.0, 81.0],
                [86.0, 101.0, 96.0, 98.0, 82.0, 109.0],
                [103.0, 109.0, 93.0, 115.0, 112.0, 119.0],
                [122.0, 103.0, 128.0, 112.0, 118.0, 94.0],
                [116.0, 66.0, 117.0, 85.0, 116.0, 66.0],
            ],
            false,
            "nose-outline",
        );
        self.curve(page, &[[93.0, 108.0, 93.0, 108.0, 94.0, 103.0], [101.0, 110.0, 101.0, 103.0, 101.0, 110.0]], false, "nose-nostril");
    }

    fn mouth(&mut self, page: usize) {
        self.curve(
            page,
            &[
                [69.0, 87.0, 69.0, 87.0, 84.0, 81.0],
                [104.0, 82.0, 99.0, 72.0, 111.0, 72.0],
                [141.0, 87.0, 126.0, 81.0, 126.0, 118.0],
                [69.0, 87.0, 86.0, 118.0, 69.0, 87.0],
            ],
            true,
            "mouth-lips",
        );
        self.curve(page, &[[70.0, 88.0, 70.0, 88.0, 94.0, 95.0], [140.0, 88.0, 116.0, 95.0, 140.0, 88.0]], false, "mouth-smile");
        self.shape(page, [98.0, 99.0, 113.0, 113.0], true, "tongue");
    }

    fn hand(&mut self, page: usize) {
        let outline = [
            [91.0, 123.0, 91.0, 123.0, 90.0, 115.0],
            [74.0, 99.0, 81.0, 108.0, 69.0, 90.0],
            [81.0, 94.0, 73.0, 86.0, 87.0, 100.0],
            [85.0, 67.0, 87.0, 83.0, 84.0, 60.0],
            [93.0, 66.0, 92.0, 59.0, 95.0, 78.0],
            [97.0, 59.0, 95.0, 64.0, 98.0, 53.0],
            [105.0, 59.0, 105.0, 53.0, 105.0, 72.0],
            [109.0, 63.0, 105.0, 60.0, 110.0, 57.0],
            [117.0, 65.0, 117.0, 58.0, 117.0, 78.0],
            [121.0, 74.0, 117.0, 71.0, 123.0, 67.0],
            [129.0, 77.0, 129.0, 69.0, 129.0, 96.0],
            [121.0, 123.0, 128.0, 115.0, 121.0, 123.0],
        ];
        self.curve(page, &outline, false, "hand-outline");
        self.line(page, [92.0, 111.0], [113.0, 106.0], "hand-palm");
    }

    fn plant(&mut self, page: usize, x: f64, y: f64) {
        self.line(page, [x + 12.0, y + 5.0], [x + 12.0, y + 45.0], "plant-stem");
        self.curve(
            page,
            &[
                [x + 12.0, y + 25.0, x + 12.0, y + 25.0, x - 5.0, y + 24.0],
                [x - 7.0, y + 8.0, x - 7.0, y + 20.0, x + 9.0, y + 8.0],
                [x + 12.0, y + 25.0, x + 12.0, y + 14.0, x + 12.0, y + 25.0],
            ],
            true,
            "plant-left-leaf",
        );
        self.curve(
            page,
            &[
                [x + 12.0, y + 16.0, x + 12.0, y + 16.0, x + 26.0, y + 16.0],
                [x + 31.0, y, x + 31.0, y + 12.0, x + 15.0, y],
                [x + 12.0, y + 16.0, x + 12.0, y + 5.0, x + 12.0, y + 16.0],
            ],
            true,
            "plant-right-leaf",
        );
        self.line(page, [x - 3.0, y + 45.0], [x + 27.0, y + 45.0], "plant-ground");
    }

    fn sun(&mut self, page: usize, x: f64, y: f64) {
        self.shape(page, [x, y, x + 20.0, y + 20.0], true, "sun-disc");
        for (a, b) in [([-5.0, 10.0], [-12.0, 10.0]), ([25.0, 10.0], [32.0, 10.0]), ([10.0, -5.0], [10.0, -12.0]), ([10.0, 25.0], [10.0, 32.0])] {
            self.line(page, [x + a[0], y + a[1]], [x + b[0], y + b[1]], "sun-ray");
        }
    }
}

/// An original eight-page science booklet for ages five to six. No external assets or IO.
pub fn science_plan() -> ProductionPlan {
    let mut b = Book::default();
    b.action(
        "layout.documentSetup",
        json!({"width":210.0*PT,"height":297.0*PT,"pages":8,
        "margins":15.0*PT,"bleed":0,"slug":0,"facingPages":false,"binding":"rightToLeft","intent":"print"}),
    );

    b.header(0, "أكتشف بحواسي", "My Five Senses");
    b.text(0, [15.0, 55.0, 195.0, 75.0], "كراسة العلوم • ٥–٦ سنوات", 22.0, true, "cover-subtitle-ar");
    b.plant(0, 91.0, 91.0);
    b.sun(0, 144.0, 96.0);
    b.text(0, [15.0, 151.0, 195.0, 169.0], "أنظر • أستمع • أشم • أتذوق • ألمس", 21.0, true, "cover-senses-ar");
    b.text(0, [15.0, 190.0, 195.0, 207.0], "اسمي:", 20.0, true, "student-name-label");
    b.line(0, [20.0, 216.0], [168.0, 216.0], "student-name-answer");
    b.text(0, [15.0, 231.0, 195.0, 249.0], "صفي:", 20.0, true, "student-class-label");
    b.line(0, [20.0, 258.0], [168.0, 258.0], "student-class-answer");

    b.header(1, "أنا ألاحظ الطبيعة", "Observe • Nature");
    b.plant(1, 66.0, 68.0);
    b.sun(1, 135.0, 75.0);
    b.text(1, [15.0, 125.0, 195.0, 159.0], "أنظر إلى نبتة. ما شكل أوراقها؟\nأحافظ على النبتة ولا أقطفها.", 20.0, true, "observation-ar");
    b.answer(1, "أرسم ورقة كما أراها:");

    b.header(2, "أرى بعيني", "Sight • Eye");
    b.eye(2);
    b.text(2, [15.0, 127.0, 195.0, 160.0], "أرى الأشكال والألوان.\nأبحث عن دائرة في صفي.", 20.0, true, "sight-explanation-ar");
    b.answer(2, "أرسم شيئاً دائرياً رأيته:");

    b.header(3, "أسمع بأذني", "Hearing • Ear");
    b.ear(3);
    b.text(3, [15.0, 132.0, 195.0, 164.0], "أستمع بهدوء إلى صوت قريب.\nأبتعد عن الأصوات العالية.", 20.0, true, "hearing-explanation-ar");
    b.answer(3, "أرسم مصدر صوت هادئ:");

    b.header(4, "أشم بأنفي", "Smell • Nose");
    b.nose(4);
    b.text(4, [15.0, 128.0, 195.0, 161.0], "أتعرف إلى رائحة زهرة من بعيد.\nلا أشم مادة مجهولة.", 20.0, true, "smell-explanation-ar");
    b.answer(4, "أرسم زهرة أحب رائحتها:");

    b.header(5, "أتذوق بلساني", "Taste • Tongue");
    b.mouth(5);
    b.text(5, [15.0, 128.0, 195.0, 162.0], "أتذكر طعم طعام أعرفه.\nلا أتذوق شيئاً مجهولاً.", 20.0, true, "taste-explanation-ar");
    b.answer(5, "أرسم طعاماً مألوفاً أحبه:");

    b.header(6, "ألمس بيدي", "Touch • Hand");
    b.hand(6);
    b.text(6, [15.0, 132.0, 195.0, 164.0], "ألمس قطعة قماش ناعمة ونظيفة.\nأبتعد عن الساخن والحاد.", 20.0, true, "touch-explanation-ar");
    b.answer(6, "أرسم شيئاً ناعماً وآمناً:");

    b.header(7, "أراجع ما تعلمت", "Look • Listen • Smell • Taste • Touch");
    b.text(7, [15.0, 58.0, 195.0, 76.0], "أصل كل عضو بما أفعله به:", 21.0, true, "review-instruction-ar");
    // Large separated rows deliberately give children room to draw their own connecting lines.
    for (row, (organ, verb)) in [("العين", "ألمس"), ("الأذن", "أشم"), ("الأنف", "أرى"), ("اللسان", "أسمع"), ("اليد", "أتذوق")].iter().enumerate()
    {
        let y = 87.0 + row as f64 * 26.0;
        b.text(7, [129.0, y, 190.0, y + 16.0], organ, 22.0, true, "review-organ-ar");
        b.text(7, [20.0, y, 79.0, y + 16.0], verb, 22.0, true, "review-action-ar");
    }
    b.text(7, [15.0, 226.0, 195.0, 244.0], "أذكر قاعدة أمان تعلمتها:", 19.0, true, "review-safety-question");
    b.line(7, [20.0, 258.0], [190.0, 258.0], "review-safety-answer");
    ProductionPlan { title: "أكتشف بحواسي — كراسة علوم أصلية".into(), commands: b.commands, images: Vec::new() }
}

/// Build a new document, validate it and export exact A4 single pages with no bleed or marks.
/// Existing open documents are retained. Callers must initialize their normal system fonts.
pub fn build_science_sample(session: &mut Session, directory: &Path) -> Result<Vec<String>, String> {
    designcraft_engine::file_access::check_write(directory)?;
    for name in ["science-preflight.json", "my-five-senses.designcraft", "my-five-senses.pdf"] {
        designcraft_engine::file_access::check_write(directory.join(name))?;
    }
    for page in 1..=8 {
        designcraft_engine::file_access::check_write(directory.join(format!("science-page-{page}.png")))?;
    }
    std::fs::create_dir_all(directory).map_err(|e| format!("create science output directory: {e}"))?;
    let plan = science_plan();
    session
        .execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false,"margins":15.0*PT,"bleed":0,"title":plan.title}))
        .map_err(|e| e.to_string())?;
    session
        .execute(
            "production.apply",
            &json!({"commands":plan.commands,"outputMode":"native_only","label":"Original science booklet","requirePreflight":true}),
        )
        .map_err(|e| e.to_string())?;
    let issues = session.execute("preflight.run", &json!({})).map_err(|e| e.to_string())?;
    let preflight = directory.join("science-preflight.json");
    designcraft_engine::io::write_file(&preflight, &serde_json::to_vec_pretty(&issues).map_err(|e| e.to_string())?)?;
    if issues.get("errors").and_then(Value::as_u64) != Some(0) {
        return Err(format!("science preflight failed; inspect {}", preflight.display()));
    }
    let source = directory.join("my-five-senses.designcraft");
    session.execute("file.saveAs", &json!({"path":source})).map_err(|e| e.to_string())?;
    let pdf = directory.join("my-five-senses.pdf");
    session
        .execute("file.exportPdf", &json!({"path":pdf,"spreads":false,"bleed":false,"marks":false,"pageLayout":"single","title":plan.title}))
        .map_err(|e| e.to_string())?;
    let mut artifacts = vec![source.to_string_lossy().into_owned(), pdf.to_string_lossy().into_owned(), preflight.to_string_lossy().into_owned()];
    let mut renderer = designcraft_render::Renderer::new();
    for page in 0..8 {
        let state = session.doc().map_err(|e| e.to_string())?;
        let image = renderer
            .render_page(&state.doc, &session.cache, page, 1.0, false, &designcraft_render::RenderOptions::default())
            .ok_or_else(|| format!("cannot render science page {}", page + 1))?;
        let path = directory.join(format!("science-page-{}.png", page + 1));
        let png = image.to_png();
        if png.is_empty() {
            return Err(format!("empty science preview for page {}", page + 1));
        }
        designcraft_engine::io::write_file(&path, &png)?;
        artifacts.push(path.to_string_lossy().into_owned());
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_exact_a4_pages_are_native_and_editable_in_one_undo_step() {
        let plan = science_plan();
        assert!(plan.images.is_empty());
        assert!(plan.commands.len() <= 512);
        assert!(!plan.commands.iter().any(|action| action.command == "file.place"));
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1})).unwrap();
        let original = session.doc().unwrap().doc.clone();
        session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only"})).unwrap();
        let document = &session.doc().unwrap().doc;
        assert_eq!(document.page_count(), 8);
        assert!(document.assets.is_empty());
        assert!(document.settings.bleed.iter().all(|edge| *edge == 0.0));
        assert!(document.settings.slug.iter().all(|edge| *edge == 0.0));
        assert!(document.spreads.iter().all(|spread| spread.pages.len() == 1));
        for page in document.spreads.iter().flat_map(|spread| &spread.pages) {
            assert!((page.width - 210.0 * PT).abs() < 0.001);
            assert!((page.height - 297.0 * PT).abs() < 0.001);
            assert!((page.margins.top - 15.0 * PT).abs() < 0.001);
            assert!((page.margins.bottom - 15.0 * PT).abs() < 0.001);
            assert!((page.margins.inside - 15.0 * PT).abs() < 0.001);
            assert!((page.margins.outside - 15.0 * PT).abs() < 0.001);
        }
        let all_text: String = document.stories.values().map(|story| story.text.as_str()).collect();
        for term in ["Sight", "Hearing", "Smell", "Taste", "Touch", "لا أتذوق شيئاً مجهولاً"] {
            assert!(all_text.contains(term), "missing editable term {term}");
        }
        assert!(document.spreads.iter().flat_map(|spread| &spread.items).all(|item| !item.label.is_empty()));
        session.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().doc, original);
        session.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().doc.page_count(), 8);
    }

    #[test]
    fn original_illustrations_are_paths_and_shape_commands_with_named_references() {
        let plan = science_plan();
        let labels: Vec<&str> = plan
            .commands
            .iter()
            .filter_map(|a| (a.command == "object.label").then(|| a.params.get("label").and_then(Value::as_str)).flatten())
            .collect();
        for label in ["eye-outline", "ear-outer", "nose-outline", "mouth-lips", "hand-outline", "plant-left-leaf", "student-drawing-space"] {
            assert!(labels.contains(&label));
        }
        assert!(plan.commands.iter().any(|a| a.command == "path.create"));
        assert!(
            plan.commands
                .iter()
                .filter(|a| a.command == "object.label")
                .all(|a| a.params["ids"][0].as_str().is_some_and(|id| id.starts_with('$') && id.ends_with(".id")))
        );
    }

    #[test]
    fn all_text_frames_stay_inside_print_margins_without_overlapping() {
        let plan = science_plan();
        let frames: Vec<_> = plan.commands.iter().filter(|action| action.command == "frame.create" && action.params["content"] == "text").collect();
        for (index, frame) in frames.iter().enumerate() {
            let bounds: Vec<f64> = frame.params["rect"].as_array().unwrap().iter().map(|coordinate| coordinate.as_f64().unwrap()).collect();
            assert!(bounds[0] >= 15.0 * PT && bounds[1] >= 15.0 * PT);
            assert!(bounds[2] <= 195.0 * PT && bounds[3] <= 282.0 * PT);
            for other in frames.iter().skip(index + 1).filter(|other| other.params["spread"] == frame.params["spread"]) {
                let rect: Vec<f64> = other.params["rect"].as_array().unwrap().iter().map(|coordinate| coordinate.as_f64().unwrap()).collect();
                assert!(bounds[2] <= rect[0] || rect[2] <= bounds[0] || bounds[3] <= rect[1] || rect[3] <= bounds[1]);
            }
        }
    }

    #[test]
    fn saved_booklet_reopens_with_independently_editable_text_and_path_anchors() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("original.designcraft");
        let edited_path = directory.path().join("edited-copy.designcraft");
        let mut author = Session::new();
        author.execute("file.new", &json!({"preset":"A4","pages":1})).unwrap();
        author.execute("production.apply", &json!({"commands":science_plan().commands,"outputMode":"native_only"})).unwrap();
        author.execute("file.saveAs", &json!({"path":source_path})).unwrap();
        let saved = author.doc().unwrap().doc.clone();
        let original_bytes = std::fs::read(&source_path).unwrap();

        let mut reopened = Session::new();
        let result = reopened.execute("file.open", &json!({"path":source_path})).unwrap();
        assert!(result["warnings"].as_array().unwrap().is_empty(), "{result}");
        assert_eq!(reopened.doc().unwrap().doc, saved, "native text, paths, labels and formatting must survive saving");
        assert!(reopened.doc().unwrap().history.undo.is_empty());
        let initial = reopened.doc().unwrap().doc.clone();
        let text_frame = initial.spreads.iter().flat_map(|spread| &spread.items).find(|item| item.label == "sight-explanation-ar").unwrap();
        let story_id = text_frame.text_frame().unwrap().story;
        let original_text = initial.stories.get(&story_id).unwrap().text.clone();
        let replacement_end = original_text.find(' ').unwrap();
        let edited_text = original_text.replacen("أرى", "ألاحظ", 1);
        let eye = initial.spreads.iter().flat_map(|spread| &spread.items).find(|item| item.label == "eye-outline").unwrap();
        assert!(!eye.is_text_frame());
        let eye_id = eye.id;
        let original_path = eye.path.clone();
        let anchor = original_path.subpaths[0].anchors[0].p;

        reopened.execute("story.replaceRange", &json!({"story":story_id.0,"start":0,"end":replacement_end,"text":"ألاحظ"})).unwrap();
        assert_eq!(reopened.doc().unwrap().doc.stories.get(&story_id).unwrap().text, edited_text);
        assert_eq!(reopened.doc().unwrap().doc.item(eye_id).unwrap().path, original_path);
        assert_eq!(reopened.doc().unwrap().history.undo.len(), 1);
        let text_edited = reopened.doc().unwrap().doc.clone();
        reopened.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(reopened.doc().unwrap().doc, initial);
        reopened.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(reopened.doc().unwrap().doc, text_edited);

        // Edit real Bezier geometry, rather than merely moving a flattened image frame.
        reopened.execute("path.setAnchor", &json!({"id":eye_id.0,"anchor":0,"p":[anchor.x+2.0,anchor.y+3.0]})).unwrap();
        let changed = reopened.doc().unwrap().doc.item(eye_id).unwrap();
        assert_ne!(changed.path, original_path);
        let moved = changed.path.subpaths[0].anchors[0].p;
        assert!((moved.x - anchor.x - 2.0).abs() < 1e-6 && (moved.y - anchor.y - 3.0).abs() < 1e-6);
        assert_eq!(changed.xf, eye.xf);
        assert_eq!(reopened.doc().unwrap().doc.stories.get(&story_id).unwrap().text, edited_text);
        assert!(reopened.doc().unwrap().doc.assets.is_empty());
        assert_eq!(reopened.doc().unwrap().history.undo.len(), 2);
        let both_edited = reopened.doc().unwrap().doc.clone();
        reopened.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(reopened.doc().unwrap().doc, text_edited, "shape undo must retain the text edit");
        reopened.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(reopened.doc().unwrap().doc, both_edited);

        reopened.execute("file.saveAs", &json!({"path":edited_path})).unwrap();
        let mut edited_copy = Session::new();
        edited_copy.execute("file.open", &json!({"path":edited_path})).unwrap();
        assert_eq!(edited_copy.doc().unwrap().doc, both_edited, "edited native text and geometry must remain editable after another load");
        assert_eq!(std::fs::read(&source_path).unwrap(), original_bytes, "editing and saving a copy must not change the original file");
    }
}
