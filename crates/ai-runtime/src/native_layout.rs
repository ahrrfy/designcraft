//! Local, measured layout recovery. It never purchases requests or edits the live document.
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::protocol::ProductionPlan;
use designcraft_engine::{
    DocState, Prefs, Session,
    doc::{ItemId, StoryId},
};
use designcraft_geom::{Affine, Rect};
use serde_json::{Value, json};
use std::collections::BTreeSet;
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
    let mut notes = Vec::new();
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
