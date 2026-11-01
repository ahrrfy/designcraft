//! Clean-room native Planners, Journals, Notebooks & 365 Stationery Studio packages.
//! No third-party mockups, publisher artwork, or external Python scripts are embedded.
use crate::{NativeManifest, PackageFiles, Result};
use designcraft_ai_types::{DesignCorrection, DesignImageSlot, DesignProfile, DesignTextSlot, EngineAction, PageFamily};
use serde_json::json;
use std::collections::BTreeSet;

const NAVY: &str = "C=100 M=90 Y=10 K=0";
const ROSE: &str = "C=15 M=100 Y=100 K=0";
const BLOSSOM: &str = "C=0 M=100 Y=0 K=0";
const SAGE: &str = "C=75 M=5 Y=100 K=0";
const SKY: &str = "C=100 M=0 Y=0 K=0";
const GOLD: &str = "C=0 M=0 Y=100 K=0";

struct SlotSpec {
    id: &'static str,
    purpose: &'static str,
    command: usize,
    max_chars: usize,
}

struct FamilyBuilder {
    actions: Vec<EngineAction>,
    slots: Vec<SlotSpec>,
}

impl FamilyBuilder {
    fn new() -> Self {
        Self { actions: Vec::new(), slots: Vec::new() }
    }

    fn card(&mut self, rect: [f64; 4], fill: (&str, f64), stroke: (&str, f64, f64), label: &str) {
        let (fill_swatch, fill_tint) = fill;
        let (stroke_swatch, stroke_tint, weight) = stroke;
        let [x0, y0, x1, y1] = rect;
        let r = ((x1 - x0).min(y1 - y0) * 0.18).clamp(3.0, 8.0);
        let k = r * 0.552;
        let idx = self.actions.len();
        self.actions.push(EngineAction {
            command: "path.create".into(),
            params: json!({
                "spread": 0,
                "closed": true,
                "anchors": [
                    {"p": [x0 + r, y0], "in": [x0 + r - k, y0]},
                    {"p": [x1 - r, y0], "out": [x1 - r + k, y0]},
                    {"p": [x1, y0 + r], "in": [x1, y0 + r - k]},
                    {"p": [x1, y1 - r], "out": [x1, y1 - r + k]},
                    {"p": [x1 - r, y1], "in": [x1 - r + k, y1]},
                    {"p": [x0 + r, y1], "out": [x0 + r - k, y1]},
                    {"p": [x0, y1 - r], "in": [x0, y1 - r + k]},
                    {"p": [x0, y0 + r], "out": [x0, y0 + r - k]}
                ]
            }),
        });
        self.actions.push(EngineAction {
            command: "object.fill".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": fill_swatch, "tint": fill_tint}),
        });
        self.actions.push(EngineAction {
            command: "object.stroke".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": stroke_swatch, "tint": stroke_tint, "weight": weight}),
        });
        self.actions.push(EngineAction { command: "object.label".into(), params: json!({"ids": [format!("${idx}.id")], "label": label}) });
    }

    fn ellipse(&mut self, rect: [f64; 4], fill_swatch: &str, fill_tint: f64, stroke_swatch: &str, stroke_tint: f64, weight: f64) {
        let idx = self.actions.len();
        self.actions.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": rect, "content": "unassigned", "shape": "ellipse"}),
        });
        self.actions.push(EngineAction {
            command: "object.fill".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": fill_swatch, "tint": fill_tint}),
        });
        self.actions.push(EngineAction {
            command: "object.stroke".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": stroke_swatch, "tint": stroke_tint, "weight": weight}),
        });
    }

    fn graphic_slot_frame(&mut self, rect: [f64; 4], accent: &str, label: &str) {
        let idx = self.actions.len();
        self.actions.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": rect, "content": "graphic", "shape": "rectangle"}),
        });
        self.actions
            .push(EngineAction { command: "object.fill".into(), params: json!({"ids": [format!("${idx}.id")], "swatch": accent, "tint": 0.08}) });
        self.actions.push(EngineAction {
            command: "object.stroke".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": NAVY, "tint": 0.3, "weight": 0.65, "align": "inside"}),
        });
        self.actions.push(EngineAction { command: "object.label".into(), params: json!({"ids": [format!("${idx}.id")], "label": label}) });
    }

    fn rule(&mut self, a: [f64; 2], b: [f64; 2], swatch: &str, tint: f64, weight: f64, label: Option<&str>) {
        let idx = self.actions.len();
        self.actions.push(EngineAction { command: "line.create".into(), params: json!({"spread": 0, "a": a, "b": b}) });
        self.actions.push(EngineAction {
            command: "object.stroke".into(),
            params: json!({"ids": [format!("${idx}.id")], "swatch": swatch, "tint": tint, "weight": weight}),
        });
        if let Some(lbl) = label {
            self.actions.push(EngineAction { command: "object.label".into(), params: json!({"ids": [format!("${idx}.id")], "label": lbl}) });
        }
    }

    fn rules(&mut self, segments: &[[[f64; 2]; 2]], swatch: &str, tint: f64, weight: f64) {
        if segments.is_empty() {
            return;
        }
        let mut ids = Vec::with_capacity(segments.len());
        for [a, b] in segments {
            let idx = self.actions.len();
            self.actions.push(EngineAction { command: "line.create".into(), params: json!({"spread": 0, "a": a, "b": b}) });
            ids.push(format!("${idx}.id"));
        }
        self.actions
            .push(EngineAction { command: "object.stroke".into(), params: json!({"ids": ids, "swatch": swatch, "tint": tint, "weight": weight}) });
    }

    #[allow(clippy::too_many_arguments)]
    fn slot(
        &mut self,
        id: &'static str,
        purpose: &'static str,
        sample: &str,
        rect: [f64; 4],
        size: f64,
        bold: bool,
        align: &str,
        fill: &str,
        max_chars: usize,
    ) {
        let cmd = self.actions.len();
        let rtl = sample.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c));
        self.actions.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": rect, "content": "text", "text": sample, "caret": false}),
        });
        self.actions.push(EngineAction {
            command: "type.char".into(),
            params: json!({
                "attrs": {
                    "fontFamily": "Tahoma",
                    "fontStyle": if bold { "Bold" } else { "Regular" },
                    "size": size,
                    "leading": {"kind": "points", "value": size + 3.5},
                    "fill": fill
                }
            }),
        });
        self.actions.push(EngineAction {
            command: "type.para".into(),
            params: json!({
                "attrs": {
                    "direction": if rtl { "rightToLeft" } else { "leftToRight" },
                    "align": align,
                    "hyphenate": false,
                    "spaceBefore": 0,
                    "spaceAfter": 0
                }
            }),
        });
        self.actions.push(EngineAction {
            command: "object.label".into(),
            params: json!({"ids": [format!("${cmd}.id")], "label": format!("part.slot.{id}")}),
        });
        self.slots.push(SlotSpec { id, purpose, command: cmd, max_chars });
    }

    fn footer_chrome(&mut self) {
        self.rule([40.0, 534.0], [379.0, 534.0], NAVY, 0.28, 0.55, Some("part.ui.footer.rule"));
        let cmd = self.actions.len();
        self.actions.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": [332.0, 537.0, 379.0, 555.0], "content": "text", "text": "\u{e000}", "caret": false}),
        });
        self.actions.push(EngineAction {
            command: "type.char".into(),
            params: json!({"attrs": {"fontFamily": "Tahoma", "fontStyle": "Bold", "size": 8.5, "leading": {"kind": "points", "value": 11.5}, "fill": NAVY}}),
        });
        self.actions.push(EngineAction {
            command: "type.para".into(),
            params: json!({"attrs": {"direction": "leftToRight", "align": "right", "hyphenate": false}}),
        });
        self.actions
            .push(EngineAction { command: "object.label".into(), params: json!({"ids": [format!("${cmd}.id")], "label": "part.ui.footer.page"}) });
    }

    fn finish(self, id: &str, purpose: &str, template: &str, image_slots: Vec<DesignImageSlot>) -> (PageFamily, Vec<EngineAction>) {
        let text_slots = self
            .slots
            .into_iter()
            .map(|s| DesignTextSlot { id: s.id.into(), purpose: s.purpose.into(), command: s.command, max_chars: s.max_chars })
            .collect();
        (PageFamily { id: id.into(), purpose: purpose.into(), template: template.into(), text_slots, image_slots }, self.actions)
    }
}

pub(crate) fn instruction_package() -> Result<PackageFiles> {
    let id = "planners-journals-notebooks-studio";
    let description = "Create complete print and design production layouts (from 6-page prototypes up to 365-page year-round books) for planners, daily/weekly/monthly agendas, entertainment & mindfulness journals, Cornell & Bullet notebooks, posters, brochures, and stationery in Arabic (RTL), English (LTR), or bilingual formats. تصميم وإنتاج البلانر والمفكرات السنوية 365 صفحة واليوميات والدفاتر والملصقات والمطويات.";
    let instructions = concat!(
        "# Planners, Journals & Notebooks Studio (Native Clean-Room Edition)\n\n",
        "Use this studio skill whenever designing planners (`بلانر / مفكرة 365 صفحة / أجندة يومية أو أسبوعية أو شهرية`), ",
        "guided journals (`يوميات الامتنان والعادات والقراءة والسفر`), entertainment & mindfulness spreads (`تلوين ماندالا، سودوكو، متاهات، بينغو السعادة`), ",
        "or structured notebooks (`نظام كورنيل / دفتر منقط BuJo`).\n\n",
        "## Core Production Principles\n",
        "1. **Zero Repetition Across 365 Pages**: Rotate 12 seasonal themes across the year, vary daily micro-joy & evening reflection prompts on every single day, and alternate weekly creative escapes (Coloring Mandala -> Blossom Sudoku -> Happiness Bingo -> Botanical Doodle Studio).\n",
        "2. **Pastel Watercolor Legibility**: Keep writable box fills pale (`6-16%` tint) so pen ink stays high-contrast; keep all editable text, grids, timelines, and tables in crisp native DesignCraft frames (`frame.create`, `path.create`, `line.create`, `table.insert`), never rasterized inside images.\n",
        "3. **Commercial Stationery Geometry**: Default to A5 Portrait (`148 × 210 mm`, `12-14 mm` safe margins, `Smyth-Sewn Lay-Flat` binding) unless the user requests B5, Executive, or A4.\n"
    );
    let blueprints_ref = concat!(
        "# 365-Page Master Planner & Stationery Architecture\n\n",
        "- **Zone 1 (Front Matter & Wonderland)**: Cover (`planner-cover`), Ownership & 8-Petal Wheel of Dreams (`ownership-dream-wheel`), Yearly Vision & Milestones, Reading/Cinema/Travel Logs, and Savings Jars.\n",
        "- **Zone 2 (12 Seasonal Months)**: Each month opens with a Monthly Garden Calendar & Habit Matrix (`monthly-garden`), interleaves Weekly Starlight Compasses (`weekly-compass`), Daily Focus & Gratitude Sanctuaries (`daily-sanctuary`), Weekly Creative Escapes (`creative-escape`), Cornell/Dot Idea Studios (`cornell-dot-notes`), and closes with a Polaroid Memory Scrapbook (`polaroid-scrapbook`).\n",
        "- **Zone 3 (Year-End Reflection & Keepsakes)**: Annual Highlights Gallery, Letter to Future Self, Achievement Summary, and Creative Lounge.\n\n",
        "## 12 Seasonal Themes\n",
        "1. Frost & Starlight (Jan) | 2. Rosewater Dawn (Feb) | 3. Cherry Blossom Breeze (Mar) | 4. Wisteria Rain (Apr) | ",
        "5. Meadow Wildflower (May) | 6. Sunlit Citrus (Jun) | 7. Ocean Mist & Coral (Jul) | 8. Golden Lavender (Aug) | ",
        "9. Velvet Fig & Sage (Sep) | 10. Cinnamon Maple (Oct) | 11. Amber Lanterns (Nov) | 12. Evergreen Snowdrop (Dec).\n"
    );
    let prepress_ref = concat!(
        "# Stationery Prepress & Binding Reference\n\n",
        "- **Trim Sizes**: A5 (`148 × 210 mm`), B5 (`176 × 250 mm`), A4 (`210 × 297 mm`).\n",
        "- **Paper & Ink**: 100-120 gsm warm ivory uncoated stock; rule weight `0.45-0.75 pt` at `20-35%` tint for comfortable handwriting.\n",
        "- **RTL / LTR**: Use `rightToLeft` direction and right alignment for Arabic planners; `leftToRight` for English planners.\n"
    );
    let sample_actions = vec![
        EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": [54.0, 54.0, 540.0, 108.0], "content": "text", "text": "Starlight & Blossom 365 Planner Studio", "caret": false}),
        },
        EngineAction {
            command: "type.char".into(),
            params: json!({"attrs": {"fontFamily": "Tahoma", "fontStyle": "Bold", "size": 20.0, "leading": {"kind": "points", "value": 26.0}}}),
        },
        EngineAction { command: "type.para".into(), params: json!({"attrs": {"direction": "leftToRight", "align": "left", "hyphenate": false}}) },
        EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "rect": [54.0, 122.0, 540.0, 210.0], "content": "text", "text": "استوديو تصميم البلانر والمفكرات السنوية المتكاملة (365 صفحة) واليوميات الإبداعية", "caret": false}),
        },
        EngineAction {
            command: "type.char".into(),
            params: json!({"attrs": {"fontFamily": "Tahoma", "fontStyle": "Regular", "size": 16.0, "leading": {"kind": "points", "value": 22.0}}}),
        },
        EngineAction { command: "type.para".into(), params: json!({"attrs": {"direction": "rightToLeft", "align": "right", "hyphenate": false}}) },
    ];
    let manifest = NativeManifest {
        schema_version: 1,
        skill_id: id.into(),
        required_commands: vec!["frame.create".into(), "type.char".into(), "type.para".into()],
        references: vec!["references/planner-journal-blueprints.md".into(), "references/stationery-prepress-and-binding.md".into()],
        templates: vec!["templates/sample.json".into()],
        sample_template: Some("templates/sample.json".into()),
        external_tools: Vec::new(),
        compatibility_notes: "Clean-room native stationery & 365-planner architecture guide and prepress specification.".into(),
        design_profile: None,
    };
    Ok(PackageFiles::from([
        ("SKILL.md".into(), format!("---\nname: {id}\ndescription: {description}\n---\n{instructions}").into_bytes()),
        ("designcraft.json".into(), serde_json::to_vec_pretty(&manifest)?),
        ("templates/sample.json".into(), serde_json::to_vec_pretty(&sample_actions)?),
        ("references/planner-journal-blueprints.md".into(), blueprints_ref.as_bytes().to_vec()),
        ("references/stationery-prepress-and-binding.md".into(), prepress_ref.as_bytes().to_vec()),
    ]))
}

pub(crate) fn style_package() -> Result<PackageFiles> {
    let id = "planner-journal-studio-style";
    let families_and_templates = [
        build_cover(),
        build_ownership_wheel(),
        build_monthly_garden(),
        build_weekly_compass(),
        build_daily_sanctuary(),
        build_creative_escape(),
        build_cornell_notes(),
        build_polaroid_scrapbook(),
    ];
    let mut page_families = Vec::with_capacity(families_and_templates.len());
    let mut template_files = Vec::with_capacity(families_and_templates.len());
    let mut required_commands = BTreeSet::new();
    for (family, actions) in families_and_templates {
        for action in &actions {
            required_commands.insert(action.command.clone());
        }
        template_files.push((family.template.clone(), actions));
        page_families.push(family);
    }

    let profile = DesignProfile {
        name: "Starlight & Blossom 365 Planner Studio / استوديو البلانر والمفكرات 365".into(),
        keywords: [
            "بلانر",
            "مفكرة",
            "مفكرات",
            "أجندة",
            "اجندة",
            "يوميات",
            "دفتر",
            "دفاتر",
            "ملاحظات",
            "تخطيط",
            "عادات",
            "امتنان",
            "كورنيل",
            "رمضان",
            "365",
            "planner",
            "planners",
            "journal",
            "journals",
            "notebook",
            "notebooks",
            "agenda",
            "diary",
            "bujo",
            "bullet journal",
            "stationery",
            "habit tracker",
            "gratitude",
            "scrapbook",
            "cornell",
        ]
        .map(String::from)
        .into(),
        age_range: [8, 99],
        page_mm: [148.0, 210.0],
        margin_mm: 12.0,
        palette: ["[Paper]", "[Black]", ROSE, BLOSSOM, NAVY, SAGE, SKY, GOLD].map(String::from).into(),
        typography: vec![
            "Use native editable Tahoma Regular/Bold frames with explicit leading and automatic RTL/LTR paragraph direction; never bake text into raster art.".into(),
            "Titles 13–16 pt Bold, card headers 9.5–11 pt Bold, daily prompts and trackers 8.5–10 pt Regular with generous handwriting space.".into(),
        ],
        layout: vec![
            "A5 Portrait (148 × 210 mm) with 12 mm safe print margins; keep all vector cards, rules, grids, and illustrations strictly inside safe bounds.".into(),
            "Rotate page families across months, weeks, days, creative escapes, notes, and scrapbooks so long 365-page planners stay fresh and varied.".into(),
        ],
        pedagogy: vec![
            "Pair structured productivity (top-3 priorities, time-blocking, habit rings) with gentle mindfulness (gratitude, micro-joys, creative brain games).".into(),
            "Vary daily reflection prompts and weekly creative challenges across all 12 seasonal chapters without repeating boilerplate text.".into(),
        ],
        illustration: vec![
            "Delicate pastel watercolor botanical, celestial, and stationery vignettes with clean paper breathing room and zero embedded text.".into(),
            "Reuse recurring monthly seasonal header motifs intelligently across daily/weekly spreads when SmartHybrid visual density is active.".into(),
        ],
        page_families,
        corrections: vec![DesignCorrection {
            before: "Repeating identical daily boxes across 365 pages without seasonal themes or creative breaks".into(),
            after: "Interleave monthly gardens, weekly compasses, varied daily micro-joys, creative puzzle/coloring escapes, and monthly scrapbooks".into(),
            reason: "Eliminates planner fatigue and delivers a complete luxury 365-page stationery experience".into(),
        }],
    };
    profile.validate().map_err(crate::SkillError::Invalid)?;

    let manifest = NativeManifest {
        schema_version: 1,
        skill_id: id.into(),
        required_commands: required_commands.into_iter().collect(),
        references: vec!["references/style-guide.md".into()],
        templates: template_files.iter().map(|(path, _)| path.clone()).collect(),
        sample_template: Some("templates/daily-sanctuary.json".into()),
        external_tools: Vec::new(),
        compatibility_notes: "Clean-room A5 native stationery & 365-planner design profile with 8 vector page families. Validated locally against print preflight and 12 mm safe margins.".into(),
        design_profile: Some(profile),
    };
    let mut files = PackageFiles::from([
        (
            "SKILL.md".into(),
            format!(
                "---\nname: {id}\ndescription: Executable A5 pastel watercolor & editorial stationery style with 8 native vector page families for 6-to-365-page planners, journals, and notebooks. نمط استوديو البلانر والمفكرات التنفيذي والفنتازي بـ 8 قوالب فيكتور أصيلة.\n---\nSelect the declared A5 page family matching each page's role (cover, ownership wheel, monthly garden, weekly compass, daily sanctuary, creative escape, Cornell/dot notes, or Polaroid scrapbook). Populate every text slot with fresh seasonal, bilingual or single-language copy matching the user's language, and keep illustrations clean of embedded text.\n"
            )
            .into_bytes(),
        ),
        ("designcraft.json".into(), serde_json::to_vec_pretty(&manifest)?),
        (
            "references/style-guide.md".into(),
            b"# Starlight & Blossom 365 Studio Style Guide\n\nAll 8 A5 page families keep strokes, rounded pastel cards, timelines, puzzle grids, and Polaroid frames strictly inside the 12 mm safe margin. Never rasterize text.\n".to_vec(),
        ),
    ]);
    for (path, actions) in template_files {
        files.insert(path, serde_json::to_vec_pretty(&actions)?);
    }
    Ok(files)
}

fn build_cover() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 379.0, 74.0], (BLOSSOM, 0.14), (NAVY, 0.3, 0.7), "part.cover.ribbon");
    b.slot(
        "edition-badge",
        "Top collection or seasonal edition badge",
        "STARLIGHT & BLOSSOM • 365 EDITION",
        [48.0, 48.0, 371.0, 70.0],
        9.5,
        true,
        "center",
        NAVY,
        64,
    );
    b.card([40.0, 82.0, 379.0, 174.0], ("[Paper]", 1.0), (ROSE, 0.4, 0.85), "part.cover.title-card");
    b.slot(
        "book-title",
        "Main planner, journal, or notebook cover title",
        "Starlight & Blossom 365 Planner",
        [50.0, 90.0, 369.0, 132.0],
        15.0,
        true,
        "center",
        NAVY,
        72,
    );
    b.slot(
        "book-subtitle",
        "Inspiring cover subtitle and scope",
        "Daily Sanctuary • Seasonal Gardens • Mindful Escapes",
        [50.0, 134.0, 369.0, 168.0],
        9.5,
        false,
        "center",
        NAVY,
        96,
    );
    b.graphic_slot_frame([56.0, 184.0, 363.0, 382.0], ROSE, "part.art.cover-emblem");
    b.card([40.0, 392.0, 205.0, 434.0], (SKY, 0.12), (NAVY, 0.25, 0.6), "part.cover.pillar-1");
    b.slot(
        "pillar-1",
        "First planner feature highlight",
        "12 Seasonal Botanical Chapters",
        [46.0, 398.0, 199.0, 430.0],
        8.5,
        true,
        "center",
        NAVY,
        56,
    );
    b.card([214.0, 392.0, 379.0, 434.0], (SAGE, 0.12), (NAVY, 0.25, 0.6), "part.cover.pillar-2");
    b.slot(
        "pillar-2",
        "Second planner feature highlight",
        "Daily Focus & Gratitude Rituals",
        [220.0, 398.0, 373.0, 430.0],
        8.5,
        true,
        "center",
        NAVY,
        56,
    );
    b.card([40.0, 442.0, 379.0, 524.0], (GOLD, 0.10), (NAVY, 0.3, 0.7), "part.cover.owner-box");
    b.slot(
        "owner-line",
        "Personal ownership and year inscription",
        "Belongs to: ______________________   Year: ______",
        [50.0, 452.0, 369.0, 486.0],
        9.5,
        true,
        "center",
        NAVY,
        80,
    );
    b.slot(
        "cover-motto",
        "Closing cover blessing or motto",
        "One mindful page, one gentle victory, every day of the year.",
        [50.0, 488.0, 369.0, 518.0],
        8.5,
        false,
        "center",
        NAVY,
        96,
    );
    b.footer_chrome();
    b.finish(
        "planner-cover",
        "Opening A5 cover with edition ribbon, framed title, botanical emblem frame, highlights, and owner inscription",
        "templates/planner-cover.json",
        vec![DesignImageSlot {
            id: "cover-emblem".into(),
            purpose: "Pastel watercolor botanical and starlight cover centerpiece illustration, no embedded text".into(),
            rect: [56.0, 184.0, 363.0, 382.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_ownership_wheel() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 278.0, 92.0], (BLOSSOM, 0.12), (NAVY, 0.28, 0.65), "part.wheel.header");
    b.slot(
        "page-title",
        "Front-matter or vision page heading",
        "Wheel of Dreams & Life Compass",
        [48.0, 47.0, 270.0, 70.0],
        12.0,
        true,
        "left",
        NAVY,
        64,
    );
    b.slot(
        "page-subtitle",
        "Instruction for rating or mapping life pillars",
        "Shade each petal from 1–10 and set your seasonal intention.",
        [48.0, 70.0, 270.0, 89.0],
        8.5,
        false,
        "left",
        NAVY,
        84,
    );
    b.graphic_slot_frame([286.0, 42.0, 379.0, 92.0], ROSE, "part.art.corner-motif");
    // Vector 8-spoke concentric lotus/compass wheel centered at (209.5, 212.0), radius 92 pt
    b.ellipse([117.5, 120.0, 301.5, 304.0], ROSE, 0.08, NAVY, 0.35, 0.8);
    b.ellipse([147.5, 150.0, 271.5, 274.0], SKY, 0.08, NAVY, 0.25, 0.55);
    b.ellipse([177.5, 180.0, 241.5, 244.0], GOLD, 0.12, NAVY, 0.25, 0.55);
    b.rule([117.5, 212.0], [301.5, 212.0], NAVY, 0.28, 0.55, None);
    b.rule([209.5, 120.0], [209.5, 304.0], NAVY, 0.28, 0.55, None);
    b.rule([144.5, 147.0], [274.5, 277.0], NAVY, 0.24, 0.5, None);
    b.rule([144.5, 277.0], [274.5, 147.0], NAVY, 0.24, 0.5, None);
    b.card([40.0, 318.0, 205.0, 392.0], (SAGE, 0.10), (NAVY, 0.25, 0.6), "part.wheel.domain-1");
    b.slot(
        "domain-1",
        "First two life/goal domains & target habit",
        "Mind & Wellness • Restful energy & mindful mornings",
        [47.0, 324.0, 198.0, 386.0],
        8.5,
        false,
        "left",
        NAVY,
        96,
    );
    b.card([214.0, 318.0, 379.0, 392.0], (SKY, 0.10), (NAVY, 0.25, 0.6), "part.wheel.domain-2");
    b.slot(
        "domain-2",
        "Second two life/goal domains & target habit",
        "Career & Learning • Deep craft & joyful curiosity",
        [221.0, 324.0, 372.0, 386.0],
        8.5,
        false,
        "left",
        NAVY,
        96,
    );
    b.card([40.0, 400.0, 205.0, 474.0], (ROSE, 0.10), (NAVY, 0.25, 0.6), "part.wheel.domain-3");
    b.slot(
        "domain-3",
        "Third two life/goal domains & target habit",
        "Family & Belonging • Warm presence & gratitude",
        [47.0, 406.0, 198.0, 468.0],
        8.5,
        false,
        "left",
        NAVY,
        96,
    );
    b.card([214.0, 400.0, 379.0, 474.0], (GOLD, 0.12), (NAVY, 0.25, 0.6), "part.wheel.domain-4");
    b.slot(
        "domain-4",
        "Fourth two life/goal domains & target habit",
        "Play & Adventure • Books, cinema, travel & art",
        [221.0, 406.0, 372.0, 468.0],
        8.5,
        false,
        "left",
        NAVY,
        96,
    );
    b.card([40.0, 482.0, 379.0, 526.0], (BLOSSOM, 0.09), (NAVY, 0.28, 0.6), "part.wheel.motto");
    b.slot(
        "guiding-word",
        "Personal word of the year or guiding compass",
        "Word of the Year & North Star Intention: ________________________",
        [48.0, 488.0, 371.0, 520.0],
        9.0,
        true,
        "center",
        NAVY,
        96,
    );
    b.footer_chrome();
    b.finish(
        "ownership-dream-wheel",
        "8-Petal Wheel of Dreams, lifestyle compass, or milestone map with vector radial chart and 4 domain cards",
        "templates/ownership-dream-wheel.json",
        vec![DesignImageSlot {
            id: "corner-motif".into(),
            purpose: "Delicate watercolor lotus and starlight corner stationery vignette, no text".into(),
            rect: [286.0, 42.0, 379.0, 92.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_monthly_garden() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 266.0, 96.0], (ROSE, 0.13), (NAVY, 0.3, 0.7), "part.month.banner");
    b.slot(
        "month-name",
        "Month title and seasonal theme",
        "Month 01 • Frost & Starlight Garden",
        [48.0, 48.0, 258.0, 72.0],
        12.5,
        true,
        "left",
        NAVY,
        64,
    );
    b.slot(
        "month-quote",
        "Seasonal opening quote or focus motto",
        "Plant quiet seeds beneath the winter stars.",
        [48.0, 73.0, 258.0, 93.0],
        8.5,
        false,
        "left",
        NAVY,
        84,
    );
    b.graphic_slot_frame([274.0, 42.0, 379.0, 96.0], BLOSSOM, "part.art.monthly-header-art");
    // Calendar header strip + 5x7 calendar grid (x: 40..379, y: 104..364)
    b.card([40.0, 104.0, 379.0, 128.0], (NAVY, 0.88), (NAVY, 0.9, 0.6), "part.month.weekdays");
    b.slot(
        "weekdays-bar",
        "Seven weekday column labels",
        "SUN     MON     TUE     WED     THU     FRI     SAT",
        [46.0, 108.0, 373.0, 125.0],
        8.5,
        true,
        "center",
        "[Paper]",
        72,
    );
    b.card([40.0, 130.0, 379.0, 360.0], ("[Paper]", 1.0), (NAVY, 0.3, 0.7), "part.month.grid");
    let mut grid_lines = Vec::new();
    for col in 1..7 {
        let x = 40.0 + f64::from(col) * (339.0 / 7.0);
        grid_lines.push([[x, 130.0], [x, 360.0]]);
    }
    for row in 1..5 {
        let y = 130.0 + f64::from(row) * (230.0 / 5.0);
        grid_lines.push([[40.0, y], [379.0, y]]);
    }
    b.rules(&grid_lines, NAVY, 0.18, 0.45);
    b.card([40.0, 370.0, 205.0, 456.0], (SAGE, 0.11), (NAVY, 0.25, 0.6), "part.month.goals");
    b.slot(
        "monthly-goals",
        "Top 3 monthly goals or intentions",
        "Monthly Seeds:\n1. _______________\n2. _______________",
        [48.0, 376.0, 197.0, 450.0],
        8.5,
        false,
        "left",
        NAVY,
        110,
    );
    b.card([214.0, 370.0, 379.0, 456.0], (GOLD, 0.12), (NAVY, 0.25, 0.6), "part.month.habits");
    b.slot(
        "habit-rituals",
        "3 core monthly habits to cultivate",
        "Daily Ritual Petals:\n• Hydration & Walk\n• 15m Reading",
        [222.0, 376.0, 371.0, 450.0],
        8.5,
        false,
        "left",
        NAVY,
        110,
    );
    b.card([40.0, 464.0, 379.0, 526.0], (SKY, 0.10), (NAVY, 0.25, 0.6), "part.month.celebrate");
    b.slot(
        "month-reward",
        "Key dates, birthdays, and end-of-month celebration",
        "Key Dates & Monthly Reward: ____________________________________",
        [48.0, 472.0, 371.0, 518.0],
        8.5,
        false,
        "left",
        NAVY,
        110,
    );
    b.footer_chrome();
    b.finish(
        "monthly-garden",
        "Monthly seasonal calendar overview with 5x7 vector date grid, monthly intentions, habit rituals, and seasonal vignette",
        "templates/monthly-garden.json",
        vec![DesignImageSlot {
            id: "monthly-header-art".into(),
            purpose: "Seasonal watercolor floral and botanical header motif for the month, no text".into(),
            rect: [274.0, 42.0, 379.0, 96.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_weekly_compass() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 282.0, 88.0], (SKY, 0.14), (NAVY, 0.28, 0.65), "part.week.header");
    b.slot(
        "week-title",
        "Week number, season, and date span",
        "Week 01 • Starlight Compass Overview",
        [48.0, 47.0, 274.0, 68.0],
        11.5,
        true,
        "left",
        NAVY,
        64,
    );
    b.slot(
        "week-focus",
        "Single guiding focus for the week",
        "Main Focus: Protect creative mornings & gentle evenings",
        [48.0, 68.0, 274.0, 86.0],
        8.5,
        false,
        "left",
        NAVY,
        84,
    );
    b.graphic_slot_frame([290.0, 42.0, 379.0, 88.0], SKY, "part.art.weekly-accent");

    let cards = [
        ("days-mon-tue", "Monday & Tuesday cloud plan", "Mon / Tue • Priority blocks & appointments", [40.0, 96.0, 205.0, 198.0], ROSE),
        ("days-wed-thu", "Wednesday & Thursday cloud plan", "Wed / Thu • Deep work & mid-week check-in", [214.0, 96.0, 379.0, 198.0], BLOSSOM),
        ("days-fri-sat", "Friday & Saturday sanctuary plan", "Fri / Sat • Wrap-up, family & outdoor joy", [40.0, 206.0, 205.0, 308.0], SAGE),
        ("day-sun-reset", "Sunday reset & weekly prep", "Sunday Reset • Tidy space, meal prep & rest", [214.0, 206.0, 379.0, 308.0], GOLD),
    ];
    for (id, purpose, sample, rect, swatch) in cards {
        b.card(rect, (swatch, 0.09), (NAVY, 0.25, 0.6), &format!("part.week.{id}"));
        b.slot(id, purpose, sample, [rect[0] + 8.0, rect[1] + 6.0, rect[2] - 8.0, rect[3] - 48.0], 8.5, true, "left", NAVY, 96);
        b.rules(
            &[
                [[rect[0] + 10.0, rect[3] - 32.0], [rect[2] - 10.0, rect[3] - 32.0]],
                [[rect[0] + 10.0, rect[3] - 14.0], [rect[2] - 10.0, rect[3] - 14.0]],
            ],
            NAVY,
            0.18,
            0.45,
        );
    }
    b.card([40.0, 318.0, 379.0, 422.0], ("[Paper]", 1.0), (NAVY, 0.28, 0.65), "part.week.priorities");
    b.slot(
        "top-three-week",
        "Weekly top-3 outcomes & habit tracker prompt",
        "Top 3 Weekly Outcomes & Habit Check (M T W T F S S):",
        [48.0, 324.0, 371.0, 354.0],
        9.0,
        true,
        "left",
        NAVY,
        88,
    );
    b.rule([48.0, 374.0], [371.0, 374.0], NAVY, 0.2, 0.5, None);
    b.rule([48.0, 398.0], [371.0, 398.0], NAVY, 0.2, 0.5, None);
    b.card([40.0, 430.0, 379.0, 526.0], (BLOSSOM, 0.08), (NAVY, 0.25, 0.6), "part.week.gratitude");
    b.slot(
        "weekly-win",
        "Weekly gratitude, joy jar & reflection prompt",
        "Brightest Moment of the Week & Next Week's Wish:",
        [48.0, 438.0, 371.0, 472.0],
        9.0,
        true,
        "left",
        NAVY,
        96,
    );
    b.rule([48.0, 496.0], [371.0, 496.0], NAVY, 0.2, 0.5, None);
    b.footer_chrome();
    b.finish(
        "weekly-compass",
        "Weekly overview spread with 4 pastel day-pair cloud cards, weekly top-3 outcomes, habit check, and brightest-moment reflection",
        "templates/weekly-compass.json",
        vec![DesignImageSlot {
            id: "weekly-accent".into(),
            purpose: "Soft watercolor starlight compass and floral corner accent, no embedded text".into(),
            rect: [290.0, 42.0, 379.0, 88.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_daily_sanctuary() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 286.0, 88.0], (ROSE, 0.12), (NAVY, 0.28, 0.65), "part.daily.header");
    b.slot(
        "day-header",
        "Day number, date/season label, and weekday",
        "Day 001 • Frost & Starlight Sanctuary",
        [48.0, 46.0, 278.0, 68.0],
        11.5,
        true,
        "left",
        NAVY,
        68,
    );
    b.slot(
        "morning-intention",
        "Morning energy & single guiding intention",
        "Today's Gentle Intention: Move with calm focus.",
        [48.0, 68.0, 278.0, 86.0],
        8.5,
        false,
        "left",
        NAVY,
        88,
    );
    b.graphic_slot_frame([294.0, 42.0, 379.0, 88.0], SAGE, "part.art.daily-botanical");

    // Left column: Top 3 Priorities + Micro-Joy (x: 40..204)
    b.card([40.0, 96.0, 204.0, 250.0], (GOLD, 0.11), (NAVY, 0.26, 0.6), "part.daily.top3");
    b.slot(
        "priority-heading",
        "Top 3 Must-Do Focus Cards heading & prompt",
        "Top 3 Must-Do Priorities:",
        [48.0, 102.0, 196.0, 126.0],
        9.0,
        true,
        "left",
        NAVY,
        56,
    );
    b.slot(
        "priority-list",
        "Suggested focus slots or scripted daily anchors",
        "1. Deep work block\n2. Key follow-up\n3. Self-care pause",
        [48.0, 130.0, 196.0, 240.0],
        8.5,
        false,
        "left",
        NAVY,
        120,
    );

    b.card([40.0, 258.0, 204.0, 386.0], (BLOSSOM, 0.10), (NAVY, 0.25, 0.6), "part.daily.microjoy");
    b.slot(
        "micro-joy",
        "Unique daily micro-joy or mindfulness activity",
        "Today's Micro-Joy:\nSavor a warm cup away from screens for 10 quiet minutes.",
        [48.0, 266.0, 196.0, 378.0],
        8.5,
        false,
        "left",
        NAVY,
        130,
    );

    // Right column: Time-Block Rhythm (x: 212..379, y: 96..386)
    b.card([212.0, 96.0, 379.0, 386.0], ("[Paper]", 1.0), (NAVY, 0.28, 0.65), "part.daily.timeline");
    b.slot(
        "schedule-heading",
        "Daily time-blocking column heading",
        "Time-Block Flow (07:00 – 21:00)",
        [220.0, 102.0, 371.0, 126.0],
        9.0,
        true,
        "left",
        NAVY,
        56,
    );
    let timeline: Vec<_> = (0..8).map(|step| [[220.0, 148.0 + f64::from(step) * 28.0], [371.0, 148.0 + f64::from(step) * 28.0]]).collect();
    b.rules(&timeline, NAVY, 0.18, 0.45);

    // Bottom sanctuary: Gratitude + Evening Reflection + Wellness drops
    b.card([40.0, 396.0, 379.0, 526.0], (SAGE, 0.10), (NAVY, 0.28, 0.65), "part.daily.gratitude");
    b.slot(
        "gratitude-prompt",
        "Unique evening gratitude & reflection question",
        "Evening Gratitude • What small moment made you smile today?",
        [48.0, 402.0, 371.0, 434.0],
        9.0,
        true,
        "left",
        NAVY,
        110,
    );
    b.rule([48.0, 458.0], [371.0, 458.0], NAVY, 0.2, 0.45, None);
    b.rule([48.0, 484.0], [371.0, 484.0], NAVY, 0.2, 0.45, None);
    b.slot(
        "wellness-bar",
        "Hydration, mood, energy & sleep check line",
        "Water: [ ] [ ] [ ] [ ]   Mood: 1 2 3 4 5   Rest: ____ hrs",
        [48.0, 494.0, 371.0, 520.0],
        8.5,
        false,
        "center",
        NAVY,
        96,
    );
    b.footer_chrome();
    b.finish(
        "daily-sanctuary",
        "Daily productivity, time-blocking, unique micro-joy, and evening gratitude page",
        "templates/daily-sanctuary.json",
        vec![DesignImageSlot {
            id: "daily-botanical".into(),
            purpose: "Small seasonal watercolor flower or botanical corner stamp, no text".into(),
            rect: [294.0, 42.0, 379.0, 88.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_creative_escape() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 379.0, 90.0], (BLOSSOM, 0.14), (NAVY, 0.3, 0.7), "part.escape.header");
    b.slot(
        "escape-title",
        "Entertainment, brain game, coloring, or bingo challenge title",
        "Creative Sanctuary • Blossom Logic & Mindful Art",
        [48.0, 47.0, 371.0, 70.0],
        12.0,
        true,
        "center",
        NAVY,
        72,
    );
    b.slot(
        "escape-rules",
        "Short instructions for the weekly puzzle, bingo, or coloring escape",
        "Unplug for 15 minutes: sketch, color, or complete the 4×4 seasonal grid.",
        [48.0, 70.0, 371.0, 88.0],
        8.5,
        false,
        "center",
        NAVY,
        96,
    );

    // Upper creative art / coloring / doodle canvas
    b.graphic_slot_frame([48.0, 98.0, 371.0, 272.0], BLOSSOM, "part.art.escape-canvas");

    // Lower 4x4 interactive Bingo / Mini-Sudoku / Challenge Grid (x: 40..232, y: 282..466) + Reflection Side Card
    b.card([40.0, 282.0, 232.0, 466.0], ("[Paper]", 1.0), (NAVY, 0.32, 0.75), "part.escape.grid");
    for i in 1..4 {
        let x = 40.0 + f64::from(i) * (192.0 / 4.0);
        let y = 282.0 + f64::from(i) * (184.0 / 4.0);
        b.rule([x, 282.0], [x, 466.0], NAVY, 0.24, 0.55, None);
        b.rule([40.0, y], [232.0, y], NAVY, 0.24, 0.55, None);
    }
    b.card([240.0, 282.0, 379.0, 466.0], (GOLD, 0.12), (NAVY, 0.28, 0.65), "part.escape.clues");
    b.slot(
        "challenge-prompts",
        "Clues, Happiness Bingo prompts, riddle, or 4-step doodle guide",
        "Joy Prompts & Clues:\n• Try a new tea\n• Sketch a leaf\n• Call a friend\n• Read 10 pages",
        [247.0, 290.0, 372.0, 458.0],
        8.5,
        false,
        "left",
        NAVY,
        150,
    );

    b.card([40.0, 474.0, 379.0, 526.0], (SAGE, 0.11), (NAVY, 0.26, 0.6), "part.escape.spark");
    b.slot(
        "creative-spark",
        "Takeaway spark, riddle hint, or mood check after completing the page",
        "Creative Spark • How do you feel after this mindful break? ______________",
        [48.0, 482.0, 371.0, 518.0],
        8.5,
        true,
        "center",
        NAVY,
        100,
    );
    b.footer_chrome();
    b.finish(
        "creative-escape",
        "Weekly entertainment & mindfulness escape page with coloring/doodle art frame, 4x4 puzzle/bingo grid, and clues card",
        "templates/creative-escape.json",
        vec![DesignImageSlot {
            id: "escape-canvas".into(),
            purpose: "Clean black-and-pastel line-art botanical mandala or whimsical illustration for mindful coloring, no text".into(),
            rect: [48.0, 98.0, 371.0, 272.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_cornell_notes() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 288.0, 88.0], (SAGE, 0.13), (NAVY, 0.28, 0.65), "part.notes.header");
    b.slot(
        "notes-title",
        "Cornell / Dot-Grid studio topic heading",
        "Enchanted Cornell & Idea Studio",
        [48.0, 46.0, 280.0, 68.0],
        11.5,
        true,
        "left",
        NAVY,
        64,
    );
    b.slot(
        "notes-meta",
        "Subject, project, or date tag",
        "Topic / Project: ______________________   Date: ______",
        [48.0, 68.0, 280.0, 86.0],
        8.5,
        false,
        "left",
        NAVY,
        84,
    );
    b.graphic_slot_frame([296.0, 42.0, 379.0, 88.0], GOLD, "part.art.notes-motif");

    // Cue column (x: 40..136) + Main capture zone (x: 144..379), y: 96..444
    b.card([40.0, 96.0, 136.0, 444.0], (SKY, 0.08), (NAVY, 0.25, 0.6), "part.notes.cues");
    b.slot("cue-heading", "Left cue/questions column prompt", "Key Cues &\nQuestions", [46.0, 102.0, 130.0, 142.0], 8.5, true, "left", NAVY, 48);
    b.card([144.0, 96.0, 379.0, 444.0], ("[Paper]", 1.0), (NAVY, 0.28, 0.65), "part.notes.body");
    b.slot(
        "capture-heading",
        "Main notes & sketches column prompt",
        "Ideas, Insights & Structured Notes",
        [152.0, 102.0, 371.0, 126.0],
        9.0,
        true,
        "left",
        NAVY,
        64,
    );
    let capture_lines: Vec<_> = (0..10).map(|step| [[152.0, 148.0 + f64::from(step) * 28.0], [371.0, 148.0 + f64::from(step) * 28.0]]).collect();
    b.rules(&capture_lines, NAVY, 0.16, 0.45);

    // Bottom synthesis summary
    b.card([40.0, 452.0, 379.0, 526.0], (GOLD, 0.11), (NAVY, 0.28, 0.65), "part.notes.summary");
    b.slot(
        "summary-prompt",
        "Bottom Cornell synthesis & next action prompt",
        "Key Synthesis & Next Action Step:",
        [48.0, 458.0, 371.0, 484.0],
        9.0,
        true,
        "left",
        NAVY,
        84,
    );
    b.rule([48.0, 506.0], [371.0, 506.0], NAVY, 0.2, 0.45, None);
    b.footer_chrome();
    b.finish(
        "cornell-dot-notes",
        "Cornell & guided idea notebook page with cue column, ruled capture workspace, and bottom synthesis card",
        "templates/cornell-dot-notes.json",
        vec![DesignImageSlot {
            id: "notes-motif".into(),
            purpose: "Minimal pastel botanical quill or notebook stationery emblem, no text".into(),
            rect: [296.0, 42.0, 379.0, 88.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}

fn build_polaroid_scrapbook() -> (PageFamily, Vec<EngineAction>) {
    let mut b = FamilyBuilder::new();
    b.card([40.0, 42.0, 379.0, 86.0], (ROSE, 0.14), (NAVY, 0.28, 0.65), "part.scrap.header");
    b.slot(
        "scrapbook-title",
        "Monthly memory scrapbook or gallery heading",
        "Monthly Polaroid Keepsakes & Memories",
        [48.0, 46.0, 371.0, 68.0],
        12.0,
        true,
        "center",
        NAVY,
        68,
    );
    b.slot(
        "scrapbook-sub",
        "Subtitle inviting photos, tickets, or sketches",
        "Paste a favorite photo, ticket stub, pressed leaf, or memory sketch.",
        [48.0, 68.0, 371.0, 84.0],
        8.5,
        false,
        "center",
        NAVY,
        92,
    );

    // Polaroid outer frame (x: 40..226, y: 96..344) + photo image_slot inside + caption strip
    b.card([40.0, 96.0, 226.0, 344.0], ("[Paper]", 1.0), (NAVY, 0.35, 0.85), "part.scrap.polaroid");
    b.graphic_slot_frame([50.0, 106.0, 216.0, 294.0], ROSE, "part.art.memory-photo");
    b.slot(
        "polaroid-caption",
        "Caption strip beneath the Polaroid photo frame",
        "Snapshot of the Month • Date & Place",
        [50.0, 300.0, 216.0, 336.0],
        8.5,
        true,
        "center",
        NAVY,
        64,
    );

    // Right keepsake cards (x: 236..379)
    b.card([236.0, 96.0, 379.0, 216.0], (GOLD, 0.12), (NAVY, 0.25, 0.6), "part.scrap.favorites");
    b.slot(
        "favorites-box",
        "Book, film, song, or cafe of the month",
        "Favorites Log:\n• Book:\n• Film/Song:\n• Cozy Spot:",
        [244.0, 104.0, 371.0, 208.0],
        8.5,
        false,
        "left",
        NAVY,
        120,
    );
    b.card([236.0, 224.0, 379.0, 344.0], (SKY, 0.11), (NAVY, 0.25, 0.6), "part.scrap.lesson");
    b.slot(
        "proud-moment",
        "Proudest win and lesson learned this month",
        "Proudest Win & Gentle Lesson Learned:",
        [244.0, 232.0, 371.0, 336.0],
        8.5,
        false,
        "left",
        NAVY,
        120,
    );

    // Bottom journaling & next month bridge
    b.card([40.0, 354.0, 379.0, 526.0], (BLOSSOM, 0.09), (NAVY, 0.28, 0.65), "part.scrap.story");
    b.slot(
        "memory-story",
        "Gratitude letter or story from the month",
        "Unforgettable Story, Quote, or Gratitude Letter from This Month:",
        [48.0, 362.0, 371.0, 396.0],
        9.0,
        true,
        "left",
        NAVY,
        110,
    );
    for step in 0..4 {
        let y = 420.0 + f64::from(step) * 26.0;
        b.rule([48.0, y], [371.0, y], NAVY, 0.18, 0.45, None);
    }
    b.footer_chrome();
    b.finish(
        "polaroid-scrapbook",
        "End-of-month Polaroid photo keepsake, favorites log, proudest win, and memory storytelling page",
        "templates/polaroid-scrapbook.json",
        vec![DesignImageSlot {
            id: "memory-photo".into(),
            purpose: "Cozy pastel watercolor seasonal keepsake scene inside a Polaroid frame, no text".into(),
            rect: [50.0, 106.0, 216.0, 294.0],
            crop: None,
            source_aspect_ratio: None,
            views: vec![],
        }],
    )
}
