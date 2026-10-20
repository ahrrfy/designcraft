//! Local, measured layout recovery. It never purchases requests or edits the live document.
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::protocol::ProductionPlan;
use designcraft_engine::{
    DocState, Prefs, Session,
    doc::{ItemId, StoryId},
};
use designcraft_geom::{Affine, Rect};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn prepare(plan: &mut ProductionPlan, base: &DocState, prefs: &Prefs, cancel: &AtomicBool) -> Result<Vec<Value>, String> {
    let mut candidate = plan.clone();
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    let result = scratch
        .execute("production.apply", &json!({"commands":candidate.commands,"outputMode":"native_with_images","requirePreflight":false}))
        .map_err(|e| e.to_string())?;
    let results = result.get("results").and_then(Value::as_array).ok_or("Native layout did not return command results")?;
    let mut frames = Vec::new();
    for (index, action) in candidate.commands.iter().enumerate() {
        if action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text") {
            let id = results.get(index).and_then(|v| v.get("id")).and_then(Value::as_u64).ok_or("Native text frame has no ID")?;
            let story = results.get(index).and_then(|v| v.get("story")).and_then(Value::as_u64).ok_or("Native text frame has no story")?;
            frames.push((index, ItemId(id), StoryId(story)));
        }
    }
    let mut notes = recover_text_positions(&mut candidate, &mut scratch, base, &frames, cancel)?;
    let mut seen = BTreeSet::new();
    for (index, id, story_id) in frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if !seen.insert(story_id) {
            continue;
        }
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
        let db = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
        let mut fallbacks = Vec::new();
        for (pi, range) in story.para_ranges().into_iter().enumerate() {
            let para = story.paras.get(pi).ok_or("Native paragraph formatting disappeared")?;
            let (_, base_chars) = doc.styles.resolve_para(para);
            let text = story.text.get(range.clone()).ok_or("Invalid native paragraph boundary")?;
            for (offset, ch) in text.char_indices() {
                // Only decorative symbols get a fallback. Ordinary copy and Arabic letterforms
                // retain the selected family; a missing requested font remains an explicit error.
                if !('\u{2000}'..='\u{2bff}').contains(&ch) || ch.is_whitespace() {
                    continue;
                }
                let byte = range.start.saturating_add(offset);
                let attrs = doc.styles.resolve_char(&base_chars, story.char_format_of(byte));
                if db.face(&attrs.font_family, &attrs.font_style).covers(ch) {
                    continue;
                }
                let face = ["Segoe UI Symbol", "Noto Sans Symbols 2", "DejaVu Sans", "Arial"]
                    .into_iter()
                    .filter(|family| db.has_family(family))
                    .map(|family| db.face(family, "Regular"))
                    .find(|face| face.covers(ch))
                    .ok_or_else(|| format!("Native text frame {} needs an installed font covering symbol {ch}", id.0))?;
                fallbacks.push((byte, byte.saturating_add(ch.len_utf8()), ch, face.family.clone(), face.style.clone()));
                if fallbacks.len() > 128 {
                    return Err("Too many symbol fallback ranges in one native story".into());
                }
            }
        }
        for (start, end, ch, family, style) in fallbacks {
            let actions = [
                EngineAction { command: "text.select".into(), params: json!({"story":format!("${index}.story"),"anchor":start,"focus":end}) },
                EngineAction { command: "type.char".into(), params: json!({"attrs":{"fontFamily":family,"fontStyle":style}}) },
            ];
            scratch.execute("text.select", &json!({"story":story_id.0,"anchor":start,"focus":end})).map_err(|e| e.to_string())?;
            scratch.execute("type.char", &actions[1].params).map_err(|e| e.to_string())?;
            candidate.commands.extend(actions);
            notes.push(json!({"kind":"symbolFontFallback","item":id.0,"symbol":ch.to_string(),"fontFamily":family,"message":"Preserved the character using an installed symbol font"}));
        }
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        if !scratch.cache.get(&doc, story_id, None).is_overset() {
            continue;
        }
        let loc = doc.find(id).ok_or("Native text frame location disappeared")?;
        let designcraft_engine::doc::SpreadRef::Doc(spread_index) = loc.spread else {
            return Err("Cannot fit a parent-page frame in production".into());
        };
        let spread = doc.spreads.get(spread_index).ok_or("Native page disappeared")?;
        let item = doc.item(id).ok_or("Native frame disappeared")?;
        let outer = doc.parent_xf(&loc);
        let linear = (outer * item.xf).as_coeffs();
        if (linear[0] - 1.0).abs() > 0.001 || linear[1].abs() > 0.001 || linear[2].abs() > 0.001 || (linear[3] - 1.0).abs() > 0.001 {
            return Err(format!(
                "Page {} frame '{}' has overset text in a transformed frame; reflow its layout before generation",
                spread_index + 1,
                item.label
            ));
        }
        let old = outer.transform_rect_bbox(item.bounds());
        scratch.execute("object.fit", &json!({"ids":[id.0],"mode":"fitFrameToContent","axis":"height"})).map_err(|e| e.to_string())?;
        let fitted_doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let fitted = fitted_doc.item(id).ok_or("Fitted text frame disappeared")?;
        let grown = outer.transform_rect_bbox(fitted.bounds());
        let safe = spread.pages.iter().any(|page| contains(page.margin_rect(), grown));
        let mut pending: Vec<_> = spread.items.iter().map(|node| (node.as_ref(), Affine::IDENTITY, 0u32)).collect();
        // Parent artwork is rendered on the document page too. Match the engine's
        // placement/override rules before declaring a growth area to be empty.
        for (page_index, page) in spread.pages.iter().enumerate().filter(|(_, page)| page.show_parent_items) {
            for (parent_index, parent_page_index) in doc.shown_parent_pages(designcraft_engine::doc::SpreadRef::Doc(spread_index), page_index) {
                let Some(parent) = doc.parents.get(parent_index) else { continue };
                let Some(parent_page) = parent.pages.get(parent_page_index) else { continue };
                for node in &parent.items {
                    if page.overridden.contains(&node.id)
                        || (parent.pages.len() > 1 && parent.page_at_x(node.bounds().center().x) != Some(parent_page_index))
                    {
                        continue;
                    }
                    if pending.len() >= 10_000 {
                        return Err("Native fitting parent-item bound exceeded".into());
                    }
                    pending.push((node.as_ref(), Affine::translate((page.x - parent_page.x, 0.0)), 0u32));
                }
            }
        }
        let mut visited = 0usize;
        let mut collision = false;
        while let Some((node, parent, depth)) = pending.pop() {
            visited = visited.saturating_add(1);
            if visited > 10_000 || depth > 64 {
                return Err("Native fitting item-tree bound exceeded".into());
            }
            if !node.children().is_empty() {
                for child in node.children() {
                    pending.push((child.as_ref(), parent * node.xf, depth.saturating_add(1)));
                }
                continue;
            }
            if node.id == id {
                continue;
            }
            let bounds = parent.transform_rect_bbox(node.visible_bounds());
            if contains(bounds, old) {
                if !contains(bounds, grown) {
                    collision = true;
                    break;
                }
            } else if overlaps(grown, bounds) && !overlaps(old, bounds) {
                collision = true;
                break;
            }
        }
        for image in &candidate.images {
            if image.page == spread_index {
                let [x0, y0, x1, y1] = image.rect;
                let bounds = Rect::new(x0, y0, x1, y1);
                if overlaps(grown, bounds) && !overlaps(old, bounds) {
                    collision = true;
                }
            }
        }
        if !safe || collision || scratch.cache.get(&fitted_doc, story_id, None).is_overset() {
            return Err(format!(
                "Page {} frame '{}' has overset text and needs {:.1} pt more height, but no safe free space remains; copy and font size were preserved. Reflow this activity before buying images",
                spread_index + 1,
                item.label,
                (grown.height() - old.height()).max(0.0)
            ));
        }
        let rect = candidate
            .commands
            .get_mut(index)
            .and_then(|a| a.params.get_mut("rect"))
            .and_then(Value::as_array_mut)
            .ok_or("Native text frame has no source rectangle")?;
        let bottom = rect.get(3).and_then(Value::as_f64).ok_or("Invalid native frame height")?;
        let value = rect.get_mut(3).ok_or("Invalid native frame rectangle")?;
        *value = json!(bottom + grown.height() - old.height() + 0.01);
        notes.push(json!({"kind":"nativeTextReflow","page":spread_index,"item":id.0,"label":item.label,"extraHeightPt":grown.height()-old.height()+0.01,"message":"Expanded text frame into verified free space; preserved all text and font size"}));
    }
    if candidate.commands.len() > 512 {
        return Err("Local fitting exceeds the bounded native action limit".into());
    }
    // Recompose from the original source with the exact recoveries that will be published.
    let mut verified = Session::new();
    verified.prefs = prefs.clone();
    verified.add_document(base.clone());
    verified
        .execute("production.apply", &json!({"commands":candidate.commands,"outputMode":"native_with_images","requirePreflight":true}))
        .map_err(|e| e.to_string())?;
    *plan = candidate;
    Ok(notes)
}

#[derive(Clone)]
struct TextPosition {
    index: usize,
    id: ItemId,
    spread: usize,
    page: usize,
    original: Rect,
    target: Rect,
    safe: Rect,
    language: u8,
}

/// Only translate new, ungrouped, axis-aligned frames. Authored commands and their
/// indices stay intact; the exact private recovery is appended as explicit moves.
fn recover_text_positions(
    plan: &mut ProductionPlan,
    scratch: &mut Session,
    base: &DocState,
    frames: &[(usize, ItemId, StoryId)],
    cancel: &AtomicBool,
) -> Result<Vec<Value>, String> {
    let doc = scratch.doc().map_err(|error| error.to_string())?.doc.clone();
    let mut positions = Vec::new();
    for &(index, id, story) in frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if base.doc.item(id).is_some() {
            continue;
        }
        let loc = doc.find(id).ok_or("Native text frame location disappeared")?;
        let designcraft_engine::doc::SpreadRef::Doc(spread_index) = loc.spread else { continue };
        let item = doc.item(id).ok_or("Native text frame disappeared")?;
        let text = &doc.story(story).ok_or("Native text story disappeared")?.text;
        if text.trim().is_empty() {
            continue;
        }
        let linear = (doc.parent_xf(&loc) * item.xf).as_coeffs();
        if loc.path.len() != 1
            || item.locked
            || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
            || (linear[0] - 1.0).abs() > 0.001
            || linear[1].abs() > 0.001
            || linear[2].abs() > 0.001
            || (linear[3] - 1.0).abs() > 0.001
        {
            continue;
        }
        let spread = doc.spreads.get(spread_index).ok_or("Native text page disappeared")?;
        let original = item.bounds();
        let page = spread.page_at_x(original.center().x).ok_or("Native text frame has no owning page")?;
        let safe = spread.pages.get(page).ok_or("Native text page disappeared")?.margin_rect();
        if ![original.x0, original.y0, original.x1, original.y1, safe.x0, safe.y0, safe.x1, safe.y1].iter().all(|v| v.is_finite()) {
            return Err("Native text geometry must be finite".into());
        }
        if original.width() > safe.width() + 0.01 || original.height() > safe.height() + 0.01 {
            return Err("Native text frame cannot fit its exact safe margins without resizing; reflow the layout".into());
        }
        let dx = if original.x0 < safe.x0 {
            safe.x0 - original.x0
        } else if original.x1 > safe.x1 {
            safe.x1 - original.x1
        } else {
            0.0
        };
        let dy = if original.y0 < safe.y0 {
            safe.y0 - original.y0
        } else if original.y1 > safe.y1 {
            safe.y1 - original.y1
        } else {
            0.0
        };
        let arabic = text.chars().any(|c| c.is_alphabetic() && ('\u{0600}'..='\u{08ff}').contains(&c));
        let latin = text.chars().any(|c| c.is_alphabetic() && c <= '\u{024f}');
        positions.push(TextPosition {
            index,
            id,
            spread: spread_index,
            page,
            original,
            target: shift(original, dx, dy),
            safe,
            language: match (latin, arabic) {
                (true, false) => 1,
                (false, true) => 2,
                _ => 0,
            },
        });
    }
    // A crossing can be recovered only when two aligned, single-language header
    // columns are unambiguous. Body activities and mixed-language frames are never guessed.
    for _ in 0..positions.len() {
        let mut crossing = None;
        'pairs: for (left, a) in positions.iter().enumerate() {
            for (right, b) in positions.iter().enumerate().skip(left.saturating_add(1)) {
                if a.spread == b.spread && a.page == b.page && overlaps(a.target, b.target) {
                    crossing = Some((left, right));
                    break 'pairs;
                }
            }
        }
        let Some((a, b)) = crossing else { break };
        let first = positions.get(a).ok_or("Missing header position")?.clone();
        let second = positions.get(b).ok_or("Missing header position")?.clone();
        if first.language == 0
            || second.language == 0
            || first.language == second.language
            || first.target.y1 > first.safe.y0 + first.safe.height() * 0.25
            || second.target.y1 > second.safe.y0 + second.safe.height() * 0.25
        {
            return Err("Overlapping native text does not form two unambiguous bilingual header columns; reflow the layout".into());
        }
        let (left, right) = if first.target.center().x < second.target.center().x { (first, second) } else { (second, first) };
        let available = left.safe.width() - left.target.width() - right.target.width();
        if !available.is_finite() || available < 1.0 {
            return Err("Bilingual header columns cannot fit exact safe margins without resizing".into());
        }
        let gap = available.min(8.0);
        let width = left.target.width() + right.target.width() + gap;
        let ideal = (left.target.x0 + right.target.x0 - left.target.width() - gap) / 2.0;
        let left_x = ideal.max(left.safe.x0).min(left.safe.x1 - width);
        let right_x = left_x + left.target.width() + gap;
        let same_column = |position: &TextPosition, column: &TextPosition| {
            position.spread == column.spread
                && position.page == column.page
                && position.language == column.language
                && (position.target.x0 - column.target.x0).abs() < 0.01
                && (position.target.x1 - column.target.x1).abs() < 0.01
                && position.target.y1 <= position.safe.y0 + position.safe.height() * 0.25
        };
        if positions.iter().any(|position| {
            position.spread == left.spread
                && position.page == left.page
                && !same_column(position, &left)
                && !same_column(position, &right)
                && (overlaps(position.target, left.target) || overlaps(position.target, right.target))
        }) {
            return Err("Overlapping native text has an ambiguous third header column; reflow the layout".into());
        }
        let mut changed = false;
        for position in &mut positions {
            let dx = if same_column(position, &left) {
                left_x - left.target.x0
            } else if same_column(position, &right) {
                right_x - right.target.x0
            } else {
                continue;
            };
            position.target = shift(position.target, dx, 0.0);
            changed = true;
        }
        if !changed {
            return Err("Bilingual header recovery made no progress".into());
        }
    }
    let targets: BTreeMap<_, _> = positions.iter().map(|position| (position.id, position.target)).collect();
    let moved: Vec<_> = positions.iter().filter(|position| position.target != position.original).collect();
    if plan.commands.len().checked_add(moved.len()).is_none_or(|length| length > 512) {
        return Err("Native text translation exceeds the bounded native action limit".into());
    }
    for position in &moved {
        if !contains(position.safe, position.target) || position_collision(position, &targets, &doc, plan, cancel)? {
            return Err("Native text translation has no safe free space; copy, font sizes and existing artwork were preserved".into());
        }
    }
    let mut notes = Vec::new();
    for position in moved {
        let dx = position.target.x0 - position.original.x0;
        let dy = position.target.y0 - position.original.y0;
        scratch.execute("transform.move", &json!({"ids":[position.id.0],"dx":dx,"dy":dy})).map_err(|error| error.to_string())?;
        plan.commands
            .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[format!("${}.id",position.index)],"dx":dx,"dy":dy}) });
        notes.push(json!({"kind":"nativeTextTranslation","page":position.spread,"item":position.id.0,"dx":dx,"dy":dy,
            "message":"Translated new text into verified safe space; preserved copy, fonts, frame dimensions and authored command indices"}));
    }
    Ok(notes)
}

fn position_collision(
    position: &TextPosition,
    targets: &BTreeMap<ItemId, Rect>,
    doc: &designcraft_engine::doc::Document,
    plan: &ProductionPlan,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let spread = doc.spreads.get(position.spread).ok_or("Native text page disappeared")?;
    if spread.items.len() > 10_000 {
        return Err("Native translation item-tree bound exceeded".into());
    }
    let mut pending: Vec<_> = spread.items.iter().map(|node| (node.as_ref(), Affine::IDENTITY, 0u32, false)).collect();
    for (page_index, page) in spread.pages.iter().enumerate().filter(|(_, page)| page.show_parent_items) {
        for (parent_index, parent_page_index) in doc.shown_parent_pages(designcraft_engine::doc::SpreadRef::Doc(position.spread), page_index) {
            let Some(parent) = doc.parents.get(parent_index) else { continue };
            let Some(parent_page) = parent.pages.get(parent_page_index) else { continue };
            for node in &parent.items {
                if page.overridden.contains(&node.id)
                    || (parent.pages.len() > 1 && parent.page_at_x(node.bounds().center().x) != Some(parent_page_index))
                {
                    continue;
                }
                if pending.len() >= 10_000 {
                    return Err("Native translation parent-item bound exceeded".into());
                }
                pending.push((node.as_ref(), Affine::translate((page.x - parent_page.x, 0.0)), 0, true));
            }
        }
    }
    let mut visited = 0usize;
    while let Some((node, parent, depth, inherited)) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 10_000 || depth > 64 || pending.len() > 10_000 {
            return Err("Native translation item-tree bound exceeded".into());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if !node.children().is_empty() {
            for child in node.children() {
                if pending.len() >= 10_000 {
                    return Err("Native translation item-tree bound exceeded".into());
                }
                pending.push((child.as_ref(), parent * node.xf, depth.saturating_add(1), inherited));
            }
            continue;
        }
        if !inherited && node.id == position.id {
            continue;
        }
        let obstacle = if inherited {
            parent.transform_rect_bbox(node.visible_bounds())
        } else {
            targets.get(&node.id).copied().unwrap_or_else(|| parent.transform_rect_bbox(node.visible_bounds()))
        };
        if blocks_translation(obstacle, position.original, position.target) {
            return Ok(true);
        }
    }
    for image in plan.images.iter().filter(|image| image.page == position.spread) {
        let [x0, y0, x1, y1] = image.rect;
        if blocks_translation(Rect::new(x0, y0, x1, y1), position.original, position.target) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn blocks_translation(obstacle: Rect, old: Rect, new: Rect) -> bool {
    // A containing background remains a background only while it still contains
    // the complete translated frame. Any other new intersection is a blocker.
    if contains(obstacle, old) { !contains(obstacle, new) } else { overlaps(obstacle, new) }
}

fn shift(rect: Rect, dx: f64, dy: f64) -> Rect {
    Rect::new(rect.x0 + dx, rect.y0 + dy, rect.x1 + dx, rect.y1 + dy)
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 - 0.01 && a.x1 > b.x0 + 0.01 && a.y0 < b.y1 - 0.01 && a.y1 > b.y0 + 0.01
}
fn contains(a: Rect, b: Rect) -> bool {
    a.x0 <= b.x0 + 0.01 && a.y0 <= b.y0 + 0.01 && a.x1 >= b.x1 - 0.01 && a.y1 >= b.y1 - 0.01
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn bilingual_header_columns_move_coherently_without_resizing_or_retyping() {
        let fonts = designcraft_fonts::FontDb::global();
        let Some(family) = ["Arial", "DejaVu Sans", "Tahoma", "Cairo"]
            .into_iter()
            .find(|family| fonts.has_family(family) && fonts.face(family, "Regular").covers('ب'))
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
}
