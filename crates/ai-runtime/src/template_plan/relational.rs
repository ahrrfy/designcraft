//! Relational InDesign-parity authoring primitives for template-backed multi-page books:
//! - `A-Parent` MasterSpread with live `PAGE_NUMBER` (`\u{E000}`) + `RunningHeader` text variable
//! - Per-page `Section` markers (`layout.section` + `SECTION_MARKER` `\u{E001}`)
//! - Cover `[None]` master detachment (`layout.pages.applyParent`)
//! - Atomic 1:1 `LiveCaption` card bindings (`object.altText` + `object.caption`)

use designcraft_ai_types::EngineAction;
use designcraft_ai_types::design::{DesignProfile, PageFamily};
use designcraft_ai_types::protocol::AiSettings;
use designcraft_engine::{DocState, Session, doc};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) const UNIT_HEADING_STYLE: &str = "Unit Heading";
pub(crate) const UNIT_RUNNING_VAR: &str = "Unit Running";

pub(crate) fn profile_uses_relational_master(profile: &DesignProfile) -> bool {
    profile.page_families.iter().any(|family| family.text_slots.iter().any(|slot| slot.id == "title-main"))
}

pub(crate) fn has_relational_master_footer(base: &DocState) -> bool {
    let Some(parent) = base.doc.parents.first() else {
        return false;
    };
    let mut stack: Vec<(&doc::Item, usize)> = parent.items.iter().map(|it| (it.as_ref(), 0usize)).collect();
    while let Some((item, depth)) = stack.pop() {
        if item.label == "part.master.footer.running" {
            return true;
        }
        if depth < 8 {
            for child in item.children() {
                stack.push((child.as_ref(), depth + 1));
            }
        }
    }
    false
}

pub(super) fn base_has_unit_heading_style(base: &DocState) -> bool {
    base.doc.styles.para(UNIT_HEADING_STYLE).is_some()
}

pub(crate) fn initialize_relational_book_master(
    session: &mut Session,
    profile: Option<&DesignProfile>,
    settings: &AiSettings,
    brief: &str,
) -> Result<(), String> {
    let Some(prof) = profile.filter(|p| profile_uses_relational_master(p)) else {
        return Ok(());
    };
    if session.doc().map_err(|e| e.to_string())?.doc.parents.is_empty() {
        return Ok(());
    }
    let rtl = crate::runtime::book_binding(settings, brief);
    let facing = settings.facing_pages;

    for (swatch_name, c, m, y, k) in [("C=100 M=0 Y=0 K=0", 100, 0, 0, 0), ("C=100 M=90 Y=10 K=0", 100, 90, 10, 0)] {
        let has_swatch = session.doc().map_err(|e| e.to_string())?.doc.swatches.iter().any(|s| s.name == swatch_name);
        if !has_swatch {
            session.execute("swatch.create", &json!({"name": swatch_name, "color": {"c": c, "m": m, "y": y, "k": k}})).map_err(|e| e.to_string())?;
        }
    }

    let font_scope = session.doc().map_err(|e| e.to_string())?.doc.font_scope;
    let scoped_fonts = designcraft_fonts::FontDb::global().scoped(font_scope);
    let covers_bilingual_master = |m: &designcraft_fonts::ArabicFontMatch| {
        const REQUIRED_MASTER_CHARS: &str = "ابت12AB";
        REQUIRED_MASTER_CHARS.chars().all(|c| {
            scoped_fonts.face(&m.regular_family, &m.regular_style).glyph_for(c) != 0
                && scoped_fonts.face(&m.bold_family, &m.bold_style).glyph_for(c) != 0
        })
    };
    let rtl_bilingual_match = rtl
        .then(|| {
            settings
                .arabic_font
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|req| scoped_fonts.resolve_arabic_font(req))
                .filter(covers_bilingual_master)
                .or_else(|| scoped_fonts.preferred_arabic_font().filter(covers_bilingual_master))
                .or_else(|| {
                    scoped_fonts
                        .available_arabic_families()
                        .into_iter()
                        .find_map(|fam| scoped_fonts.resolve_arabic_font(&fam).filter(covers_bilingual_master))
                })
        })
        .flatten();
    let (regular_family, regular_style, bold_family, bold_style) = if let Some(ar) = rtl_bilingual_match {
        (ar.regular_family, ar.regular_style, ar.bold_family, ar.bold_style)
    } else if !scoped_fonts.styles("Tahoma").is_empty() {
        let styles = scoped_fonts.styles("Tahoma");
        let reg = if styles.iter().any(|s| s == "Regular") { "Regular".into() } else { styles.first().cloned().unwrap_or_else(|| "Regular".into()) };
        let bld = if styles.iter().any(|s| s == "Bold") { "Bold".into() } else { reg.clone() };
        ("Tahoma".into(), reg, "Tahoma".into(), bld)
    } else {
        let styles = scoped_fonts.styles("Source Sans 3");
        let reg = if styles.iter().any(|s| s == "Regular") { "Regular".into() } else { styles.first().cloned().unwrap_or_else(|| "Regular".into()) };
        let bld = if styles.iter().any(|s| s == "Bold") {
            "Bold".into()
        } else if styles.iter().any(|s| s == "Semibold") {
            "Semibold".into()
        } else {
            reg.clone()
        };
        ("Source Sans 3".into(), reg, "Source Sans 3".into(), bld)
    };

    if session.doc().map_err(|e| e.to_string())?.doc.styles.para(UNIT_HEADING_STYLE).is_none() {
        session
            .execute(
                "style.paragraph.create",
                &json!({
                    "name": UNIT_HEADING_STYLE,
                    "chars": {"fontFamily": bold_family, "fontStyle": bold_style, "size": 13.0}
                }),
            )
            .map_err(|e| e.to_string())?;
    }

    let defined = session
        .execute(
            "variables.define",
            &json!({
                "name": UNIT_RUNNING_VAR,
                "type": "runningHeader",
                "style": UNIT_HEADING_STYLE,
                "use": "firstOnPage"
            }),
        )
        .map_err(|e| e.to_string())?;
    let running_char = defined
        .get("char")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("RunningHeader variable did not allocate a marker character")?
        .to_owned();

    if facing {
        session
            .edit(|d, _| {
                if let Some(parent_arc) = d.parents.first().cloned()
                    && parent_arc.pages.len() == 1
                    && let Some(first_pg) = parent_arc.pages.first().cloned()
                {
                    let second_id = doc::PageId(d.alloc());
                    if let Some(parent_mut) = d.parents.first_mut() {
                        let sp = Arc::make_mut(parent_mut);
                        let mut left_pg = first_pg.clone();
                        left_pg.side = doc::PageSide::Left;
                        let mut right_pg = first_pg;
                        right_pg.id = second_id;
                        right_pg.side = doc::PageSide::Right;
                        sp.pages = if rtl { vec![right_pg, left_pg] } else { vec![left_pg, right_pg] };
                        sp.relayout();
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
    }

    let parent_pages: Vec<(f64, f64, f64, doc::PageSide)> = session
        .doc()
        .map_err(|e| e.to_string())?
        .doc
        .parents
        .first()
        .map(|sp| sp.pages.iter().map(|pg| (pg.x, pg.width, pg.height, pg.side)).collect())
        .unwrap_or_default();

    let margin = prof.margin_mm * 72.0 / 25.4;
    let parent_spread = json!({"kind": "parent", "index": 0});
    let lang = if rtl { "ar" } else { "en" };
    let dir = if rtl { "rightToLeft" } else { "leftToRight" };

    for (offset_x, page_w, page_h, side) in parent_pages {
        let outer_left = match side {
            doc::PageSide::Left => true,
            doc::PageSide::Right => false,
            doc::PageSide::Single => rtl,
        };
        let rule_y = (page_h - margin - 26.37).max(margin + 12.0);
        let text_y0 = rule_y + 3.0;
        let text_y1 = (rule_y + 22.0).min(page_h - margin);
        let bar_y0 = rule_y + 9.0;
        let bar_y1 = (rule_y + 12.5).min(page_h - margin);
        let rule_x0 = offset_x + margin + 3.48;
        let rule_x1 = offset_x + page_w - margin - 3.76;
        let inner_x0 = offset_x + margin + 5.48;
        let outer_x1 = offset_x + page_w - margin - 5.76;

        let (bar_x0, bar_x1, page_x0, page_x1, page_align) = if outer_left {
            (outer_x1 - 64.0, outer_x1, inner_x0, inner_x0 + 62.0, "left")
        } else {
            (inner_x0, inner_x0 + 64.0, outer_x1 - 62.0, outer_x1, "right")
        };
        let run_x0 = inner_x0 + 76.0;
        let run_x1 = (outer_x1 - 76.0).max(run_x0 + 48.0);

        let r_rule = session
            .execute("line.create", &json!({"spread": parent_spread, "a": [rule_x0, rule_y], "b": [rule_x1, rule_y]}))
            .map_err(|e| e.to_string())?;
        let rule_id = r_rule.get("id").and_then(Value::as_u64).ok_or("Master footer rule missing id")?;
        session
            .execute(
                "object.stroke",
                &json!({"ids": [rule_id], "swatch": "C=100 M=0 Y=0 K=0", "tint": 0.6, "weight": 0.8, "cap": "round", "join": "round"}),
            )
            .map_err(|e| e.to_string())?;
        session.execute("object.label", &json!({"ids": [rule_id], "label": "part.ui.footer.rule"})).map_err(|e| e.to_string())?;

        let mid_y = (bar_y0 + bar_y1) * 0.5;
        let rx = 1.75;
        let r_bar = session
            .execute(
                "path.create",
                &json!({
                    "spread": parent_spread,
                    "closed": true,
                    "anchors": [
                        {"p": [bar_x0 + rx, bar_y0], "in": [bar_x0 + rx * 0.45, bar_y0]},
                        {"p": [bar_x1 - rx, bar_y0], "out": [bar_x1 - rx * 0.45, bar_y0]},
                        {"p": [bar_x1, mid_y], "in": [bar_x1, bar_y0 + rx * 0.45], "out": [bar_x1, bar_y1 - rx * 0.45]},
                        {"p": [bar_x1 - rx, bar_y1], "in": [bar_x1 - rx * 0.45, bar_y1]},
                        {"p": [bar_x0 + rx, bar_y1], "out": [bar_x0 + rx * 0.45, bar_y1]},
                        {"p": [bar_x0, mid_y], "in": [bar_x0, bar_y1 - rx * 0.45], "out": [bar_x0, bar_y0 + rx * 0.45]}
                    ]
                }),
            )
            .map_err(|e| e.to_string())?;
        let bar_id = r_bar.get("id").and_then(Value::as_u64).ok_or("Master footer bar missing id")?;
        session.execute("object.fill", &json!({"ids": [bar_id], "swatch": "C=100 M=0 Y=0 K=0", "tint": 0.75})).map_err(|e| e.to_string())?;
        session.execute("object.stroke", &json!({"ids": [bar_id], "swatch": "[None]", "weight": 0.0})).map_err(|e| e.to_string())?;
        session.execute("object.label", &json!({"ids": [bar_id], "label": "part.ui.footer.bar"})).map_err(|e| e.to_string())?;

        let r_run = session
            .execute(
                "frame.create",
                &json!({
                    "spread": parent_spread,
                    "rect": [run_x0, text_y0, run_x1, text_y1],
                    "content": "text",
                    "text": running_char,
                    "caret": false
                }),
            )
            .map_err(|e| e.to_string())?;
        let run_id = r_run.get("id").and_then(Value::as_u64).ok_or("Master running header missing id")?;
        session
            .execute(
                "type.char",
                &json!({
                    "attrs": {
                        "fontFamily": regular_family,
                        "fontStyle": regular_style,
                        "size": 8.5,
                        "leading": {"kind": "points", "value": 11.0},
                        "fill": "C=100 M=90 Y=10 K=0",
                        "language": lang
                    }
                }),
            )
            .map_err(|e| e.to_string())?;
        session
            .execute(
                "type.para",
                &json!({
                    "attrs": {
                        "direction": dir,
                        "align": "center",
                        "hyphenate": false,
                        "spaceBefore": 0.0,
                        "spaceAfter": 0.0
                    }
                }),
            )
            .map_err(|e| e.to_string())?;
        session.execute("object.textFrameOptions", &json!({"ids": [run_id], "verticalJustification": "center"})).map_err(|e| e.to_string())?;
        session.execute("object.label", &json!({"ids": [run_id], "label": "part.master.footer.running"})).map_err(|e| e.to_string())?;

        let r_page = session
            .execute(
                "frame.create",
                &json!({
                    "spread": parent_spread,
                    "rect": [page_x0, text_y0, page_x1, text_y1],
                    "content": "text",
                    "text": "\u{E000}",
                    "caret": false
                }),
            )
            .map_err(|e| e.to_string())?;
        let page_id = r_page.get("id").and_then(Value::as_u64).ok_or("Master page folio missing id")?;
        session
            .execute(
                "type.char",
                &json!({
                    "attrs": {
                        "fontFamily": bold_family,
                        "fontStyle": bold_style,
                        "size": 10.0,
                        "leading": {"kind": "points", "value": 13.0},
                        "fill": "C=100 M=90 Y=10 K=0",
                        "language": lang
                    }
                }),
            )
            .map_err(|e| e.to_string())?;
        session
            .execute(
                "type.para",
                &json!({
                    "attrs": {
                        "direction": dir,
                        "align": page_align,
                        "hyphenate": false,
                        "spaceBefore": 0.0,
                        "spaceAfter": 0.0
                    }
                }),
            )
            .map_err(|e| e.to_string())?;
        session.execute("object.textFrameOptions", &json!({"ids": [page_id], "verticalJustification": "center"})).map_err(|e| e.to_string())?;
        session.execute("object.label", &json!({"ids": [page_id], "label": "part.ui.footer.page"})).map_err(|e| e.to_string())?;

        let r_grp = session.execute("object.group", &json!({"ids": [rule_id, bar_id, run_id, page_id]})).map_err(|e| e.to_string())?;
        let grp_id = r_grp.get("id").and_then(Value::as_u64).ok_or("Master footer group missing id")?;
        session.execute("object.label", &json!({"ids": [grp_id], "label": "group.ui.page-footer"})).map_err(|e| e.to_string())?;
    }

    if let Ok(state) = session.doc_mut() {
        state.selection = Default::default();
    }
    Ok(())
}

fn parse_local_step_ref(token: &str, current: usize) -> Option<(usize, Option<&str>)> {
    let body = token.strip_prefix("${").and_then(|s| s.strip_suffix('}')).or_else(|| token.strip_prefix('$'))?;
    let looks_like_ref = body == "last" || body.starts_with("last.") || body.bytes().next().is_some_and(|b| b.is_ascii_digit());
    if !looks_like_ref {
        return None;
    }
    let (step_tok, rest) = match body.split_once('.') {
        Some((s, p)) => (s, Some(p)),
        None => (body, None),
    };
    let step = if step_tok == "last" { current.checked_sub(1)? } else { step_tok.parse::<usize>().ok()? };
    (step < current).then_some((step, rest))
}

fn remap_action_value(value: &mut Value, old_index: usize, index_map: &[Option<usize>]) -> Result<(), String> {
    match value {
        Value::String(text) => {
            if let Some((target_step, path_opt)) = parse_local_step_ref(text, old_index) {
                let mapped = index_map.get(target_step).copied().flatten().ok_or("Template action references an omitted footer object")?;
                *text = match path_opt {
                    Some(path) => format!("${mapped}.{path}"),
                    None => format!("${mapped}"),
                };
            }
        }
        Value::Array(items) => {
            items.retain(|item| {
                item.as_str()
                    .and_then(|s| parse_local_step_ref(s, old_index))
                    .is_none_or(|(step, _)| index_map.get(step).copied().flatten().is_some())
            });
            for item in items {
                remap_action_value(item, old_index, index_map)?;
            }
        }
        Value::Object(map) => {
            for val in map.values_mut() {
                remap_action_value(val, old_index, index_map)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// When the target document already carries the relational `A-Parent` master footer,
/// strip the per-page `group.ui.page-footer` sequence cleanly and remap step indices.
pub(super) fn prepare_template_for_relational_master(
    template: &[EngineAction],
    family: &PageFamily,
    base: Option<&DocState>,
) -> Result<(Option<Vec<EngineAction>>, BTreeMap<String, usize>), String> {
    let default_slots: BTreeMap<String, usize> = family.text_slots.iter().map(|s| (s.id.clone(), s.command)).collect();
    if !base.is_some_and(has_relational_master_footer) {
        return Ok((None, default_slots));
    }
    let Some(footer_lbl_idx) = template
        .iter()
        .position(|act| act.command == "object.label" && act.params.get("label").and_then(Value::as_str) == Some("group.ui.page-footer"))
    else {
        return Ok((None, default_slots));
    };
    let Some(group_idx) = footer_lbl_idx.checked_sub(1) else {
        return Ok((None, default_slots));
    };
    let Some(group_act) = template.get(group_idx).filter(|act| act.command == "object.group") else {
        return Ok((None, default_slots));
    };
    let Some(member_ids) = group_act.params.get("ids").and_then(Value::as_array) else {
        return Ok((None, default_slots));
    };
    let mut min_member = group_idx;
    for member in member_ids {
        if let Some((step, _)) = member.as_str().and_then(|s| parse_local_step_ref(s, group_idx)) {
            min_member = min_member.min(step);
        }
    }
    let omitted: BTreeSet<usize> = (min_member..=footer_lbl_idx).collect();
    let mut index_map = vec![None; template.len()];
    let mut filtered = Vec::with_capacity(template.len().saturating_sub(omitted.len()));
    let mut kept_origins = Vec::with_capacity(template.len().saturating_sub(omitted.len()));
    for (old_idx, act) in template.iter().enumerate() {
        if omitted.contains(&old_idx) {
            continue;
        }
        let new_idx = filtered.len();
        if let Some(slot) = index_map.get_mut(old_idx) {
            *slot = Some(new_idx);
        }
        filtered.push(act.clone());
        kept_origins.push(old_idx);
    }
    for (act, old_idx) in filtered.iter_mut().zip(kept_origins) {
        remap_action_value(&mut act.params, old_idx, &index_map)?;
    }
    let mut mapped_slots = BTreeMap::new();
    for slot in &family.text_slots {
        let mapped_cmd = index_map.get(slot.command).copied().flatten().ok_or("Text slot targets an omitted footer command")?;
        mapped_slots.insert(slot.id.clone(), mapped_cmd);
    }
    Ok((Some(filtered), mapped_slots))
}

/// Resolves the authored text-slot string backing an `object.altText` + `object.caption` (`captionFrame`) pair.
pub(super) fn linked_caption_slot_text<'a>(
    template: &[EngineAction],
    alt_index: usize,
    slot_cmds: &BTreeMap<String, usize>,
    page_text: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    let next_idx = alt_index.checked_add(1)?;
    let next_act = template.get(next_idx)?;
    if next_act.command != "object.caption" {
        return None;
    }
    let cf_ref = next_act.params.get("captionFrame")?.as_str()?;
    let (cf_step, _) = parse_local_step_ref(cf_ref, next_idx)?;
    let (slot_id, _) = slot_cmds.iter().find(|(_, cmd_idx)| **cmd_idx == cf_step)?;
    page_text.get(slot_id).map(String::as_str)
}
