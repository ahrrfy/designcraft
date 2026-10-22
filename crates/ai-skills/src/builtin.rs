use crate::{NativeManifest, PackageFiles, Result};
use designcraft_ai_types::EngineAction;
use serde_json::json;

/// Native instructions and synthetic samples, including attributed adaptations.
/// No source PDFs, fonts or third-party artwork. All packages begin unapproved.
pub(crate) fn packages() -> Result<Vec<PackageFiles>> {
    let mut packages = [
        ("early-years-booklet-planning", "Plan a clear early-years booklet from teacher requirements, age range and learning outcomes. تخطيط كتيب وكراس وملزمة ومنهج علوم ورياضيات وحاسوب لرياض الأطفال حسب العمر والأهداف التعليمية.",
         "Confirm age range, language, page count, learning objectives, available paper and binding. Produce a page-by-page plan before layout. Each activity needs a clear objective and short instruction; a page may contain several related activities, subtopics and pictures. Choose observation, comparison, matching, sequence, structured table (`table.insert`, `table.setCellStyle`, `table.setTableStyle`), bulleted/numbered list (`type.bulletedList`, `type.numberedList`), or mixed layouts according to the brief rather than repeating a single-picture template. Define reusable paragraph, character and object styles (`style.paragraph.create`/`apply`, `style.character.create`/`apply`, `style.object.create`/`apply`) for consistent headings, instructions and cards across pages. Keep tasks suitable for the stated developmental stage. Do not invent school branding, curriculum alignment, prices or permissions. Use native editable text and shapes. Obtain teacher review of learning content and visual review before production.",
         "Booklet planning sample", "1. Cover and pupil name\n2. Trace a line\n3. Count three circles\n4. Teacher review", "Check reading level, pencil control, realistic page count and safe binding margins."),
        ("early-years-activity-layout", "Lay out simple, editable early-years activities with generous spacing and clear task areas. تصميم وتوزيع أنشطة ورسومات تعليمية قابلة للتحرير بمساحات واضحة ومريحة للأطفال.",
         "Use a traditional flat page with aligned sections, clear headings and spacious activity areas. Each activity has its own short instruction; several related activities and independent illustrations may share a page. Choose the visual density and picture count from the brief, not a fixed one-picture or one-topic rule. Keep type straight and separate from task marks. Use native vector shapes, Pathfinder Boolean operations (`object.pathfinder`), compound paths (`object.compoundPath.make`), shape conversions (`object.convertShape`), gradients (`object.gradientFill`), effects (`object.effects`), and step-and-repeat grids (`edit.stepAndRepeat`) for tracing, diagrams and counting when supported, plus structured tables (`table.insert`, `table.mergeCells`, `table.setCellStyle`) and QR codes (`object.qrCode`) where appropriate. Use realistic images when requested, within separate native frames. Keep answer areas and writing lines clear. Preserve exact user dimensions and margins. Never silently substitute an image for an editable activity.",
         "Activity layout sample", "Name: ____________________\nDraw a line from each number to its group.\n\n1                       one circle\n2                       two circles\n3                       three circles", "Inspect every object boundary, writing area, contrast and instruction length."),
        ("arabic-english-typography", "Check Arabic and English typography, text direction and editable bilingual classroom layouts. تنسيق الخطوط والنصوص العربية والإنجليزية واتجاه الفقرات والمحاذاة في الصفحات التعليمية.",
         "Keep Arabic as logical Unicode text and use native shaping and paragraph direction. Use RTL paragraphs for Arabic and LTR for English; review mixed numbers, punctuation and parentheses visually. Do not reverse character strings or rasterize text to hide shaping problems. Verify the actual selected font supports every glyph and intended print size. Use explicit aligned text areas with sufficient leading, reusable paragraph/character styles (`style.paragraph.create`/`apply`, `style.character.create`/`apply`), native bulleted/numbered lists (`type.bulletedList`, `type.numberedList`), and multi-column story threading (`text.thread`, `text.unthread`) when flowing long bilingual copy across frames. Preserve supplied copy exactly using `frame.create` `text` or `story.setText` (never `type.set`). Native command capability determines available direction and style operations; report unsupported typography controls rather than simulating them.",
         "Arabic / English typography", "نشاط الأرقام ١ ٢ ٣\nNumbers activity 1 2 3\nالاسم: ____________________\nName: ____________________", "Review connected Arabic glyphs, missing-glyph boxes, mixed-script punctuation, baselines and overflow."),
        ("school-print-preflight", "Prepare school print checks for exact page dimensions, margins, text overflow and production handoff. فحص طباعة المواد المدرسية ومقاس A4 وهوامش الأمان وتجاوز النصوص قبل التسليم للمطبعة.",
         "Require the printer's trim dimensions, bleed, safe area, duplex orientation, binding and color requirements. Record values in millimetres and convert explicitly to native points where needed. Check every page for overflow, clipped text, overlapping objects, missing glyphs and image resolution at placed size. Enlarging a small image does not restore detail. Keep content editable and review exported page geometry. Native output availability does not prove physical printing: request a paper proof for duplex order and binding alignment. Report unsupported CMYK, ICC or PDF standard requirements honestly.",
         "School print preflight", "Print review checklist\n[ ] Page size and safe margins\n[ ] No text overflow\n[ ] Arabic and English glyphs\n[ ] Duplex paper proof", "Check trim, bleed, page order, font availability and actual exported dimensions; record unresolved production constraints."),
    ].into_iter().map(|(id, description, instructions, title, body, review)| {
        let mut actions = Vec::new();
        append_text(&mut actions, title, [54.0, 54.0, 540.0, 112.0], 22.0, false);
        // Separate native text frames preserve each paragraph's own direction
        // without relying on unstable story IDs or reversing Arabic text. All
        // bounds lie inside the A4 15 mm safe area with extra tolerance.
        for (index, line) in body.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let top = 140.0 + index as f64 * 44.0;
            let rtl = line.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c));
            append_text(&mut actions, line, [54.0, top, 540.0, top + 38.0], 18.0, rtl);
        }
        let manifest = NativeManifest {
            design_profile: None,
            schema_version: 1, skill_id: id.into(), required_commands: vec!["frame.create".into(), "type.char".into(), "type.para".into()],
            references: vec!["references/review.md".into()], templates: vec!["templates/sample.json".into()],
            sample_template: Some("templates/sample.json".into()), external_tools: Vec::new(),
            compatibility_notes: "Sample requires a scratch A4-or-larger page and installed Tahoma Regular. All frames stay at least 54 pt inside A4 trim; text uses explicit native font, size, leading and paragraph direction. Arabic and English paragraphs occupy separate editable frames. This package is a draft until production preflight passes and a human reviews the actual render.".into(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let actions_bytes = serde_json::to_vec_pretty(&actions)?;
        Ok(PackageFiles::from([
            ("SKILL.md".into(), format!("---\nname: {id}\ndescription: {description}\n---\n{instructions}\n").into_bytes()),
            ("designcraft.json".into(), manifest_bytes),
            ("templates/sample.json".into(), actions_bytes),
            ("references/review.md".into(), format!("# Human sample review\n\n{review}\n").into_bytes()),
        ]))
    }).collect::<Result<Vec<_>>>()?;
    packages.extend([
        embedded(&[
            ("SKILL.md", include_bytes!("../skills/native-design-composition/SKILL.md")),
            ("designcraft.json", include_bytes!("../skills/native-design-composition/designcraft.json")),
            ("templates/sample.json", include_bytes!("../skills/native-design-composition/templates/sample.json")),
            ("references/layout-contract.md", include_bytes!("../skills/native-design-composition/references/layout-contract.md")),
            ("references/LICENSE.txt", include_bytes!("../skills/native-design-composition/references/LICENSE.txt")),
            ("references/NOTICE.md", include_bytes!("../skills/native-design-composition/references/NOTICE.md")),
        ]),
        embedded(&[
            ("SKILL.md", include_bytes!("../skills/native-design-quality/SKILL.md")),
            ("designcraft.json", include_bytes!("../skills/native-design-quality/designcraft.json")),
            ("templates/sample.json", include_bytes!("../skills/native-design-quality/templates/sample.json")),
            ("references/layout-contract.md", include_bytes!("../skills/native-design-quality/references/layout-contract.md")),
            ("references/LICENSE.txt", include_bytes!("../skills/native-design-quality/references/LICENSE.txt")),
            ("references/NOTICE.md", include_bytes!("../skills/native-design-quality/references/NOTICE.md")),
        ]),
        embedded(&[
            ("SKILL.md", include_bytes!("../skills/native-engine-workflow/SKILL.md")),
            ("designcraft.json", include_bytes!("../skills/native-engine-workflow/designcraft.json")),
            ("templates/sample.json", include_bytes!("../skills/native-engine-workflow/templates/sample.json")),
            ("references/layout-contract.md", include_bytes!("../skills/native-engine-workflow/references/layout-contract.md")),
            ("references/NOTICE.md", include_bytes!("../skills/native-engine-workflow/references/NOTICE.md")),
        ]),
        embedded(&[
            ("SKILL.md", include_bytes!("../skills/native-illustration/SKILL.md")),
            ("designcraft.json", include_bytes!("../skills/native-illustration/designcraft.json")),
            ("templates/sample.json", include_bytes!("../skills/native-illustration/templates/sample.json")),
            ("references/drawing-contract.md", include_bytes!("../skills/native-illustration/references/drawing-contract.md")),
            ("references/NOTICE.md", include_bytes!("../skills/native-illustration/references/NOTICE.md")),
        ]),
    ]);
    packages.push(crate::educational_style::package()?);
    Ok(packages)
}

fn embedded(files: &[(&str, &[u8])]) -> PackageFiles {
    files.iter().map(|(path, bytes)| ((*path).into(), bytes.to_vec())).collect()
}

fn append_text(actions: &mut Vec<EngineAction>, text: &str, rect: [f64; 4], size: f64, rtl: bool) {
    actions.push(EngineAction { command: "frame.create".into(), params: json!({"rect":rect,"content":"text","text":text,"caret":false}) });
    actions.push(EngineAction {
        command: "type.char".into(),
        params: json!({"attrs":{"fontFamily":"Tahoma","fontStyle":"Regular","size":size,"leading":{"kind":"points","value":size + 8.0}}}),
    });
    actions.push(EngineAction {
        command: "type.para".into(),
        params: json!({"attrs":{"direction":if rtl { "rightToLeft" } else { "leftToRight" },"align":if rtl { "right" } else { "left" },"hyphenate":false}}),
    });
}
