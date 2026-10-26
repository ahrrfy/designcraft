//! Bind explicit print fonts on new native stories, preserving copy and all non-font formatting.
use crate::print_contract::{AvailablePrintFont, PrintContract};
use designcraft_ai_types::{EngineAction, protocol::ProductionPlan};
use designcraft_engine::{DocState, Prefs, Session, doc::StoryId};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_ACTIONS: usize = 512;
const MAX_NEW_TEXT_BYTES: usize = 512 * 1024;

pub(crate) fn apply(plan: &mut ProductionPlan, base: &DocState, prefs: &Prefs, contract: &PrintContract) -> Result<Vec<Value>, String> {
    if plan.commands.len() > MAX_ACTIONS {
        return Err("Native print font binding exceeds the 512-action limit".into());
    }
    if contract.english_fonts.is_empty() && contract.arabic_fonts.is_empty() {
        return Ok(Vec::new());
    }
    let selected = contract.select_fonts(base)?;
    let mut notes: Vec<Value> = selected.notices.iter().map(|message| json!({"kind":"printFontFallback","message":message})).collect();
    let mut candidate = plan.clone();
    notes.extend(crate::native_layout::recover_unregistered_swatches(&mut candidate, base)?);
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    let result = scratch
        .execute("production.apply", &json!({"commands":candidate.commands,"outputMode":"native_with_images","requirePreflight":false}))
        .map_err(|e| e.to_string())?;
    let results = result.get("results").and_then(Value::as_array).ok_or("Native print font validation did not return command results")?;
    let mut frames = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, action) in candidate.commands.iter().enumerate() {
        if action.command != "frame.create" {
            continue;
        }
        let Some(story) = results.get(index).and_then(|v| v.get("story")).and_then(Value::as_u64) else { continue };
        let story_id = StoryId(story);
        if base.doc.story(story_id).is_some() || !seen.insert(story_id) {
            continue;
        }
        frames.push((index, story_id));
    }
    let mut text_bytes = 0usize;
    for (index, story_id) in frames {
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let story = doc.story(story_id).ok_or("A new text story was removed or threaded during font binding")?;
        if story.frames.iter().any(|id| base.doc.item(*id).is_some()) {
            return Err("Print font binding cannot change a story threaded through an existing frame".into());
        }
        text_bytes = text_bytes
            .checked_add(story.text.len())
            .filter(|bytes| *bytes <= MAX_NEW_TEXT_BYTES)
            .ok_or("New print text exceeds the 512-KiB font-binding limit")?;
        let has_arabic = story.text.chars().any(is_arabic);
        let has_latin = story.text.chars().any(|ch| ch.is_ascii_alphabetic() || ('\u{00c0}'..='\u{024f}').contains(&ch));
        let font = if has_arabic { selected.arabic.as_ref() } else { selected.english.as_ref() };
        let Some(font) = font else { continue };
        if has_arabic && has_latin {
            notes.push(json!({"kind":"mixedScriptPrintFont","story":story_id.0,"fontFamily":font.family,
                "message":"Kept the existing mixed Arabic/Latin frame intact; applied the available Arabic print face with checked Latin coverage"}));
        }
        let fonts = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
        let mut ranges: Vec<(usize, usize, String, String)> = Vec::new();
        let mut runs = story.runs().peekable();
        for (paragraph_index, mut paragraph_range) in story.para_ranges().into_iter().enumerate() {
            let para = story.paras.get(paragraph_index).ok_or("Missing native paragraph during print font binding")?;
            let (_, base_chars) = doc.styles.resolve_para(para);
            // Include the existing paragraph separator without inserting or removing any bytes.
            if story.text.as_bytes().get(paragraph_range.end) == Some(&b'\n') {
                paragraph_range.end = paragraph_range.end.checked_add(1).ok_or("Print paragraph boundary overflow")?;
            }
            while let Some((run_range, format)) = runs.peek().cloned() {
                if run_range.end <= paragraph_range.start {
                    let _ = runs.next();
                    continue;
                }
                if run_range.start >= paragraph_range.end {
                    break;
                }
                let finished = run_range.end <= paragraph_range.end;
                if finished {
                    let _ = runs.next();
                }
                let start = paragraph_range.start.max(run_range.start);
                let end = paragraph_range.end.min(run_range.end);
                if start >= end {
                    return Err("Print font run has an invalid paragraph intersection".into());
                }
                let text = story.text.get(start..end).ok_or("Print font run is not on Unicode character boundaries")?;
                let attrs = doc.styles.resolve_char(&base_chars, format);
                let (weight, italic) = fonts.traits_of(&attrs.font_family, &attrs.font_style);
                let (run_family, style) = binding_style(font, &fonts, weight >= 600.0, italic)?;
                let face = fonts.face(&run_family, &style);
                // Symbols and covered Arabic honorific ligatures/signs are left for the separate explicit fallback-font repair pass.
                if let Some(ch) = text.chars().find(|&ch| {
                    ch != '\u{061c}'
                        && (ch.is_alphabetic() || ch.is_numeric() || is_arabic(ch))
                        && face.glyph_for(ch) == 0
                        && !(crate::native_layout::is_fallback_symbol_or_honorific(ch) && crate::native_layout::has_fallback_glyph(&fonts, ch))
                }) {
                    return Err(format!("Requested print font '{} {}' lacks supplied character {ch}; copy was preserved", run_family, style));
                }
                let merge = ranges.last().is_some_and(|(_, previous_end, previous_fam, previous_style)| {
                    *previous_end == start && *previous_fam == run_family && *previous_style == style
                });
                if merge {
                    if let Some((_, previous_end, _, _)) = ranges.last_mut() {
                        *previous_end = end;
                    }
                } else {
                    ranges.push((start, end, run_family, style));
                }
                if ranges.len() > MAX_ACTIONS / 2 {
                    return Err("Print text has too many formatting ranges for the 512-action limit".into());
                }
                if !finished {
                    break;
                }
            }
        }
        let count = ranges.len();
        for (start, end, run_family, style) in ranges {
            if candidate.commands.len().checked_add(2).is_none_or(|count| count > MAX_ACTIONS) {
                return Err("Native print font binding exceeds the 512-action limit".into());
            }
            let actions = [
                EngineAction { command: "text.select".into(), params: json!({"story":format!("${index}.story"),"anchor":start,"focus":end}) },
                EngineAction { command: "type.char".into(), params: json!({"attrs":{"fontFamily":run_family,"fontStyle":style}}) },
            ];
            scratch.execute("text.select", &json!({"story":story_id.0,"anchor":start,"focus":end})).map_err(|e| e.to_string())?;
            let _ = scratch.execute("type.char", &actions.get(1).ok_or("Missing print font action")?.params).map_err(|e| e.to_string())?;
            candidate.commands.extend(actions);
        }
        if scratch.doc().map_err(|e| e.to_string())?.doc.story(story_id).is_none_or(|after| after.text != story.text) {
            return Err("Print font binding changed supplied text unexpectedly; the plan was preserved".into());
        }
        notes.push(json!({"kind":"nativePrintFontBinding","story":story_id.0,"fontFamily":font.family,"ranges":count,
            "message":"Bound available print font while preserving copy, sizes, leading, paragraph direction and bold status"}));
    }
    *plan = candidate;
    Ok(notes)
}

fn is_arabic(ch: char) -> bool {
    matches!(ch,'\u{0600}'..='\u{06ff}'|'\u{0750}'..='\u{077f}'|'\u{08a0}'..='\u{08ff}'|'\u{fb50}'..='\u{fdff}'|'\u{fe70}'..='\u{feff}')
}

#[cfg(test)]
fn has_fallback_glyph(fonts: &designcraft_fonts::ScopedFonts<'_>, ch: char) -> bool {
    crate::native_layout::has_fallback_glyph(fonts, ch)
}

fn binding_style(
    font: &AvailablePrintFont,
    fonts: &designcraft_fonts::ScopedFonts<'_>,
    bold: bool,
    italic: bool,
) -> Result<(String, String), String> {
    let target_family = if bold { font.heading_family.as_deref().unwrap_or(&font.family) } else { &font.family };
    let selected = if bold { &font.heading } else { &font.regular };
    let styles = fonts.styles(target_family);
    let style = styles
        .iter()
        .find(|style| {
            let (weight, slanted) = fonts.traits_of(target_family, style);
            (weight >= 600.0) == bold && slanted == italic && style.eq_ignore_ascii_case(selected)
        })
        .or_else(|| {
            styles.iter().find(|style| {
                let (weight, slanted) = fonts.traits_of(target_family, style);
                (weight >= 600.0) == bold && slanted == italic
            })
        })
        .or_else(|| (!italic).then(|| styles.iter().find(|style| style.eq_ignore_ascii_case(selected))).flatten());
    style.map(|s| (target_family.to_owned(), s.clone())).ok_or_else(|| {
        format!("Print font '{}' has no installed face preserving bold={bold}, italic={italic}; formatting was preserved", target_family)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_engine::doc::TextDirection;

    fn fonts() -> Option<String> {
        let db = designcraft_fonts::FontDb::global();
        ["Cairo", "Tahoma", "DejaVu Sans", "Noto Sans Arabic", "Arial"]
            .into_iter()
            .find(|family| {
                db.has_family(family)
                    && ["Regular", "Bold"].iter().all(|style| {
                        db.styles(family).iter().any(|s| s.as_str() == *style)
                            && "AaGgالعربيةَُِّْ0123456789٠١٢٣٤٥٦٧٨٩".chars().all(|c| db.face(family, style).glyph_for(c) != 0)
                    })
            })
            .map(str::to_string)
    }

    fn execute(plan: &ProductionPlan, base: &DocState, prefs: &Prefs) -> Session {
        let mut session = Session::new();
        session.prefs = prefs.clone();
        session.add_document(base.clone());
        session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_with_images","requirePreflight":false})).unwrap();
        session
    }

    fn formatting(state: &DocState) -> Vec<Value> {
        let db = designcraft_fonts::FontDb::global().scoped(state.doc.font_scope);
        let mut result = Vec::new();
        for story in state.doc.stories.values() {
            let mut chars = Vec::new();
            for (pi, range) in story.para_ranges().into_iter().enumerate() {
                let (para, base_chars) = state.doc.styles.resolve_para(story.paras.get(pi).unwrap());
                for (offset, ch) in story.text.get(range.clone()).unwrap().char_indices() {
                    let attrs = state.doc.styles.resolve_char(&base_chars, story.char_format_of(range.start + offset));
                    chars.push(json!({"char":ch.to_string(),"size":attrs.size,"leading":attrs.leading,"direction":para.direction,
                        "bold":db.traits_of(&attrs.font_family,&attrs.font_style).0>=600.0}));
                }
            }
            result.push(json!({"text":story.text,"direction":story.direction,"chars":chars}));
        }
        result
    }

    #[test]
    fn bilingual_unicode_font_binding_preserves_every_character_size_leading_direction_and_bold_run() {
        let Some(family) = fonts() else { return };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","facingPages":false})).unwrap();
        session.execute("frame.create", &json!({"rect":[40,700,250,760],"content":"text","text":"Original user content","caret":false})).unwrap();
        let base = session.doc().unwrap().clone();
        let arabic = "نَبَاتٌ جَمِيلٌ ٣";
        let arabic_bold_end = arabic.find(' ').unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Bound fonts","images":[],"commands":[
            {"command":"frame.create","params":{"rect":[45,45,300,100],"content":"text","text":"Count three apples.","caret":false}},
            {"command":"type.char","params":{"attrs":{"fontFamily":family,"fontStyle":"Regular","size":18,"leading":{"kind":"points","value":24}}}},
            {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":5}},
            {"command":"type.char","params":{"attrs":{"fontStyle":"Bold"}}},
            {"command":"frame.create","params":{"rect":[45,130,400,205],"content":"text","text":arabic,"caret":false}},
            {"command":"type.para","params":{"attrs":{"direction":"rightToLeft","align":"right"}}},
            {"command":"type.char","params":{"attrs":{"fontFamily":family,"fontStyle":"Regular","size":20,"leading":{"kind":"points","value":27}}}},
            {"command":"text.select","params":{"story":"$4.story","anchor":0,"focus":arabic_bold_end}},
            {"command":"type.char","params":{"attrs":{"fontStyle":"Bold"}}},
            {"command":"frame.create","params":{"rect":[45,250,450,320],"content":"text","text":"نَبَاتٌ | Plant ٣","caret":false}},
            {"command":"type.char","params":{"attrs":{"fontFamily":family,"fontStyle":"Regular","size":17,"leading":{"kind":"points","value":23}}}}
        ]}))
        .unwrap();
        let old = plan.clone();
        let expected = formatting(execute(&old, &base, &session.prefs).doc().unwrap());
        let contract = PrintContract { english_fonts: vec![family.clone()], arabic_fonts: vec![family], ..Default::default() };
        let notes = apply(&mut plan, &base, &session.prefs, &contract).unwrap();
        let after = execute(&plan, &base, &session.prefs);
        assert_eq!(formatting(after.doc().unwrap()), expected);
        assert_eq!(serde_json::to_value(&plan.commands[..old.commands.len()]).unwrap(), serde_json::to_value(&old.commands).unwrap());
        assert!(notes.iter().any(|note| note["kind"] == "mixedScriptPrintFont"));
        assert!(
            plan.commands
                .iter()
                .skip(old.commands.len())
                .filter(|a| a.command == "type.char")
                .all(|a| a.params["attrs"].as_object().unwrap().len() == 2)
        );
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        assert!(
            after
                .doc()
                .unwrap()
                .doc
                .stories
                .values()
                .any(|s| s.paras.iter().any(|p| after.doc().unwrap().doc.styles.resolve_para(p).0.direction == TextDirection::RightToLeft))
        );
    }

    #[test]
    fn action_limit_and_missing_fonts_leave_the_entire_plan_unchanged() {
        let Some(family) = fonts() else { return };
        let mut session = Session::new();
        session.execute("file.new", &json!({})).unwrap();
        let mut commands = vec![EngineAction {
            command: "frame.create".into(),
            params: json!({"rect":[50,50,250,100],"content":"text","text":"نَبَاتٌ","caret":false}),
        }];
        commands.extend((1..511).map(|_| EngineAction { command: "selection.set".into(), params: json!({"ids":[]}) }));
        let mut plan = ProductionPlan { title: "Atomic fonts".into(), commands, images: Vec::new() };
        let before = serde_json::to_value(&plan).unwrap();
        let contract = PrintContract { arabic_fonts: vec![family], ..Default::default() };
        assert!(apply(&mut plan, session.doc().unwrap(), &session.prefs, &contract).unwrap_err().contains("512"));
        assert_eq!(serde_json::to_value(&plan).unwrap(), before);
        let missing = PrintContract { arabic_fonts: vec!["Missing Print Font 12345".into()], ..Default::default() };
        assert!(apply(&mut plan, session.doc().unwrap(), &session.prefs, &missing).is_err());
        assert_eq!(serde_json::to_value(&plan).unwrap(), before);
    }

    #[test]
    fn print_font_binding_recovers_unregistered_inline_swatches_before_scratch_apply() {
        let Some(family) = fonts() else { return };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","facingPages":false})).unwrap();
        let base = session.doc().unwrap().clone();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Print fonts with inline swatch",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"rect": [45, 45, 300, 100], "content": "text", "text": "Animals in the Garden", "caret": false}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": family, "fontStyle": "Regular", "size": 18, "fill": "C:85 M:75 Y:0 K:0"}}},
                {"command": "frame.create", "params": {"rect": [45, 120, 200, 180], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$2.id"], "swatch": "C=0 M=15 Y=95 K=0", "tint": 0.15}},
                {"command": "object.stroke", "params": {"ids": ["$2.id"], "swatch": "Paper", "weight": 1.0}}
            ]
        }))
        .unwrap();
        let contract = PrintContract { english_fonts: vec![family.clone()], arabic_fonts: vec![family], ..Default::default() };
        let notes = apply(&mut plan, &base, &session.prefs, &contract).unwrap();
        assert_eq!(plan.commands[1].params["attrs"]["fill"], "C=100 M=90 Y=10 K=0");
        assert_eq!(plan.commands[3].params["swatch"], "C=0 M=0 Y=100 K=0");
        assert_eq!(plan.commands[4].params["swatch"], "[Paper]");
        assert_eq!(notes.iter().filter(|n| n["kind"] == "nativeSwatchFallback").count(), 3);
    }

    #[test]
    fn arabic_honorifics_pass_print_font_binding_when_fallback_exists_while_missing_letters_fail() {
        let Some(family) = fonts() else { return };
        let db = designcraft_fonts::FontDb::global();
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","facingPages":false})).unwrap();
        let base = session.doc().unwrap().clone();
        let scoped = db.scoped(base.doc.font_scope);
        if !has_fallback_glyph(&scoped, '\u{fdfa}') || !has_fallback_glyph(&scoped, '\u{fdfd}') {
            return;
        }
        let copy = "قَالَ النَّبِيُّ ﷺ وبسم الله ﷽";
        let bold_end = copy.find(" وبسم").unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Islamic honorifics in Tahoma",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"rect": [50, 60, 520, 140], "content": "text", "text": copy, "caret": false}},
                {"command": "type.para", "params": {"attrs": {"direction": "rightToLeft", "align": "right"}}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": family, "fontStyle": "Regular", "size": 16, "leading": {"kind": "points", "value": 24}}}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": bold_end}},
                {"command": "type.char", "params": {"attrs": {"fontStyle": "Bold"}}}
            ]
        }))
        .unwrap();
        let contract = PrintContract { english_fonts: vec![family.clone()], arabic_fonts: vec![family.clone()], ..Default::default() };
        apply(&mut plan, &base, &session.prefs, &contract).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        crate::native_layout::prepare(&mut plan, &base, &session.prefs, &cancel).unwrap();
        let mut verified = Session::new();
        verified.prefs = session.prefs.clone();
        verified.add_document(base.clone());
        verified.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

        // An Arabic Extended-A letter (U+08B5) lacking coverage in Tahoma must still be rejected strictly if unprintable.
        if db.face(&family, "Regular").glyph_for('\u{08b5}') == 0 {
            let mut missing_letter_plan: ProductionPlan = serde_json::from_value(json!({
                "title": "Missing ordinary Arabic letter",
                "images": [],
                "commands": [
                    {"command": "frame.create", "params": {"rect": [50, 60, 420, 120], "content": "text", "text": "كلمة \u{08b5}", "caret": false}}
                ]
            }))
            .unwrap();
            let snapshot = serde_json::to_value(&missing_letter_plan).unwrap();
            assert!(apply(&mut missing_letter_plan, &base, &session.prefs, &contract).is_err());
            assert_eq!(serde_json::to_value(&missing_letter_plan).unwrap(), snapshot);
        }
    }
}
